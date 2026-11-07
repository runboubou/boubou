use crate::{
    profiles::{Download, Profile},
    progress::Progress,
};
use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use fs2::FileExt;
use reqwest::{StatusCode, blocking::Client, header};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone)]
pub struct Assets {
    pub profile: &'static Profile,
    pub server: PathBuf,
    pub model: PathBuf,
    pub projector: PathBuf,
}

pub fn prepare(profile: &'static Profile, cache_dir: &Path, offline: bool) -> Result<Assets> {
    ensure!(
        cfg!(all(target_os = "macos", target_arch = "aarch64")),
        "This first release supports Apple Silicon Macs."
    );
    let _lock = preparation_lock(cache_dir)?;

    let download = |spec: &Download<'_>, directory: &Path| {
        fetch(spec, directory, offline).with_context(|| {
            format!(
                "Cannot prepare {}. Run `boubou pull {}` while online.",
                profile.id, profile.id
            )
        })
    };
    let model_dir = cache_dir.join(profile.model_directory);
    let archive = download(&profile.runtime.archive, &cache_dir.join("downloads"))?;
    let installed = cache_dir.join(profile.runtime.directory);
    install_runtime(&archive, &installed, profile.runtime.server, false)?;

    // Cache the model's license alongside its weights before making it runnable.
    for spec in profile.model_assets() {
        download(spec, &model_dir)?;
    }
    Ok(Assets {
        profile,
        server: installed.join(profile.runtime.server),
        model: model_dir.join(profile.model.name),
        projector: model_dir.join(profile.projector.name),
    })
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

pub(crate) fn install_runtime(
    archive: &Path,
    installed: &Path,
    server: &str,
    refresh: bool,
) -> Result<()> {
    if refresh || !runtime_valid(archive, installed, server)? {
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
            runtime_valid(archive, &staging, server)?,
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
    let archive = cache_dir
        .join("downloads")
        .join(profile.runtime.archive.name);
    let runtime_ready = verified(&archive, &profile.runtime.archive)?
        && runtime_valid(
            &archive,
            &cache_dir.join(profile.runtime.directory),
            profile.runtime.server,
        )?;
    println!(
        "Speech engine ({}, macOS arm64): {}",
        profile.runtime.label,
        if runtime_ready {
            "verified"
        } else {
            "missing or incomplete"
        }
    );
    ensure!(
        ready && runtime_ready,
        "Liquid assets are missing or invalid. Run `boubou pull {}` while online.",
        profile.id
    );
    Ok(())
}

fn sha256(mut reader: impl Read, progress: Option<&Progress>) -> Result<String> {
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
    let progress = Progress::new(&format!("Verifying {}", spec.name), Some(spec.size));
    Ok(sha256(file, Some(&progress))? == spec.sha256)
}

fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("Cache path has no parent")?)?.sync_all()?;
    Ok(())
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
    let mut request = client
        .get(spec.url)
        .header(header::ACCEPT_ENCODING, "identity");
    if offset > 0 {
        request = request.header(header::RANGE, format!("bytes={offset}-"));
    }
    let progress = Progress::new(
        &format!(
            "{} {}",
            if offset == 0 {
                "Downloading"
            } else {
                "Resuming"
            },
            spec.name,
        ),
        Some(spec.size),
    );
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

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(start > 0)
        .truncate(start == 0)
        .open(&partial)?;
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
    drop(file);
    drop(progress);
    ensure!(
        verified(&partial, spec)?,
        "{} failed its size or SHA-256 check; it was not installed. Run again to retry.",
        spec.name
    );
    fs::rename(&partial, &destination)?;
    sync_parent(&destination)?;
    Ok(destination)
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/weights", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(&mut socket);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                request.push_str(&line);
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            socket.write_all(response.as_bytes()).unwrap();
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
