//! Reuse verified files from an installed runner; fetch changed gzip members by byte range.
use super::{Runner, integrity, tree_matches};
use crate::{
    assets,
    profiles::Download,
    progress::{Phase, Progress},
};
use anyhow::{Context, Result, ensure};
use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs::{self, File},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
};

#[derive(Deserialize)]
pub(super) struct Entry {
    path: PathBuf,
    #[serde(flatten)]
    kind: Kind,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Kind {
    Dir,
    Link {
        target: PathBuf,
    },
    File {
        size: u64,
        sha256: String,
        executable: bool,
        offset: u64,
        compressed_size: u64,
    },
}

pub(super) fn validate(entries: &[Entry], archive_size: u64) -> Result<()> {
    let mut paths = HashMap::new();
    for entry in entries {
        ensure!(
            !entry.path.as_os_str().is_empty()
                && entry
                    .path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
                && paths.insert(&entry.path, &entry.kind).is_none(),
            "Invalid or duplicate incremental runner path"
        );
        match &entry.kind {
            Kind::File {
                sha256,
                offset,
                compressed_size,
                ..
            } => ensure!(
                sha256.len() == 64
                    && sha256.bytes().all(|b| b.is_ascii_hexdigit())
                    && *compressed_size > 0
                    && offset
                        .checked_add(*compressed_size)
                        .is_some_and(|end| end <= archive_size),
                "Invalid incremental runner file metadata"
            ),
            Kind::Link { target } => {
                // Relative links may use '..' but must resolve within the installed tree.
                let mut depth = entry.path.components().count() - 1;
                for part in target.components() {
                    match part {
                        Component::Normal(_) => depth += 1,
                        Component::CurDir => (),
                        Component::ParentDir if depth > 0 => depth -= 1,
                        _ => anyhow::bail!("Incremental runner link escapes its tree"),
                    }
                }
                ensure!(!target.as_os_str().is_empty(), "Empty runner link");
            }
            Kind::Dir => (),
        }
    }
    for entry in entries {
        for parent in entry
            .path
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
        {
            ensure!(
                matches!(paths.get(&parent.to_path_buf()), Some(Kind::Dir)),
                "Incremental runner path has a missing or non-directory parent"
            );
        }
    }
    Ok(())
}

impl Entry {
    /// Bytes of its gzip member in the archive; directories and links have none.
    fn compressed(&self) -> u64 {
        match self.kind {
            Kind::File {
                compressed_size, ..
            } => compressed_size,
            _ => 0,
        }
    }
}

/// Whether `root` holds `entry`'s file: the same size, and with `hash` the same SHA-256.
fn holds(root: &Path, entry: &Entry, hash: bool) -> bool {
    let Kind::File { size, sha256, .. } = &entry.kind else {
        return false;
    };
    let path = root.join(&entry.path);
    // No source path may traverse a symlink, even when it points to the expected bytes.
    integrity::safe_destination(root, &entry.path).is_ok()
        && fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() == *size)
        && (!hash
            || File::open(&path)
                .ok()
                .and_then(|file| assets::sha256(file, None).ok())
                .is_some_and(|digest| digest == *sha256))
}

/// Installed runners to reuse files from. None for a legacy manifest or a cold install, or when
/// a completed archive (including a resumed .part) already supplies every byte, also offline.
fn sources(runner: &Runner, root: &Path, installed: &Path) -> Vec<PathBuf> {
    let complete = ["runner.tar.gz", "runner.tar.gz.part"].iter().any(|name| {
        fs::symlink_metadata(installed.with_file_name(name))
            .is_ok_and(|m| m.is_file() && m.len() == runner.size)
    });
    if runner.url.is_none() || runner.entries.is_empty() || complete {
        return Vec::new();
    }
    fs::read_dir(root.join("runtimes"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("installed"))
        .filter(|path| fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()))
        .collect()
}

/// Compressed bytes an update would fetch, judged by file sizes, so the checklist shows the
/// update rather than the whole archive. Its download replaces this with the hashed result.
pub(super) fn estimate(runner: &Runner, root: &Path, installed: &Path) -> Option<u64> {
    let sources = sources(runner, root, installed);
    (!sources.is_empty()).then(|| {
        runner
            .entries
            .iter()
            .filter(|entry| !sources.iter().any(|source| holds(source, entry, false)))
            .map(Entry::compressed)
            .sum()
    })
}

/// Assembles `installed` from verified files of installed runners plus changed members fetched
/// by byte range. False means the complete archive is needed instead: nothing to reuse, a server
/// without ranges, or any failure. Older runtimes are preserved, and a partly assembled tree is
/// never activated.
pub(super) fn install(runner: &Runner, root: &Path, installed: &Path, offline: bool) -> bool {
    let sources = sources(runner, root, installed);
    let staging = installed.with_extension("partial");
    let done = !sources.is_empty()
        && assemble(runner, &sources, &staging, installed, offline).unwrap_or(false);
    let _ = fs::remove_dir_all(&staging);
    done
}

fn assemble(
    runner: &Runner,
    sources: &[PathBuf],
    staging: &Path,
    installed: &Path,
    offline: bool,
) -> Result<bool> {
    validate(&runner.entries, runner.size)?;
    if staging.exists() {
        fs::remove_dir_all(staging)?;
    }
    fs::create_dir_all(staging)?;
    // All parent paths were checked to be directories; links are created only after files.
    for entry in &runner.entries {
        if matches!(entry.kind, Kind::Dir) {
            fs::create_dir_all(staging.join(&entry.path))?;
        }
    }
    let mut missing = Vec::new();
    for entry in &runner.entries {
        if !matches!(entry.kind, Kind::File { .. }) {
            continue;
        }
        match sources.iter().find(|source| holds(source, entry, true)) {
            // Copy rather than hard-link: permissions or later corruption must not alter an old runner.
            Some(source) => {
                fs::copy(source.join(&entry.path), staging.join(&entry.path))?;
            }
            None => missing.push(entry),
        }
    }
    if offline && !missing.is_empty() || !fetch(runner, &missing, staging)? {
        return Ok(false);
    }
    for entry in &runner.entries {
        match &entry.kind {
            Kind::File { executable, .. } => fs::set_permissions(
                staging.join(&entry.path),
                fs::Permissions::from_mode(if *executable { 0o755 } else { 0o644 }),
            )?,
            Kind::Link { target } => symlink(target, staging.join(&entry.path))?,
            Kind::Dir => (),
        }
    }
    ensure!(
        tree_matches(staging, &runner.tree_sha256)?,
        "Incrementally updated runner failed verification"
    );
    if installed.exists() {
        fs::remove_dir_all(installed)?;
    }
    fs::rename(staging, installed)?;
    File::open(installed.parent().context("Missing runtime parent")?)?.sync_all()?;
    Ok(true)
}

/// Fetches the changed members on several connections, each range retried as a download's is.
/// False when the server ignores byte ranges.
fn fetch(runner: &Runner, missing: &[&Entry], staging: &Path) -> Result<bool> {
    if missing.is_empty() {
        return Ok(true);
    }
    let client = assets::download_client()?;
    let archive = runner.archive();
    let progress = Progress::file(
        &format!(
            "Updating the speech engine: downloading {} changed files, reusing {}",
            missing.len(),
            runner
                .entries
                .iter()
                .filter(|e| matches!(e.kind, Kind::File { .. }))
                .count()
                - missing.len()
        ),
        &staging.with_file_name("runner.tar.gz"),
        missing.iter().map(|entry| entry.compressed()).sum(),
        Phase::Download,
    );
    let next = AtomicUsize::new(0);
    thread::scope(|scope| {
        let workers: Vec<_> = (0..assets::CONNECTIONS.min(missing.len()))
            .map(|_| {
                scope.spawn(|| -> Result<bool> {
                    while let Some(entry) = missing.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let fetched = fetch_member(
                            client,
                            &archive,
                            entry,
                            &staging.join(&entry.path),
                            &progress,
                        );
                        if !matches!(fetched, Ok(true)) {
                            // The complete archive replaces this attempt, so the others stop too.
                            next.store(missing.len(), Ordering::Relaxed);
                            return fetched;
                        }
                    }
                    Ok(true)
                })
            })
            .collect();
        workers.into_iter().try_fold(true, |all, worker| {
            Ok(worker.join().expect("fetch worker")? && all)
        })
    })
}

/// Fetches and verifies one changed file. False when the server answers with the whole archive.
fn fetch_member(
    client: &Client,
    archive: &Download<'_>,
    entry: &Entry,
    destination: &Path,
    progress: &Progress,
) -> Result<bool> {
    let Kind::File {
        size,
        sha256,
        offset,
        compressed_size,
        ..
    } = &entry.kind
    else {
        unreachable!()
    };
    let Some(bytes) = assets::range(client, archive, *offset, offset + compressed_size, progress)?
    else {
        return Ok(false);
    };
    let mut member_archive = tar::Archive::new(GzDecoder::new(bytes.as_slice()));
    let mut entries = member_archive.entries()?;
    let mut member = entries
        .next()
        .context("Missing incremental runner tar member")??;
    ensure!(
        member.path()? == entry.path
            && member.header().entry_type().is_file()
            && member.size() == *size,
        "Unexpected incremental runner tar member"
    );
    let mut file = File::create(destination)?;
    std::io::copy(&mut member, &mut file)?;
    ensure!(
        entries.next().is_none(),
        "Extra incremental runner tar member"
    );
    ensure!(
        file.metadata()?.len() == *size
            && assets::sha256(File::open(destination)?, None)? == *sha256,
        "Incremental runner file failed verification"
    );
    file.sync_all()?;
    progress.advance(*compressed_size);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::atomic::AtomicU64,
        time::Duration,
    };

    fn fixture(test: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "boubou-delta-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root)?;
        let result = test(&root);
        fs::remove_dir_all(&root)?;
        result
    }

    fn member(path: &str, data: &[u8], offset: u64) -> (Entry, Vec<u8>) {
        let mut header = tar::Header::new_ustar();
        header.set_path(path).unwrap();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        let mut gzip = GzEncoder::new(Vec::new(), Compression::fast());
        gzip.write_all(header.as_bytes()).unwrap();
        gzip.write_all(data).unwrap();
        gzip.write_all(&vec![0; (512 - data.len() % 512) % 512])
            .unwrap();
        let bytes = gzip.finish().unwrap();
        (
            Entry {
                path: path.into(),
                kind: Kind::File {
                    size: data.len() as u64,
                    sha256: assets::sha256(data, None).unwrap(),
                    executable: false,
                    offset,
                    compressed_size: bytes.len() as u64,
                },
            },
            bytes,
        )
    }

    // Answers one connection per reply, in order: 206 with the requested range, 200 with the
    // whole archive, or 0 by closing the connection unanswered. Returns the requests.
    fn server(
        data: Vec<u8>,
        replies: Vec<u16>,
        corrupt: bool,
    ) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/runner", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let answer = |status: u16| {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    assert_eq!(stream.read(&mut byte).unwrap(), 1);
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap().to_lowercase();
                if status == 0 {
                    return request;
                }
                let range = request
                    .lines()
                    .find_map(|line| line.strip_prefix("range: bytes="))
                    .unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let (start, end): (usize, usize) = (start.parse().unwrap(), end.parse().unwrap());
                let mut body = if status == 200 {
                    data.clone()
                } else {
                    data[start..=end].to_vec()
                };
                if corrupt {
                    body[0] ^= 1;
                }
                let header = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nConnection: close\r\n\r\n",
                    body.len(),
                    data.len()
                );
                let _ = stream
                    .write_all(header.as_bytes())
                    .and_then(|()| stream.write_all(&body));
                request
            };
            replies.into_iter().map(answer).collect()
        });
        (url, handle)
    }

    fn runner(entries: Vec<Entry>, data: &[u8], url: String, expected: &Path) -> Result<Runner> {
        Ok(Runner {
            url: Some(url),
            size: data.len() as u64,
            sha256: assets::sha256(data, None)?,
            tree_sha256: integrity::tree_digest(expected)?.unwrap(),
            entries,
            host: super::super::Host::Mac {
                min_macos: "15.0".into(),
            },
        })
    }

    #[test]
    fn changed_file_downloads_only_its_range_and_preserves_models_and_old_runner() -> Result<()> {
        fixture(|root| {
            let old = root.join("runtimes/old/installed");
            let expected = root.join("expected");
            fs::create_dir_all(&old)?;
            fs::create_dir(&expected)?;
            fs::create_dir(root.join("hub"))?;
            fs::write(root.join("hub/model.bin"), b"downloaded model")?;
            fs::write(old.join("library"), b"unchanged library")?;
            fs::write(old.join("script"), b"old script")?;
            fs::write(expected.join("library"), b"unchanged library")?;
            fs::write(expected.join("script"), b"new script")?;
            let (library, mut data) = member("library", b"unchanged library", 0);
            let offset = data.len();
            let (script, part) = member("script", b"new script", offset as u64);
            data.extend(part);
            let (url, served) = server(data.clone(), vec![206], false);
            let runner = runner(vec![library, script], &data, url, &expected)?;
            let installed = root.join("runtimes/new/installed");
            assert!(install(&runner, root, &installed, false));
            let request = &served.join().unwrap()[0];
            assert!(request.contains(&format!("range: bytes={offset}-{}", data.len() - 1)));
            assert!(tree_matches(&installed, &runner.tree_sha256)?);
            assert_eq!(fs::read(old.join("script"))?, b"old script");
            assert_eq!(fs::read(root.join("hub/model.bin"))?, b"downloaded model");
            Ok(())
        })
    }

    #[test]
    fn changed_files_download_on_several_connections_and_a_dropped_one_is_retried() -> Result<()> {
        fixture(|root| {
            let old = root.join("runtimes/old/installed");
            let expected = root.join("expected");
            fs::create_dir_all(&old)?;
            fs::create_dir(&expected)?;
            let (mut entries, mut data) = (Vec::new(), Vec::new());
            for name in ["one", "two", "three"] {
                let new = format!("new {name}");
                fs::write(old.join(name), b"old")?;
                fs::write(expected.join(name), &new)?;
                let (entry, part) = member(name, new.as_bytes(), data.len() as u64);
                entries.push(entry);
                data.extend(part);
            }
            let (url, served) = server(data.clone(), vec![0, 206, 206, 206], false);
            let runner = runner(entries, &data, url, &expected)?;
            let installed = root.join("runtimes/new/installed");
            assert_eq!(estimate(&runner, root, &installed), Some(data.len() as u64));
            assert!(install(&runner, root, &installed, false));
            assert_eq!(served.join().unwrap().len(), 4);
            assert!(tree_matches(&installed, &runner.tree_sha256)?);
            Ok(())
        })
    }

    #[test]
    fn failures_leave_the_complete_archive_to_download() -> Result<()> {
        // A corrupted member, a server that ignores ranges, and missing files while offline.
        for reply in [Some((206, true)), Some((200, false)), None] {
            fixture(|root| {
                let old = root.join("runtimes/old/installed");
                fs::create_dir_all(&old)?;
                fs::write(old.join("file"), b"old")?;
                let expected = root.join("expected");
                fs::create_dir(&expected)?;
                fs::write(expected.join("file"), b"new")?;
                let (entry, data) = member("file", b"new", 0);
                let (url, served) = match reply {
                    Some((status, corrupt)) => {
                        let (url, handle) = server(data.clone(), vec![status], corrupt);
                        (url, Some(handle))
                    }
                    None => ("http://127.0.0.1:1".into(), None),
                };
                let runner = runner(vec![entry], &data, url, &expected)?;
                let installed = root.join("runtimes/new/installed");
                assert!(!install(&runner, root, &installed, reply.is_none()));
                if let Some(served) = served {
                    served.join().unwrap();
                }
                assert!(!installed.exists());
                assert!(!installed.with_extension("partial").exists());
                assert_eq!(fs::read(old.join("file"))?, b"old");
                Ok(())
            })?;
        }
        Ok(())
    }

    #[test]
    fn completed_cached_archives_take_priority_over_delta_even_offline() -> Result<()> {
        for name in ["runner.tar.gz", "runner.tar.gz.part"] {
            fixture(|root| {
                let old = root.join("runtimes/old/installed");
                fs::create_dir_all(&old)?;
                fs::write(old.join("file"), b"old")?;
                let expected = root.join("expected");
                fs::create_dir(&expected)?;
                fs::write(expected.join("file"), b"new")?;
                let (entry, data) = member("file", b"new", 0);
                let runner = runner(vec![entry], &data, "http://127.0.0.1:1".into(), &expected)?;
                let engine = root.join("runtimes/new");
                fs::create_dir(&engine)?;
                fs::write(engine.join(name), &data)?;
                let installed = engine.join("installed");
                assert!(!install(&runner, root, &installed, true));
                let archive = assets::fetch(&runner.archive(), &engine, true)?;
                assets::install_runtime(&archive, &installed, true, |path| {
                    tree_matches(path, &runner.tree_sha256)
                })?;
                assert!(tree_matches(&installed, &runner.tree_sha256)?);
                Ok(())
            })?;
        }
        Ok(())
    }

    #[test]
    fn full_install_reads_all_gzip_members() -> Result<()> {
        fixture(|root| {
            let (_, mut data) = member("one", b"first", 0);
            let (_, second) = member("two", b"second", data.len() as u64);
            data.extend(second);
            let archive = root.join("runner.tar.gz");
            fs::write(&archive, data)?;
            let installed = root.join("installed");
            assets::install_runtime(&archive, &installed, true, |path| {
                Ok(path.join("one").is_file() && path.join("two").is_file())
            })?;
            assert_eq!(fs::read(installed.join("two"))?, b"second");
            Ok(())
        })
    }

    #[test]
    fn index_rejects_traversal_duplicate_paths_and_symlink_parents() {
        for json in [
            r#"[{"path":"../outside","kind":"dir"}]"#,
            r#"[{"path":"/outside","kind":"dir"}]"#,
            r#"[{"path":"same","kind":"dir"},{"path":"same","kind":"dir"}]"#,
            r#"[{"path":"link","kind":"link","target":"../outside"}]"#,
            r#"[{"path":"link","kind":"link","target":"safe"},{"path":"link/child","kind":"dir"}]"#,
            r#"[{"path":"missing/child","kind":"dir"}]"#,
        ] {
            let entries: Vec<Entry> = serde_json::from_str(json).unwrap();
            assert!(validate(&entries, 100).is_err(), "{json}");
        }
    }
}
