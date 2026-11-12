use crate::{
    profiles::{Download, Profile},
    progress::{self, Phase, Progress, Step},
};
use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use fs2::FileExt;
use reqwest::{StatusCode, blocking::Client, header};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Condvar, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

/// Files larger than two chunks download over several ranged connections, because one connection
/// to the Hub carries only part of a fast link. Chunks are appended in order, so a partial file
/// always holds a prefix and stays resumable.
const CHUNK: u64 = 16 << 20;
const CONNECTIONS: usize = 8;

#[derive(Debug, Clone)]
pub struct Assets {
    pub profile: &'static Profile,
    pub server: PathBuf,
    pub model: PathBuf,
    pub projector: PathBuf,
}

pub fn prepare(profile: &'static Profile, cache_dir: &Path, offline: bool) -> Result<Assets> {
    let runtime = profile.runtime()?;
    let _lock = preparation_lock(cache_dir)?;

    let context = || {
        format!(
            "Cannot prepare {}. Run `{} pull {}` while online.",
            profile.id,
            crate::update::program(),
            profile.id
        )
    };
    let model_dir = cache_dir.join(profile.model_directory);
    let downloads = cache_dir.join("downloads");
    // The model's license is cached alongside its weights before anything is runnable.
    let files: Vec<_> = [(&runtime.archive, downloads.as_path())]
        .into_iter()
        .chain(
            profile
                .model_assets()
                .map(|spec| (spec, model_dir.as_path())),
        )
        .collect();
    // Download everything first, then verify everything, so startup reads as two clear steps.
    progress::step(Step::Download);
    for (spec, directory) in &files {
        prefetch(spec, directory, offline).with_context(context)?;
    }
    progress::step(Step::Verify);
    let mut archive = PathBuf::new();
    for (spec, directory) in &files {
        let path = fetch(spec, directory, offline).with_context(context)?;
        if std::ptr::eq(*spec, &runtime.archive) {
            archive = path;
        }
    }
    progress::step(Step::Start);
    let installed = cache_dir.join(runtime.directory);
    install_runtime(&archive, &installed, false, |dir| {
        runtime_valid(&archive, dir, runtime.server)
    })?;
    Ok(Assets {
        profile,
        server: installed.join(runtime.server),
        model: model_dir.join(profile.model.name),
        projector: model_dir.join(profile.projector.name),
    })
}

/// The checklist's download chips: the model's files and the speech engine.
pub fn parts(profile: &'static Profile, cache_dir: &Path) -> Result<Vec<progress::Part>> {
    let runtime = profile.runtime()?;
    let model_dir = cache_dir.join(profile.model_directory);
    Ok(vec![
        progress::Part {
            name: profile.id.into(),
            files: profile
                .model_assets()
                .map(|spec| (model_dir.join(spec.name), spec.size))
                .collect(),
        },
        progress::Part {
            name: "Speech engine".into(),
            files: vec![(
                cache_dir.join("downloads").join(runtime.archive.name),
                runtime.archive.size,
            )],
        },
    ])
}

pub(crate) fn preparation_lock(cache_dir: &Path) -> Result<File> {
    fs::create_dir_all(cache_dir).context("Cannot create the Boubou cache")?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(cache_dir.join("install.lock"))?;
    FileExt::try_lock_exclusive(&lock)
        .context("Another Boubou process is preparing models; wait for it to finish")?;
    Ok(lock)
}

/// Unpacks `archive` into `installed` unless `valid` accepts the installed tree already.
pub(crate) fn install_runtime(
    archive: &Path,
    installed: &Path,
    refresh: bool,
    valid: impl Fn(&Path) -> Result<bool>,
) -> Result<()> {
    if refresh || !valid(installed)? {
        let staging = installed.with_extension("partial");
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;
        {
            let _progress = Progress::new("Installing the local speech engine", None);
            if archive
                .extension()
                .is_some_and(|extension| extension == "zip")
            {
                zip::ZipArchive::new(File::open(archive)?)?
                    .extract(&staging)
                    .context("Cannot unpack the speech engine")?;
            } else {
                tar::Archive::new(GzDecoder::new(File::open(archive)?))
                    .unpack(&staging)
                    .context("Cannot unpack the speech engine")?;
            }
        }
        ensure!(
            valid(&staging)?,
            "The unpacked speech engine failed verification"
        );
        if installed.exists() {
            fs::remove_dir_all(installed)?;
        }
        fs::rename(&staging, installed)?;
        sync_parent(installed)?;
    }
    Ok(())
}

pub fn status(profile: &Profile, cache_dir: &Path) -> Result<()> {
    let model_dir = cache_dir.join(profile.model_directory);
    let mut ready = true;
    for spec in profile.model_assets() {
        let valid = verified(&model_dir.join(spec.name), spec)?;
        ready &= valid;
        println!(
            "{}: {}",
            spec.name,
            if valid {
                "verified"
            } else {
                "missing or incomplete"
            }
        );
    }
    let runtime = profile.runtime()?;
    let archive = cache_dir.join("downloads").join(runtime.archive.name);
    let runtime_ready = verified(&archive, &runtime.archive)?
        && runtime_valid(&archive, &cache_dir.join(runtime.directory), runtime.server)?;
    println!(
        "Speech engine ({}): {}",
        runtime.label,
        if runtime_ready {
            "verified"
        } else {
            "missing or incomplete"
        }
    );
    ensure!(
        ready && runtime_ready,
        "Liquid assets are missing or invalid. Run `{} pull {}` while online.",
        crate::update::program(),
        profile.id
    );
    Ok(())
}

pub(crate) fn sha256(mut reader: impl Read, progress: Option<&Progress>) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        if let Some(progress) = progress {
            progress.advance(n as u64);
        }
    }
    Ok(hex(&hash.finalize()))
}

/// SHA-256 of each file, in order, hashed on several threads with one progress line.
pub(crate) fn sha256_files(files: &[(&Path, u64)], label: &str) -> Result<Vec<String>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let progress = Progress::new(label, Some(files.iter().map(|(_, size)| size).sum()));
    // Largest first, so one big model file does not start last.
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by_key(|&index| std::cmp::Reverse(files[index].1));
    let next = AtomicUsize::new(0);
    let digests: Vec<OnceLock<String>> = files.iter().map(|_| OnceLock::new()).collect();
    let workers = thread::available_parallelism().map_or(4, |n| n.get().min(16));
    thread::scope(|scope| {
        let handles: Vec<_> = (0..workers.min(files.len()))
            .map(|_| {
                scope.spawn(|| -> Result<()> {
                    while let Some(&index) = order.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let path = files[index].0;
                        let file = File::open(path)
                            .with_context(|| format!("Cannot read {}", path.display()))?;
                        let _ = digests[index].set(sha256(file, Some(&progress))?);
                    }
                    Ok(())
                })
            })
            .collect();
        handles
            .into_iter()
            .try_for_each(|handle| handle.join().expect("hash worker"))
    })?;
    Ok(digests
        .into_iter()
        .map(|digest| digest.into_inner().expect("every file hashed"))
        .collect())
}

/// Which files are present with the expected size and SHA-256, checked on several threads.
pub(crate) fn verified_all(files: &[(PathBuf, Download<'_>)]) -> Result<Vec<bool>> {
    let present: Vec<usize> = (0..files.len())
        .filter(|&index| {
            let (path, spec) = &files[index];
            fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() == spec.size)
        })
        .collect();
    let paths: Vec<_> = present
        .iter()
        .map(|&index| (files[index].0.as_path(), files[index].1.size))
        .collect();
    let mut valid = vec![false; files.len()];
    for (index, digest) in present
        .into_iter()
        .zip(sha256_files(&paths, "Verifying model files")?)
    {
        valid[index] = digest == files[index].1.sha256;
    }
    Ok(valid)
}

/// Lowercase hexadecimal, as used in SHA-256 manifests.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn verified(path: &Path, spec: &Download<'_>) -> Result<bool> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("Cannot read {}", path.display())),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != spec.size {
        return Ok(false);
    }
    let progress = Progress::file(
        &format!("Verifying {}", spec.name),
        &path_for_progress(path, spec),
        spec.size,
        Phase::Verify,
    );
    Ok(sha256(file, Some(&progress))? == spec.sha256)
}

fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("Cache path has no parent")?)?.sync_all()?;
    Ok(())
}

/// The checklist tracks a pinned file by its final name, whether it is read from there or from
/// its `.part` download.
fn path_for_progress<'a>(path: &'a Path, spec: &Download<'_>) -> std::borrow::Cow<'a, Path> {
    let partial = format!("{}.part", spec.name);
    if path.file_name().and_then(|n| n.to_str()) == Some(partial.as_str()) {
        path.with_file_name(spec.name).into()
    } else {
        path.into()
    }
}

/// Download a missing file into its `.part` without verifying it, when a full-sized copy is not
/// already present. [`fetch`] then verifies, and repairs anything that fails.
pub(crate) fn prefetch(spec: &Download<'_>, dir: &Path, offline: bool) -> Result<()> {
    let full = |path: PathBuf| {
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.len() == spec.size)
    };
    if offline
        || spec.url.is_empty()
        || full(dir.join(spec.name))
        || full(dir.join(format!("{}.part", spec.name)))
    {
        return Ok(());
    }
    download(spec, dir)
}

pub(crate) fn fetch(spec: &Download<'_>, dir: &Path, offline: bool) -> Result<PathBuf> {
    let destination = dir.join(spec.name);
    if verified(&destination, spec)? {
        return Ok(destination);
    }
    let partial = dir.join(format!("{}.part", spec.name));
    if verified(&partial, spec)? {
        fs::rename(&partial, &destination)?;
        sync_parent(&destination)?;
        return Ok(destination);
    }
    ensure!(
        !offline,
        "{} is missing or corrupted and offline mode is enabled.",
        spec.name
    );
    download(spec, dir)?;
    ensure!(
        verified(&partial, spec)?,
        "{} failed its size or SHA-256 check; it was not installed. Run again to retry.",
        spec.name
    );
    fs::rename(&partial, &destination)?;
    sync_parent(&destination)?;
    Ok(destination)
}

/// Download or resume `spec` into its `.part` file.
fn download(spec: &Download<'_>, dir: &Path) -> Result<()> {
    let partial = dir.join(format!("{}.part", spec.name));
    fs::create_dir_all(dir)?;
    let offset = fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
    // A full-sized partial with an incorrect checksum cannot be resumed.
    let offset = if offset < spec.size { offset } else { 0 };
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(30 * 60))
        .user_agent(concat!("boubou/", env!("CARGO_PKG_VERSION")));
    // Unit tests use local HTTP fixtures, independent of the developer's proxy.
    #[cfg(test)]
    let client = client.no_proxy();
    let client = client.build()?;
    let progress = Progress::file(
        &format!(
            "{} {}",
            if offset == 0 {
                "Downloading"
            } else {
                "Resuming"
            },
            spec.name,
        ),
        &dir.join(spec.name),
        spec.size,
        Phase::Download,
    );
    if spec.size - offset > 2 * CHUNK {
        download_ranges(
            &client,
            spec,
            &partial,
            offset,
            &progress,
            CHUNK,
            CONNECTIONS,
        )
    } else {
        download_stream(&client, spec, &partial, offset, &progress)
    }
}

/// Appends the rest of the file from one request, restarting if the server ignores the range.
fn download_stream(
    client: &Client,
    spec: &Download<'_>,
    partial: &Path,
    offset: u64,
    progress: &Progress,
) -> Result<()> {
    progress.status(&format!("Connecting to download server for {}", spec.name));
    let mut request = client
        .get(spec.url)
        .header(header::ACCEPT_ENCODING, "identity");
    if offset > 0 {
        request = request.header(header::RANGE, format!("bytes={offset}-"));
    }
    let mut response = request
        .send()
        .map_err(reqwest::Error::without_url)
        .with_context(|| format!("Cannot download {}", spec.name))?;
    let start = match response.status() {
        StatusCode::OK => 0,
        StatusCode::PARTIAL_CONTENT => {
            let expected = format!("bytes {offset}-{}/{}", spec.size - 1, spec.size);
            ensure!(
                response
                    .headers()
                    .get(header::CONTENT_RANGE)
                    .and_then(|h| h.to_str().ok())
                    == Some(expected.as_str()),
                "{} returned an inconsistent Content-Range; partial download preserved",
                spec.name
            );
            offset
        }
        status => bail!("Download of {} failed: HTTP {status}", spec.name),
    };
    if let Some(length) = response.content_length() {
        ensure!(
            length == spec.size - start,
            "{} returned an unexpected download size; partial download preserved",
            spec.name
        );
    }
    progress.status(&format!("Receiving {}", spec.name));

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(start > 0)
        .truncate(start == 0)
        .open(partial)?;
    let mut received = start;
    progress.advance(start);
    let mut buffer = vec![0; 256 * 1024];
    loop {
        let n = response
            .read(&mut buffer)
            .with_context(|| format!("Download interrupted; run again to resume {}", spec.name))?;
        if n == 0 {
            break;
        }
        ensure!(
            received + n as u64 <= spec.size,
            "{} exceeded its expected size; download was not installed",
            spec.name
        );
        file.write_all(&buffer[..n])?;
        received += n as u64;
        progress.advance(n as u64);
    }
    file.sync_all()?;
    Ok(())
}

#[derive(Default)]
struct Chunks {
    ready: BTreeMap<usize, Vec<u8>>,
    written: usize,
    failure: Option<anyhow::Error>,
    ignored_range: bool,
}

/// Downloads the rest of the file in `chunk`-sized ranges over `connections` connections and
/// appends them in order. Connections stay at most two chunks each ahead of the writer.
fn download_ranges(
    client: &Client,
    spec: &Download<'_>,
    partial: &Path,
    offset: u64,
    progress: &Progress,
    chunk: u64,
    connections: usize,
) -> Result<()> {
    let count = (spec.size - offset).div_ceil(chunk) as usize;
    let window = 2 * connections;
    progress.status(&format!(
        "Connecting to download server for {} ({} connections)",
        spec.name,
        connections.min(count)
    ));
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(offset > 0)
        .truncate(offset == 0)
        .open(partial)?;
    progress.advance(offset);
    let state = Mutex::new(Chunks::default());
    let changed = Condvar::new();
    let next = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..connections.min(count) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        return;
                    }
                    let waiting = changed
                        .wait_while(state.lock().expect("download lock"), |chunks| {
                            index >= chunks.written + window && chunks.failure.is_none()
                        })
                        .expect("download lock");
                    if waiting.failure.is_some() {
                        return;
                    }
                    drop(waiting);
                    let start = offset + index as u64 * chunk;
                    let received = range(
                        client,
                        spec,
                        start,
                        (start + chunk).min(spec.size),
                        progress,
                    );
                    let mut chunks = state.lock().expect("download lock");
                    match received {
                        Ok(Some(bytes)) => {
                            chunks.ready.insert(index, bytes);
                        }
                        Ok(None) => {
                            chunks.ignored_range = true;
                            chunks.failure.get_or_insert_with(|| {
                                anyhow::anyhow!("{} ignored a ranged request", spec.name)
                            });
                        }
                        Err(error) => {
                            chunks.failure.get_or_insert(error);
                        }
                    }
                    changed.notify_all();
                }
            });
        }
        let mut chunks = state.lock().expect("download lock");
        while chunks.written < count {
            let written = chunks.written;
            if let Some(bytes) = chunks.ready.remove(&written) {
                drop(chunks);
                let appended = file.write_all(&bytes);
                chunks = state.lock().expect("download lock");
                if let Err(error) = appended {
                    chunks.failure.get_or_insert(error.into());
                } else {
                    progress.status(&format!("Receiving {}", spec.name));
                    progress.advance(bytes.len() as u64);
                    chunks.written += 1;
                }
                changed.notify_all();
            } else if chunks.failure.is_some() {
                break;
            } else {
                chunks = changed.wait(chunks).expect("download lock");
            }
        }
    });
    file.sync_all()?;
    drop(file);
    let chunks = state.into_inner().expect("download lock");
    match chunks.failure {
        None => Ok(()),
        Some(_) if chunks.ignored_range && chunks.written == 0 => {
            download_stream(client, spec, partial, offset, progress)
        }
        Some(error) => Err(error.context(format!(
            "Download interrupted; run again to resume {}",
            spec.name
        ))),
    }
}

/// Bytes `start..end` of the file, or None if the server answered with the whole file.
fn range(
    client: &Client,
    spec: &Download<'_>,
    start: u64,
    end: u64,
    progress: &Progress,
) -> Result<Option<Vec<u8>>> {
    let attempt = || -> Result<Option<Vec<u8>>> {
        let mut response = client
            .get(spec.url)
            .header(header::ACCEPT_ENCODING, "identity")
            .header(header::RANGE, format!("bytes={start}-{}", end - 1))
            .send()
            .map_err(reqwest::Error::without_url)?;
        match response.status() {
            StatusCode::OK => return Ok(None),
            StatusCode::PARTIAL_CONTENT => {}
            status => bail!("HTTP {status}"),
        }
        let expected = format!("bytes {start}-{}/{}", end - 1, spec.size);
        ensure!(
            response
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|h| h.to_str().ok())
                == Some(expected.as_str()),
            "inconsistent Content-Range"
        );
        progress.status(&format!("Receiving {}", spec.name));
        let mut bytes = Vec::with_capacity((end - start) as usize);
        response
            .by_ref()
            .take(end - start + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 == end - start, "unexpected chunk size");
        Ok(Some(bytes))
    };
    // A dropped connection is retried, since one failure would otherwise stop all connections.
    let mut result = attempt();
    for retry in 1..=2 {
        let Err(error) = &result else { break };
        // The status deliberately omits the raw error, which can contain a redirect URL.
        let reason = if let Some(error) = error.downcast_ref::<reqwest::Error>() {
            if error.is_timeout() {
                "connection timed out"
            } else if error.is_connect() {
                "connection failed"
            } else {
                "connection interrupted"
            }
        } else if error.downcast_ref::<std::io::Error>().is_some() {
            "connection interrupted"
        } else {
            "server returned an invalid response"
        };
        progress.status(&format!(
            "Retry {retry}/2: {reason} · {} bytes {start}-{}",
            spec.name,
            end - 1
        ));
        result = attempt();
    }
    result.with_context(|| format!("Cannot download {}", spec.name))
}

pub(crate) fn runtime_valid(archive: &Path, installed: &Path, server: &str) -> Result<bool> {
    if !installed.is_dir() {
        return Ok(false);
    }
    let _progress = Progress::new("Verifying the installed speech engine", None);
    if archive
        .extension()
        .is_some_and(|extension| extension == "zip")
    {
        let mut zip = zip::ZipArchive::new(File::open(archive)?)?;
        for index in 0..zip.len() {
            let mut entry = zip.by_index(index)?;
            let path = installed.join(
                entry
                    .enclosed_name()
                    .context("Invalid speech engine path")?,
            );
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            if entry.is_dir() {
                if !metadata.is_dir() {
                    return Ok(false);
                }
            } else if !metadata.file_type().is_file()
                || metadata.len() != entry.size()
                || sha256(File::open(&path)?, None)? != sha256(&mut entry, None)?
            {
                return Ok(false);
            }
        }
        return executable_valid(&installed.join(server));
    }
    let mut tar = tar::Archive::new(GzDecoder::new(File::open(archive)?));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = installed.join(entry.path()?);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let kind = entry.header().entry_type();
        if kind.is_file() {
            if !metadata.is_file()
                || metadata.len() != entry.size()
                || sha256(File::open(&path)?, None)? != sha256(&mut entry, None)?
            {
                return Ok(false);
            }
        } else if kind.is_dir() {
            if !metadata.is_dir() {
                return Ok(false);
            }
        } else if kind.is_symlink() {
            if !metadata.file_type().is_symlink()
                || Some(fs::read_link(&path)?) != entry.link_name()?.map(|p| p.into_owned())
            {
                return Ok(false);
            }
        } else {
            bail!("Unsupported entry in the pinned speech engine archive");
        }
    }
    executable_valid(&installed.join(server))
}

fn executable_valid(server: &Path) -> Result<bool> {
    if !server.is_file() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(server)?.permissions().mode() & 0o111 == 0 {
            return Ok(false);
        }
    }
    Ok(server.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::BufRead,
        net::TcpListener,
        sync::atomic::{AtomicU64, Ordering},
        thread,
    };

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "boubou-assets-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn serve(response: &'static str) -> (String, thread::JoinHandle<String>) {
        serve_sequence(&[response])
    }

    fn serve_sequence(responses: &[&'static str]) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/weights", listener.local_addr().unwrap());
        let responses = responses.to_vec();
        let worker = thread::spawn(move || {
            let mut request = String::new();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            for response in responses {
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline, "missing HTTP request");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(&mut socket);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    request.push_str(&line);
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                socket.write_all(response.as_bytes()).unwrap();
            }
            request
        });
        (url, worker)
    }

    fn spec<'a>(url: &'a str, hash: &'a str) -> Download<'a> {
        Download {
            name: "weights",
            url,
            size: 6,
            sha256: hash,
        }
    }

    /// Serves `body` with byte ranges on any number of connections. With `fail_from`, ranges
    /// starting there or later fail; with `ignore_ranges`, every answer is the whole body.
    fn serve_ranges(body: &'static [u8], fail_from: Option<u64>, ignore_ranges: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/weights", listener.local_addr().unwrap());
        thread::spawn(move || {
            for socket in listener.incoming() {
                let mut socket = socket.unwrap();
                thread::spawn(move || {
                    let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
                    let mut range = None;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(bytes) = line.to_lowercase().strip_prefix("range: bytes=") {
                            let (start, end) = bytes.trim().split_once('-').unwrap();
                            range = Some((
                                start.parse::<usize>().unwrap(),
                                end.parse::<usize>().unwrap(),
                            ));
                        }
                    }
                    let head = match range {
                        Some((start, _)) if fail_from.is_some_and(|from| start as u64 >= from) => {
                            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                        }
                        Some((start, end)) if !ignore_ranges => format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nConnection: close\r\n\r\n",
                            end + 1 - start,
                            body.len()
                        ),
                        _ => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()),
                    };
                    let payload = match range {
                        _ if head.starts_with("HTTP/1.1 500") => &[][..],
                        Some((start, end)) if !ignore_ranges => &body[start..=end],
                        _ => body,
                    };
                    let _ = socket.write_all(head.as_bytes());
                    let _ = socket.write_all(payload);
                });
            }
        });
        url
    }

    fn ranged(dir: &TestDir, url: &str, offset: u64, hash: &str) -> Result<Vec<u8>> {
        let spec = Download {
            name: "weights",
            url,
            size: 100,
            sha256: hash,
        };
        let client = Client::builder().no_proxy().build()?;
        let partial = dir.0.join("weights.part");
        let progress = Progress::new("test", None);
        download_ranges(&client, &spec, &partial, offset, &progress, 7, 3)?;
        Ok(fs::read(partial)?)
    }

    #[test]
    fn ranged_download_appends_chunks_in_order_and_resumes() {
        static BODY: [u8; 100] = {
            let mut body = [0; 100];
            let mut i = 0;
            while i < 100 {
                body[i] = i as u8;
                i += 1;
            }
            body
        };
        let dir = TestDir::new();
        let hash = sha256(&BODY[..], None).unwrap();
        assert_eq!(
            ranged(&dir, &serve_ranges(&BODY, None, false), 0, &hash).unwrap(),
            BODY
        );

        // A failed range stops every connection; the partial keeps only whole chunks before it.
        fs::remove_file(dir.0.join("weights.part")).unwrap();
        let error = ranged(&dir, &serve_ranges(&BODY, Some(35), false), 0, &hash).unwrap_err();
        assert!(
            error.to_string().contains("run again to resume"),
            "{error:#}"
        );
        let kept = fs::read(dir.0.join("weights.part")).unwrap();
        assert!(kept.len() <= 35 && kept.len().is_multiple_of(7) && kept == BODY[..kept.len()]);
        let resumed = ranged(
            &dir,
            &serve_ranges(&BODY, None, false),
            kept.len() as u64,
            &hash,
        );
        assert_eq!(resumed.unwrap(), BODY);

        // A server without ranges is read as one stream.
        fs::remove_file(dir.0.join("weights.part")).unwrap();
        assert_eq!(
            ranged(&dir, &serve_ranges(&BODY, None, true), 0, &hash).unwrap(),
            BODY
        );
    }

    #[test]
    fn ranged_download_retries_transient_failure_and_keeps_its_attempt_limit() {
        const FAILED: &str =
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        const RECEIVED: &str = "HTTP/1.1 206 Partial Content\r\nContent-Length: 6\r\nContent-Range: bytes 0-5/6\r\nConnection: close\r\n\r\nabcdef";
        let dir = TestDir::new();
        let client = Client::builder().no_proxy().build().unwrap();
        let hash = sha256(&b"abcdef"[..], None).unwrap();
        let partial = dir.0.join("weights.part");
        let progress = Progress::new("Downloading weights", Some(6));
        let (url, worker) = serve_sequence(&[FAILED, RECEIVED]);
        download_ranges(&client, &spec(&url, &hash), &partial, 0, &progress, 6, 1).unwrap();
        assert_eq!(fs::read(&partial).unwrap(), b"abcdef");
        assert_eq!(worker.join().unwrap().matches("GET /weights ").count(), 2);

        let (url, worker) = serve_sequence(&[FAILED, FAILED, FAILED]);
        let error =
            download_ranges(&client, &spec(&url, &hash), &partial, 0, &progress, 6, 1).unwrap_err();
        assert!(error.to_string().contains("run again to resume"));
        assert_eq!(worker.join().unwrap().matches("GET /weights ").count(), 3);
        assert_eq!(fs::read(&partial).unwrap(), b"");
    }

    #[test]
    fn files_are_hashed_in_their_given_order() {
        let dir = TestDir::new();
        let small = dir.0.join("small");
        let large = dir.0.join("large");
        fs::write(&small, b"abc").unwrap();
        fs::write(&large, b"abcdef").unwrap();
        let digests = sha256_files(&[(&small, 3), (&large, 6)], "test").unwrap();
        assert_eq!(digests[0], sha256(&b"abc"[..], None).unwrap());
        assert_eq!(digests[1], sha256(&b"abcdef"[..], None).unwrap());
        let files = [
            (small.clone(), spec("", &digests[0])),
            (large, spec("", &digests[1])),
            (dir.0.join("missing"), spec("", &digests[1])),
        ];
        assert_eq!(verified_all(&files).unwrap(), [false, true, false]);
    }

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(
            sha256(&b"abc"[..], None).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn resumes_partial_and_publishes_only_verified_bytes() {
        let dir = TestDir::new();
        fs::write(dir.0.join("weights.part"), b"abc").unwrap();
        let hash = sha256(&b"abcdef"[..], None).unwrap();
        let (url, worker) = serve(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nConnection: close\r\n\r\ndef",
        );
        let path = fetch(&spec(&url, &hash), &dir.0, false).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"abcdef");
        assert!(!dir.0.join("weights.part").exists());
        assert!(
            worker
                .join()
                .unwrap()
                .to_lowercase()
                .contains("range: bytes=3-")
        );
    }

    #[test]
    fn restarts_when_server_ignores_range() {
        let dir = TestDir::new();
        fs::write(dir.0.join("weights.part"), b"abc").unwrap();
        let hash = sha256(&b"abcdef"[..], None).unwrap();
        let (url, worker) =
            serve("HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdef");
        let path = fetch(&spec(&url, &hash), &dir.0, false).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"abcdef");
        worker.join().unwrap();
    }

    #[test]
    fn refuses_inconsistent_range_without_destroying_partial() {
        let dir = TestDir::new();
        fs::write(dir.0.join("weights.part"), b"abc").unwrap();
        let hash = sha256(&b"abcdef"[..], None).unwrap();
        let (url, worker) = serve(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 0-2/6\r\nConnection: close\r\n\r\nabc",
        );
        assert!(fetch(&spec(&url, &hash), &dir.0, false).is_err());
        assert_eq!(fs::read(dir.0.join("weights.part")).unwrap(), b"abc");
        assert!(!dir.0.join("weights").exists());
        worker.join().unwrap();
    }

    #[test]
    fn offline_checks_hash_and_never_connects() {
        let dir = TestDir::new();
        let hash = sha256(&b"abcdef"[..], None).unwrap();
        let spec = spec("http://127.0.0.1:1/must-not-connect", &hash);
        assert!(
            fetch(&spec, &dir.0, true)
                .unwrap_err()
                .to_string()
                .contains("offline mode")
        );
        fs::write(dir.0.join("weights"), b"badbad").unwrap();
        assert!(
            fetch(&spec, &dir.0, true)
                .unwrap_err()
                .to_string()
                .contains("offline mode")
        );
        fs::write(dir.0.join("weights"), b"abcdef").unwrap();
        assert!(fetch(&spec, &dir.0, true).is_ok());
    }

    #[test]
    fn doctor_rejects_an_empty_cache() {
        let dir = TestDir::new();
        assert!(
            status(&crate::profiles::DEFAULT, &dir.0)
                .unwrap_err()
                .to_string()
                .contains("boubou pull LFM3-Audio-2B")
        );
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 0);
    }

    #[test]
    fn recovers_complete_partial_without_network() {
        let dir = TestDir::new();
        fs::write(dir.0.join("weights.part"), b"abcdef").unwrap();
        let hash = sha256(&b"abcdef"[..], None).unwrap();
        let path = fetch(&spec("http://127.0.0.1:1", &hash), &dir.0, true).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"abcdef");
    }

    #[test]
    fn does_not_install_corrupted_download() {
        let dir = TestDir::new();
        let hash = sha256(&b"abcdef"[..], None).unwrap();
        let (url, worker) =
            serve("HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdeg");
        assert!(fetch(&spec(&url, &hash), &dir.0, false).is_err());
        assert!(!dir.0.join("weights").exists());
        worker.join().unwrap();
    }

    #[test]
    fn interrupted_body_preserves_bytes_for_next_attempt() {
        let dir = TestDir::new();
        let hash = sha256(&b"abcdef"[..], None).unwrap();
        let (url, worker) =
            serve("HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc");
        assert!(fetch(&spec(&url, &hash), &dir.0, false).is_err());
        assert_eq!(fs::read(dir.0.join("weights.part")).unwrap(), b"abc");
        assert!(!dir.0.join("weights").exists());
        worker.join().unwrap();
    }

    #[test]
    fn installed_engine_checks_actual_contents() {
        let dir = TestDir::new();
        let server = "fixture/llama-server";
        let archive = dir.0.join("engine.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            File::create(&archive).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        header.set_size(6);
        header.set_cksum();
        tar.append_data(&mut header, server, &b"engine"[..])
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();

        let installed = dir.0.join("installed");
        assert!(!runtime_valid(&archive, &installed, server).unwrap());
        tar::Archive::new(GzDecoder::new(File::open(&archive).unwrap()))
            .unpack(&installed)
            .unwrap();
        assert!(runtime_valid(&archive, &installed, server).unwrap());
        fs::write(installed.join(server), b"broken").unwrap();
        assert!(!runtime_valid(&archive, &installed, server).unwrap());
    }

    #[test]
    fn zip_engine_checks_executable_and_library_contents() {
        let dir = TestDir::new();
        let archive = dir.0.join("engine.zip");
        let mut zip = zip::ZipWriter::new(File::create(&archive).unwrap());
        for name in ["runner/server", "runner/lib.dylib"] {
            zip.start_file(
                name,
                zip::write::SimpleFileOptions::default().unix_permissions(0o755),
            )
            .unwrap();
            zip.write_all(b"engine").unwrap();
        }
        zip.finish().unwrap();
        let installed = dir.0.join("installed");
        zip::ZipArchive::new(File::open(&archive).unwrap())
            .unwrap()
            .extract(&installed)
            .unwrap();
        assert!(runtime_valid(&archive, &installed, "runner/server").unwrap());
        fs::write(installed.join("runner/lib.dylib"), b"broken").unwrap();
        assert!(!runtime_valid(&archive, &installed, "runner/server").unwrap());
        fs::write(installed.join("runner/lib.dylib"), b"engine").unwrap();
        fs::remove_file(installed.join("runner/server")).unwrap();
        assert!(!runtime_valid(&archive, &installed, "runner/server").unwrap());
    }
}
