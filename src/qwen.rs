mod integrity;

use crate::{assets, profiles::Download};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Deserialize)]
struct Runner {
    url: String,
    size: u64,
    sha256: String,
    min_macos: String,
}

impl Runner {
    fn archive(&self) -> Download<'_> {
        Download {
            name: "runner.tar.gz",
            url: &self.url,
            size: self.size,
            sha256: &self.sha256,
        }
    }
}

fn release_dir() -> Result<PathBuf> {
    Ok(env::current_exe()?
        .parent()
        .context("Cannot locate the Boubou package")?
        .join("qwen"))
}

pub fn packaged() -> bool {
    release_dir()
        .is_ok_and(|dir| dir.join("runtime.json").is_file() && dir.join("models.json").is_file())
}

fn runner(dir: &Path) -> Result<Runner> {
    let bytes = fs::read(dir.join("runtime.json")).context(
        "This build does not include a Qwen runner manifest. Use a Qwen-enabled Boubou package.",
    )?;
    let runner: Runner = serde_json::from_slice(&bytes)?;
    ensure!(
        runner.url.starts_with("https://")
            && runner.sha256.len() == 64
            && runner.sha256.bytes().all(|c| c.is_ascii_hexdigit())
            && runner.size > 0,
        "Invalid packaged Qwen runner manifest"
    );
    Ok(runner)
}

fn version(text: &str) -> Result<(u32, u32, u32)> {
    let mut parts = text.trim().split('.');
    let major = parts.next().context("Missing macOS version")?.parse()?;
    let minor = parts.next().unwrap_or("0").parse()?;
    let patch = parts.next().unwrap_or("0").parse()?;
    Ok((major, minor, patch))
}

fn check_host(runner: &Runner) -> Result<()> {
    ensure!(
        cfg!(all(target_os = "macos", target_arch = "aarch64")),
        "Qwen requires an Apple Silicon Mac."
    );
    let os = Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()?;
    ensure!(
        os.status.success(),
        "Cannot determine the installed macOS version"
    );
    ensure!(
        version(std::str::from_utf8(&os.stdout)?)? >= version(&runner.min_macos)?,
        "This Qwen runner requires macOS {} or later.",
        runner.min_macos
    );
    Ok(())
}

fn runtime_dir(root: &Path, runner: &Runner) -> Result<PathBuf> {
    let relative = Path::new("runtimes").join(&runner.sha256);
    integrity::safe_destination(root, &relative.join("runner.tar.gz"))?;
    Ok(root.join(relative))
}

pub fn prepare(id: &str, cache: &Path, offline: bool) -> Result<PathBuf> {
    let dir = release_dir()?;
    let runner = runner(&dir)?;
    check_host(&runner)?;
    let root = cache.join("qwen");
    integrity::safe_destination(cache, Path::new("qwen/install.lock"))?;
    let _lock = assets::preparation_lock(&root)?;
    let engine = runtime_dir(&root, &runner)?;
    let archive = assets::fetch(&runner.archive(), &engine, offline)?;
    let installed = engine.join("installed");
    let refresh = !integrity::runtime_tree(&archive, &installed)?;
    assets::install_runtime(&archive, &installed, "bin/python3", refresh)?;
    if refresh {
        ensure!(
            integrity::runtime_tree(&archive, &installed)?,
            "The Qwen runtime contains unexpected files"
        );
    }
    model_assets(id, &root, &dir, Some(offline))?;
    Ok(installed)
}

pub fn doctor(id: &str, cache: &Path) -> Result<()> {
    let dir = release_dir()?;
    println!("Profile: {id} · English voice chat · speech-to-speech");
    let runner = runner(&dir)?;
    check_host(&runner)?;
    let root = cache.join("qwen");
    let engine = runtime_dir(&root, &runner)?;
    let archive = engine.join("runner.tar.gz");
    let ready = assets::verified(&archive, &runner.archive())?
        && integrity::runtime_tree(&archive, &engine.join("installed"))?
        && assets::runtime_valid(&archive, &engine.join("installed"), "bin/python3")?;
    println!(
        "Qwen runner: {}",
        if ready {
            "verified"
        } else {
            "missing or incomplete"
        }
    );
    model_assets(id, &root, &dir, None)?;
    ensure!(ready, "Run `boubou pull {id}` while online.");
    Ok(())
}

#[derive(Deserialize)]
struct ModelFile {
    path: PathBuf,
    url: String,
    size: u64,
    sha256: String,
    #[serde(default)]
    profile: Option<String>,
}

// None checks the cache in place. Some(offline) performs verified preparation.
fn model_assets(id: &str, root: &Path, dir: &Path, prepare: Option<bool>) -> Result<()> {
    let files: Vec<ModelFile> = serde_json::from_slice(&fs::read(dir.join("models.json"))?)?;
    ensure!(
        files.iter().any(|f| f.profile.as_deref() == Some(id)),
        "Missing Qwen model profile {id}"
    );
    let mut ready = true;
    let mut selected = Vec::new();
    for f in files
        .iter()
        .filter(|f| f.profile.as_deref().is_none_or(|p| p == id))
    {
        integrity::safe_destination(root, &f.path)?;
        selected.push(f.path.as_path());
        let path = root.join(&f.path);
        let spec = Download {
            name: path
                .file_name()
                .and_then(|s| s.to_str())
                .context("Invalid asset name")?,
            url: &f.url,
            size: f.size,
            sha256: &f.sha256,
        };
        if let Some(offline) = prepare {
            assets::fetch(
                &spec,
                path.parent().context("Missing asset directory")?,
                offline,
            )?;
        } else if !assets::verified(&path, &spec)? {
            println!("Missing or invalid: {}", f.path.display());
            ready = false;
        }
    }
    ensure!(ready, "Run `boubou pull {id}` while online.");
    integrity::model_snapshots(root, &selected)
}

pub fn run(id: &str, cache: &Path, offline: bool) -> Result<()> {
    let installed = prepare(id, cache, offline)?;
    let mut command = Command::new(installed.join("bin/python3"));
    command
        .args(["-I", "-B"])
        .arg(installed.join("voice.py"))
        .arg(id)
        .arg(cache.join("qwen"))
        .arg(release_dir()?.join("models.json"));
    // Keep the npm launcher's PID/signals and the terminal's foreground process group.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("Cannot start the Qwen voice runner")
    }
    #[cfg(not(unix))]
    anyhow::bail!("Qwen requires an Apple Silicon Mac.")
}
