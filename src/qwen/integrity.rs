use anyhow::{Context, Result, ensure};
use flate2::read::GzDecoder;
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs::{self, File},
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
        "Invalid Qwen asset path: {}",
        relative.display()
    );
    // BOUBOU_HOME may itself be on a mounted volume. Check only the Qwen-owned tree.
    let mut destination = root.to_owned();
    for part in relative {
        ensure!(
            metadata(&destination)?.is_none_or(|m| m.is_dir()),
            "Expected a directory at {}. Remove the conflicting Qwen cache entry.",
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
                "Expected a regular file at {}. Remove the conflicting Qwen cache entry.",
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

// Walk without following links. Returns how many entries were visited, or None for an unexpected one.
fn tree_entries(
    root: &Path,
    expected: &HashSet<PathBuf>,
    allow_links: bool,
) -> Result<Option<usize>> {
    if !metadata(root)?.is_some_and(|m| m.is_dir()) {
        return Ok(None);
    }
    let root = root.canonicalize()?;
    let mut directories = vec![root.clone()];
    let mut count = 0;
    while let Some(dir) = directories.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if !expected.contains(path.strip_prefix(&root)?) {
                eprintln!(
                    "Unexpected entry in the verified Qwen cache: {}",
                    path.display()
                );
                return Ok(None);
            }
            count += 1;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                directories.push(path);
            } else if kind.is_symlink() {
                if !allow_links
                    || fs::read_link(&path)?.is_absolute()
                    || !path
                        .canonicalize()
                        .is_ok_and(|target| target.starts_with(&root))
                {
                    return Ok(None);
                }
            } else if !kind.is_file() {
                return Ok(None);
            }
        }
    }
    Ok(Some(count))
}

pub(super) fn runtime_tree(archive: &Path, installed: &Path) -> Result<bool> {
    if !metadata(installed)?.is_some_and(|m| m.is_dir()) {
        return Ok(false);
    }
    let mut expected = HashSet::new();
    let mut tar = tar::Archive::new(GzDecoder::new(File::open(archive)?));
    for entry in tar.entries()? {
        let entry = entry?;
        let path = entry.path()?;
        ensure!(
            path.components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
            "Unsafe path in Qwen runner archive"
        );
        let relative: PathBuf = path.iter().filter(|c| *c != OsStr::new(".")).collect();
        include_path(&mut expected, &relative);
    }
    // Count the entries too, so a missing subtree cannot pass.
    Ok(tree_entries(installed, &expected, true)? == Some(expected.len()))
}

fn split_snapshot(path: &Path) -> Result<(PathBuf, &Path)> {
    ensure!(
        path.iter().nth(2) == Some(OsStr::new("snapshots")) && path.iter().count() >= 5,
        "Invalid Qwen snapshot path: {}",
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
        "Packaged Qwen model manifest contains no snapshots"
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
            tree_entries(&dir, &expected, false)?.is_some(),
            "Qwen snapshot has missing or unexpected files. Remove {} and pull the model again.",
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
            "boubou-qwen-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root)?;
        let result = test(&root);
        fs::remove_dir_all(&root)?;
        result
    }

    #[test]
    fn rejects_unlisted_python_and_escaping_interpreter_links() -> Result<()> {
        fixture(|root| {
            let installed = root.join("installed");
            let bin = installed.join("bin");
            let packages = installed.join("lib/python3.11/site-packages");
            fs::create_dir_all(&bin)?;
            fs::create_dir_all(&packages)?;
            fs::write(bin.join("python3.11"), "engine")?;
            symlink("python3.11", bin.join("python3"))?;
            let archive = root.join("runner.tar.gz");
            let encoder =
                flate2::write::GzEncoder::new(File::create(&archive)?, flate2::Compression::fast());
            let mut tar = tar::Builder::new(encoder);
            tar.follow_symlinks(false);
            tar.append_dir_all(".", &installed)?;
            tar.into_inner()?.finish()?;
            assert!(runtime_tree(&archive, &installed)?);
            let extra = packages.join("injected.pth");
            fs::write(&extra, "import unexpected")?;
            assert!(!runtime_tree(&archive, &installed)?);
            fs::remove_file(extra)?;
            fs::remove_file(bin.join("python3"))?;
            symlink("../../runner.tar.gz", bin.join("python3"))?;
            assert!(!runtime_tree(&archive, &installed)?);
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
