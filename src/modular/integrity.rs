use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

fn metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Cannot inspect {}", path.display())),
    }
}

pub(super) fn safe_destination(root: &Path, relative: &Path) -> Result<()> {
    ensure!(
        relative.file_name().is_some()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "Invalid modular runtime asset path: {}",
        relative.display()
    );
    // BOUBOU_HOME may itself be on a mounted volume. Check only the modular runtime's cache.
    let mut destination = root.to_owned();
    for part in relative {
        ensure!(
            metadata(&destination)?.is_none_or(|m| m.is_dir()),
            "Expected a directory at {}. Remove the conflicting modular runtime cache entry.",
            destination.display()
        );
        destination.push(part);
    }
    let mut partial = destination.as_os_str().to_owned();
    partial.push(".part");
    for path in [destination.as_path(), Path::new(&partial)] {
        if let Some(m) = metadata(path)? {
            ensure!(
                m.is_file(),
                "Expected a regular file at {}. Remove the conflicting modular runtime cache entry.",
                path.display()
            );
            #[cfg(unix)]
            if path != destination {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    m.nlink() == 1,
                    "Remove the hard-linked partial download at {}",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

fn include_path(expected: &mut HashSet<PathBuf>, path: &Path) {
    expected.extend(
        path.ancestors()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_owned),
    );
}

// Walk without following links: only expected directories and regular files may appear.
fn tree_entries(root: &Path, expected: &HashSet<PathBuf>) -> Result<bool> {
    if !metadata(root)?.is_some_and(|m| m.is_dir()) {
        return Ok(false);
    }
    let root = root.canonicalize()?;
    let mut directories = vec![root.clone()];
    while let Some(dir) = directories.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if !expected.contains(path.strip_prefix(&root)?) {
                eprintln!(
                    "Unexpected entry in the verified modular runtime cache: {}",
                    path.display()
                );
                return Ok(false);
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                directories.push(path);
            } else if !kind.is_file() {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// The digest a runner build records over its installed tree: one line per entry, sorted and
/// hashed, with each file's SHA-256. None when the tree is missing or holds an unsupported entry.
pub(super) fn tree_digest(root: &Path) -> Result<Option<String>> {
    use std::os::unix::fs::PermissionsExt;

    if !metadata(root)?.is_some_and(|m| m.is_dir()) {
        return Ok(None);
    }
    let mut lines = Vec::new();
    let mut files = Vec::new();
    let mut directories = vec![root.to_owned()];
    while let Some(dir) = directories.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let Some(relative) = path.strip_prefix(root)?.to_str().map(str::to_owned) else {
                return Ok(None);
            };
            if relative.contains(['\t', '\n']) {
                return Ok(None);
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                lines.push(format!("{relative}\tdir\n"));
                directories.push(path);
            } else if kind.is_symlink() {
                let Some(target) = fs::read_link(&path)?.to_str().map(str::to_owned) else {
                    return Ok(None);
                };
                lines.push(format!("{relative}\tlink\t{target}\n"));
            } else if kind.is_file() {
                let metadata = entry.metadata()?;
                let executable = metadata.permissions().mode() & 0o111 != 0;
                files.push((path, relative, metadata.len(), executable));
            } else {
                return Ok(None);
            }
        }
    }
    let sizes: Vec<_> = files
        .iter()
        .map(|(path, _, size, _)| (path.as_path(), *size))
        .collect();
    let digests = crate::assets::sha256_files(&sizes, "Verifying the modular runtime")?;
    for ((_, relative, _, executable), digest) in files.iter().zip(digests) {
        let kind = if *executable { "exec" } else { "file" };
        lines.push(format!("{relative}\t{kind}\t{digest}\n"));
    }
    lines.sort_unstable();
    let mut hash = Sha256::new();
    for line in &lines {
        hash.update(line.as_bytes());
    }
    Ok(Some(crate::assets::hex(&hash.finalize())))
}

fn split_snapshot(path: &Path) -> Result<(PathBuf, &Path)> {
    ensure!(
        path.iter().nth(2) == Some(OsStr::new("snapshots")) && path.iter().count() >= 5,
        "Invalid modular runtime snapshot path: {}",
        path.display()
    );
    let snapshot: PathBuf = path.iter().take(4).collect();
    let suffix = path.strip_prefix(&snapshot)?;
    Ok((snapshot, suffix))
}

/// Checks each snapshot holding a selected file; selected files were already verified by hash.
/// Profiles can share a snapshot, which may then also hold other listed files and their partials.
pub(super) fn model_snapshots(root: &Path, listed: &[&Path], selected: &[&Path]) -> Result<()> {
    let mut snapshots = HashMap::<PathBuf, HashSet<PathBuf>>::new();
    for &path in selected.iter().filter(|p| p.starts_with("hub")) {
        safe_destination(root, path)?;
        snapshots.entry(split_snapshot(path)?.0).or_default();
    }
    ensure!(
        !snapshots.is_empty(),
        "Packaged modular model manifest contains no snapshots"
    );
    for &path in listed.iter().filter(|p| p.starts_with("hub")) {
        let (snapshot, suffix) = split_snapshot(path)?;
        if let Some(expected) = snapshots.get_mut(&snapshot) {
            include_path(expected, suffix);
            let mut partial = suffix.as_os_str().to_owned();
            partial.push(".part");
            expected.insert(partial.into());
        }
    }
    for (snapshot, expected) in snapshots {
        let dir = root.join(snapshot);
        ensure!(
            tree_entries(&dir, &expected)?,
            "Modular model snapshot has missing or unexpected files. Remove {} and pull the model again.",
            dir.display()
        );
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::symlink,
        sync::atomic::{AtomicU64, Ordering},
    };

    fn fixture(test: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "boubou-modular-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root)?;
        let result = test(&root);
        fs::remove_dir_all(&root)?;
        result
    }

    #[test]
    fn tree_digest_covers_names_contents_modes_and_links() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        fixture(|root| {
            let tree = root.join("installed");
            assert_eq!(tree_digest(&tree)?, None);
            fs::create_dir_all(tree.join("bin"))?;
            fs::create_dir(tree.join("empty"))?;
            fs::write(tree.join("bin/python3.11"), b"python")?;
            fs::set_permissions(
                tree.join("bin/python3.11"),
                fs::Permissions::from_mode(0o755),
            )?;
            fs::write(tree.join("voice.py"), b"voice")?;
            fs::set_permissions(tree.join("voice.py"), fs::Permissions::from_mode(0o644))?;
            symlink("python3.11", tree.join("bin/python3"))?;
            // The runner build hashes the same sorted lines.
            let sha = |data: &[u8]| crate::assets::sha256(data, None).unwrap();
            let lines = format!(
                "bin\tdir\nbin/python3\tlink\tpython3.11\nbin/python3.11\texec\t{}\nempty\tdir\nvoice.py\tfile\t{}\n",
                sha(b"python"),
                sha(b"voice")
            );
            let expected = sha(lines.as_bytes());
            assert_eq!(tree_digest(&tree)?.as_deref(), Some(expected.as_str()));

            fs::set_permissions(tree.join("voice.py"), fs::Permissions::from_mode(0o755))?;
            assert_ne!(tree_digest(&tree)?.as_deref(), Some(expected.as_str()));
            fs::set_permissions(tree.join("voice.py"), fs::Permissions::from_mode(0o644))?;
            fs::write(tree.join("empty/extra"), b"")?;
            assert_ne!(tree_digest(&tree)?.as_deref(), Some(expected.as_str()));
            fs::remove_file(tree.join("empty/extra"))?;
            fs::remove_file(tree.join("bin/python3"))?;
            symlink("../../runner.tar.gz", tree.join("bin/python3"))?;
            assert_ne!(tree_digest(&tree)?.as_deref(), Some(expected.as_str()));
            fs::remove_file(tree.join("bin/python3"))?;
            symlink("python3.11", tree.join("bin/python3"))?;
            fs::write(tree.join("voice.py"), b"Voice")?;
            assert_ne!(tree_digest(&tree)?.as_deref(), Some(expected.as_str()));
            Ok(())
        })
    }

    #[test]
    fn checks_only_selected_snapshots_and_rejects_redirected_downloads() -> Result<()> {
        fixture(|root| {
            let snapshot = Path::new("hub/models--org--tts/snapshots/selected");
            let config = snapshot.join("speech_tokenizer/config.json");
            let selected = [config.as_path()];
            fs::create_dir_all(root.join(config.parent().unwrap()))?;
            fs::write(root.join(&config), "{}")?;
            fs::create_dir_all(root.join("hub/models--org--tts/snapshots/other"))?;
            model_snapshots(root, &selected, &selected)?;
            let extra = root.join(snapshot).join("tokenizer.json");
            fs::write(&extra, "{}")?;
            assert!(model_snapshots(root, &selected, &selected).is_err());
            fs::remove_file(extra)?;
            let partial = root.join(&config).with_file_name("config.json.part");
            symlink("config.json", &partial)?;
            assert!(safe_destination(root, &config).is_err());
            fs::remove_file(&partial)?;
            fs::hard_link(root.join(&config), &partial)?;
            assert!(safe_destination(root, &config).is_err());
            fs::remove_file(partial)?;
            let link = root.join(snapshot).join("redirect");
            symlink("speech_tokenizer", link)?;
            assert!(safe_destination(root, &snapshot.join("redirect/config.json")).is_err());
            assert!(safe_destination(root, Path::new("../outside")).is_err());
            Ok(())
        })
    }

    #[test]
    fn shared_snapshots_accept_only_other_listed_files() -> Result<()> {
        fixture(|root| {
            let snapshot = Path::new("hub/models--org--voices/snapshots/shared");
            let mine = snapshot.join("languages/french/model.safetensors");
            let theirs = snapshot.join("languages/german/model.safetensors");
            let listed = [mine.as_path(), theirs.as_path()];
            for path in listed {
                fs::create_dir_all(root.join(path.parent().unwrap()))?;
            }
            fs::write(root.join(&mine), "fr")?;
            // Another profile's download was interrupted, then completed.
            let partial = root.join(&theirs).with_file_name("model.safetensors.part");
            fs::write(&partial, "d")?;
            model_snapshots(root, &listed, &listed[..1])?;
            fs::rename(partial, root.join(&theirs))?;
            model_snapshots(root, &listed, &listed[..1])?;
            fs::write(root.join(snapshot).join("languages/german/injected.py"), "")?;
            assert!(model_snapshots(root, &listed, &listed[..1]).is_err());
            Ok(())
        })
    }
}
