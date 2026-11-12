//! Reuse verified files from an installed runner; fetch changed gzip members by byte range.
use super::{Runner, integrity, tree_matches};
use crate::assets;
use anyhow::{Context, Result, ensure};
use flate2::read::GzDecoder;
use reqwest::{StatusCode, blocking::Client, header};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Component, Path, PathBuf},
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

fn regular_matches(root: &Path, entry: &Entry) -> Result<bool> {
    let Kind::File { size, sha256, .. } = &entry.kind else {
        return Ok(false);
    };
    // No source path may traverse a symlink, even when it points to the expected bytes.
    if integrity::safe_destination(root, &entry.path).is_err() {
        return Ok(false);
    }
    let path = root.join(&entry.path);
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return Ok(false);
    };
    if !metadata.is_file() || metadata.len() != *size {
        return Ok(false);
    }
    Ok(assets::sha256(File::open(path)?, None)? == *sha256)
}

/// False only for a legacy manifest, a cold install, or a server without Range support.
/// Errors preserve all older runtimes and never activate a partly assembled tree.
pub(super) fn install(
    runner: &Runner,
    root: &Path,
    installed: &Path,
    offline: bool,
) -> Result<bool> {
    let Some(url) = runner.url.as_deref().filter(|_| !runner.entries.is_empty()) else {
        return Ok(false);
    };
    // A completed archive (including a resumed .part) already supplies every byte offline.
    // Let the existing full-archive path verify and install it before considering ranges.
    let engine = installed.parent().context("Missing runtime parent")?;
    if ["runner.tar.gz", "runner.tar.gz.part"].iter().any(|name| {
        fs::symlink_metadata(engine.join(name)).is_ok_and(|m| m.is_file() && m.len() == runner.size)
    }) {
        return Ok(false);
    }
    let runtimes = root.join("runtimes");
    let mut sources = Vec::new();
    if runtimes.is_dir() {
        for entry in fs::read_dir(&runtimes)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let path = entry.path().join("installed");
                if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                    sources.push(path);
                }
            }
        }
    }
    if sources.is_empty() {
        return Ok(false);
    }
    validate(&runner.entries, runner.size)?;
    let staging = installed.with_extension("partial");
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    // All parent paths were checked to be directories; links are created only after files.
    for entry in &runner.entries {
        if matches!(entry.kind, Kind::Dir) {
            fs::create_dir_all(staging.join(&entry.path))?;
        }
    }
    let client = assets::download_client()?;
    let mut reused = 0;
    let mut downloaded = 0;
    for entry in &runner.entries {
        let Kind::File { executable, .. } = entry.kind else {
            continue;
        };
        let destination = staging.join(&entry.path);
        let mut found = false;
        for source in &sources {
            if regular_matches(source, entry)? {
                // Copy rather than hard-link: permissions or later corruption must not alter an old runner.
                fs::copy(source.join(&entry.path), &destination)?;
                found = true;
                reused += 1;
                break;
            }
        }
        if !found {
            ensure!(
                !offline,
                "{} is missing or corrupted and offline mode is enabled.",
                entry.path.display()
            );
            if !fetch_member(client, url, runner.size, entry, &destination)? {
                fs::remove_dir_all(&staging)?;
                eprintln!(
                    "The runner server does not support byte ranges; downloading the complete runner."
                );
                return Ok(false);
            }
            downloaded += 1;
        }
        fs::set_permissions(
            &destination,
            fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
        )?;
    }
    for entry in &runner.entries {
        if let Kind::Link { target } = &entry.kind {
            symlink(target, staging.join(&entry.path))?;
        }
    }
    ensure!(
        tree_matches(&staging, &runner.tree_sha256)?,
        "Incrementally updated runner failed verification"
    );
    if installed.exists() {
        fs::remove_dir_all(installed)?;
    }
    fs::rename(&staging, installed)?;
    File::open(installed.parent().context("Missing runtime parent")?)?.sync_all()?;
    eprintln!(
        "Updated the speech engine: reused {reused} files, downloaded {downloaded} changed files."
    );
    Ok(true)
}

fn fetch_member(
    client: &Client,
    url: &str,
    archive_size: u64,
    entry: &Entry,
    destination: &Path,
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
    let end = offset + compressed_size - 1;
    let response = client
        .get(url)
        .header(header::RANGE, format!("bytes={offset}-{end}"))
        .header(header::ACCEPT_ENCODING, "identity")
        .send()?
        .error_for_status()?;
    if response.status() == StatusCode::OK {
        return Ok(false);
    }
    ensure!(
        response.status() == StatusCode::PARTIAL_CONTENT
            && response
                .headers()
                .get(header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                == Some(format!("bytes {offset}-{end}/{archive_size}").as_str())
            && response
                .content_length()
                .is_none_or(|length| length == *compressed_size),
        "Invalid incremental runner byte-range response"
    );
    let mut archive = tar::Archive::new(GzDecoder::new(response.take(*compressed_size)));
    let mut entries = archive.entries()?;
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
    file.flush()?;
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
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::{
        net::TcpListener,
        sync::atomic::{AtomicU64, Ordering},
        thread,
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

    // A one-request server returns the requested member and records its Range header.
    fn server(data: Vec<u8>, status: u16, corrupt: bool) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/runner", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
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
            let (url, served) = server(data.clone(), 206, false);
            let runner = runner(vec![library, script], &data, url, &expected)?;
            let installed = root.join("runtimes/new/installed");
            assert!(install(&runner, root, &installed, false)?);
            let request = served.join().unwrap();
            assert!(request.contains(&format!("range: bytes={offset}-{}", data.len() - 1)));
            assert!(tree_matches(&installed, &runner.tree_sha256)?);
            assert_eq!(fs::read(old.join("script"))?, b"old script");
            assert_eq!(fs::read(root.join("hub/model.bin"))?, b"downloaded model");
            Ok(())
        })
    }

    #[test]
    fn corrupted_ranges_and_offline_missing_files_do_not_activate() -> Result<()> {
        for offline in [false, true] {
            fixture(|root| {
                let old = root.join("runtimes/old/installed");
                fs::create_dir_all(&old)?;
                fs::write(old.join("file"), b"old")?;
                let expected = root.join("expected");
                fs::create_dir(&expected)?;
                fs::write(expected.join("file"), b"new")?;
                let (entry, data) = member("file", b"new", 0);
                let (url, served) = if offline {
                    ("http://127.0.0.1:1".into(), None)
                } else {
                    let (url, handle) = server(data.clone(), 206, true);
                    (url, Some(handle))
                };
                let runner = runner(vec![entry], &data, url, &expected)?;
                let installed = root.join("runtimes/new/installed");
                assert!(install(&runner, root, &installed, offline).is_err());
                if let Some(served) = served {
                    served.join().unwrap();
                }
                assert!(!installed.exists());
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
                assert!(!install(&runner, root, &installed, true)?);
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
    fn range_ignored_falls_back_without_installing() -> Result<()> {
        fixture(|root| {
            let (entry, data) = member("file", b"new", 0);
            let (url, served) = server(data.clone(), 200, false);
            assert!(!fetch_member(
                &Client::builder().no_proxy().build()?,
                &url,
                data.len() as u64,
                &entry,
                &root.join("file")
            )?);
            served.join().unwrap();
            assert!(!root.join("file").exists());
            Ok(())
        })
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
