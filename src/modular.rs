//! Installs and launches the packaged modular speech-to-speech runtime.

mod chat;
mod integrity;

use crate::{
    assets,
    profiles::{self, Download},
    progress::{self, Step},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[derive(Deserialize)]
struct Runner {
    url: Option<String>,
    size: u64,
    sha256: String,
    /// Digest of the installed tree, so each start checks it without re-reading the archive.
    tree_sha256: String,
    #[serde(flatten)]
    host: Host,
}

/// The computer a runner was built for, with the minimum versions its build recorded.
#[derive(Deserialize)]
#[serde(untagged)]
enum Host {
    Mac {
        min_macos: String,
    },
    /// x86-64 Linux with an NVIDIA driver that supports the runner's CUDA libraries.
    Cuda {
        min_nvidia_driver: String,
        min_glibc: String,
    },
}

impl Host {
    /// The script beside the runner's interpreter that serves the conversation.
    fn entry(&self) -> &'static str {
        match self {
            Self::Mac { .. } => "voice.py",
            Self::Cuda { .. } => "cuda.py",
        }
    }
}

impl Runner {
    fn archive(&self) -> Download<'_> {
        Download {
            name: "runner.tar.gz",
            url: self.url.as_deref().unwrap_or_default(),
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
        "This build does not include a modular runtime manifest. Use a Boubou package with the modular runtime.",
    )?;
    let runner: Runner =
        serde_json::from_slice(&bytes).context("Invalid packaged modular runtime manifest")?;
    let digest = |text: &str| text.len() == 64 && text.bytes().all(|c| c.is_ascii_hexdigit());
    ensure!(
        runner
            .url
            .as_ref()
            .is_none_or(|url| url.starts_with("https://"))
            && digest(&runner.sha256)
            && digest(&runner.tree_sha256)
            && runner.size > 0,
        "Invalid packaged modular runtime manifest"
    );
    Ok(runner)
}

fn version(text: &str) -> Result<(u32, u32, u32)> {
    let mut parts = text.trim().split('.');
    let major = parts.next().context("Missing version number")?.parse()?;
    let minor = parts.next().unwrap_or("0").parse()?;
    let patch = parts.next().unwrap_or("0").parse()?;
    Ok((major, minor, patch))
}

pub fn macos_version() -> Result<(u32, u32, u32)> {
    let os = Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()?;
    ensure!(
        os.status.success(),
        "Cannot determine the installed macOS version"
    );
    version(std::str::from_utf8(&os.stdout)?)
}

/// The loaded kernel module's version, such as 595.84 in
/// "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  595.84  Release Build ...".
fn nvidia_driver(text: &str) -> Option<&str> {
    text.split_whitespace()
        .find(|word| word.contains('.') && word.bytes().all(|c| c.is_ascii_digit() || c == b'.'))
}

#[cfg(target_os = "linux")]
fn glibc_version() -> Result<(u32, u32, u32)> {
    let text = unsafe { std::ffi::CStr::from_ptr(libc::gnu_get_libc_version()) };
    version(text.to_str()?)
}

#[cfg(not(target_os = "linux"))]
fn glibc_version() -> Result<(u32, u32, u32)> {
    bail!("glibc is only available on Linux")
}

// Runs before any download, so an unsupported computer does not fetch gigabytes first.
fn check_host(runner: &Runner) -> Result<()> {
    match &runner.host {
        Host::Mac { min_macos } => {
            ensure!(
                cfg!(all(target_os = "macos", target_arch = "aarch64")),
                "The modular runtime requires an Apple Silicon Mac."
            );
            ensure!(
                macos_version()? >= version(min_macos)?,
                "This modular runtime requires macOS {min_macos} or later."
            );
        }
        Host::Cuda {
            min_nvidia_driver,
            min_glibc,
        } => {
            ensure!(
                cfg!(all(target_os = "linux", target_arch = "x86_64")),
                "This modular runtime requires x86-64 Linux with an NVIDIA GPU."
            );
            ensure!(
                glibc_version()? >= version(min_glibc)?,
                "This modular runtime requires glibc {min_glibc} or later, for example Ubuntu 24.04."
            );
            let text = fs::read_to_string("/proc/driver/nvidia/version")
                .context("The modular runtime on Linux requires an NVIDIA GPU, and no NVIDIA driver is loaded.")?;
            let driver = nvidia_driver(&text).context("Cannot read the NVIDIA driver version")?;
            ensure!(
                version(driver)? >= version(min_nvidia_driver)?,
                "The modular runtime requires NVIDIA driver {min_nvidia_driver} or later, but {driver} is loaded. Update the driver and retry."
            );
        }
    }
    Ok(())
}

fn runtime_dir(root: &Path, runner: &Runner) -> Result<PathBuf> {
    let relative = Path::new("runtimes").join(&runner.sha256);
    integrity::safe_destination(root, &relative.join("runner.tar.gz"))?;
    Ok(root.join(relative))
}

/// The verified runner archive: downloaded into the cache, or bundled beside the binary.
fn archive(runner: &Runner, dir: &Path, engine: &Path, offline: bool) -> Result<PathBuf> {
    if runner.url.is_some() {
        return assets::fetch(&runner.archive(), engine, offline);
    }
    let archive = dir.join("runner.tar.gz");
    ensure!(
        assets::verified(&archive, &runner.archive())?,
        "The bundled Python interpreter failed verification. Reinstall the package."
    );
    Ok(archive)
}

fn tree_matches(installed: &Path, tree: &str) -> Result<bool> {
    Ok(integrity::tree_digest(installed)?.as_deref() == Some(tree))
}

pub fn prepare(id: &str, cache: &Path, offline: bool) -> Result<PathBuf> {
    let dir = release_dir()?;
    let runner = runner(&dir)?;
    check_host(&runner)?;
    let root = cache.join("qwen");
    integrity::safe_destination(cache, Path::new("qwen/install.lock"))?;
    let _lock = assets::preparation_lock(&root)?;
    let engine = runtime_dir(&root, &runner)?;
    let installed = engine.join("installed");
    // The tree digest covers every installed file, so the archive is needed only to install.
    let current = tree_matches(&installed, &runner.tree_sha256)?;
    // Download everything first, then verify everything, so startup reads as two clear steps.
    progress::step(Step::Download);
    if !current && runner.url.is_some() {
        assets::prefetch(&runner.archive(), &engine, offline)?;
    }
    for f in model_files(id, &dir)?.iter().filter(|f| f.applies_to(id)) {
        integrity::safe_destination(&root, &f.path)?;
        let path = root.join(&f.path);
        assets::prefetch(
            &f.download()?,
            path.parent().context("Missing asset directory")?,
            offline,
        )?;
    }
    progress::step(Step::Verify);
    let archive = if current {
        None
    } else {
        Some(archive(&runner, &dir, &engine, offline)?)
    };
    model_assets(id, &root, &dir, Some(offline))?;
    progress::step(Step::Start);
    let Some(archive) = archive else {
        return Ok(installed);
    };
    assets::install_runtime(&archive, &installed, true, |staging| {
        tree_matches(staging, &runner.tree_sha256)
    })?;
    if runner.url.is_some() {
        fs::remove_file(&archive)?;
    }
    Ok(installed)
}

/// The checklist's download chips: speech-to-text, language model, voice and runner.
pub fn parts(id: &str, cache: &Path) -> Result<Vec<progress::Part>> {
    let dir = release_dir()?;
    let runner = runner(&dir)?;
    let root = cache.join("qwen");
    let preset = profiles::presets().iter().find(|preset| preset.id == id);
    let name = |pick: fn(&profiles::Preset) -> &'static str, fallback: &str| {
        preset.map_or_else(|| fallback.to_owned(), |preset| pick(preset).to_owned())
    };
    let mut parts = vec![
        progress::Part {
            name: name(|p| p.stt, "Speech-to-text"),
            files: Vec::new(),
        },
        progress::Part {
            name: name(|p| p.llm, "Language model"),
            files: Vec::new(),
        },
        progress::Part {
            name: name(|p| p.tts, "Voice"),
            files: Vec::new(),
        },
        progress::Part {
            name: "Speech engine".into(),
            files: Vec::new(),
        },
    ];
    let engine = runtime_dir(&root, &runner)?;
    // An installed runner has no archive left to download or verify.
    if !engine.join("installed").is_dir() {
        let archive = if runner.url.is_some() { &engine } else { &dir }.join("runner.tar.gz");
        parts[3].files.push((archive, runner.size));
    }
    for f in model_files(id, &dir)?.iter().filter(|f| f.applies_to(id)) {
        let path = f.path.to_string_lossy().to_lowercase();
        let part = if path.contains("parakeet") || path.contains("whisper") {
            0
        } else if path.contains("qwen3.5") || path.contains("qwen3_5") {
            1
        } else {
            2
        };
        parts[part].files.push((root.join(&f.path), f.size));
    }
    parts.retain(|part| !part.files.is_empty());
    Ok(parts)
}

/// Whisper entries recognize Chinese and English; the others use Parakeet.
pub fn whisper(id: &str) -> bool {
    id.ends_with("-whisper") || id == "chattts:whisper"
}

/// Language modes are explicit; Pocket profiles name their stock language.
fn language(id: &str) -> String {
    if whisper(id) {
        return "Chinese / English (Whisper)".into();
    }
    let Some(config) = id.strip_prefix("pocket-tts:") else {
        return "English".into();
    };
    let mut name = config.split('_').next().unwrap_or(config).chars();
    name.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(name).collect()
    })
}

pub fn doctor(id: &str, cache: &Path) -> Result<()> {
    let dir = release_dir()?;
    println!(
        "Profile: {id} · {} voice chat · speech-to-speech",
        language(id)
    );
    let runner = runner(&dir)?;
    check_host(&runner)?;
    let root = cache.join("qwen");
    let engine = runtime_dir(&root, &runner)?;
    let ready = tree_matches(&engine.join("installed"), &runner.tree_sha256)?;
    println!(
        "Modular runtime: {}",
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
    #[serde(default)]
    profiles: Vec<String>,
}

impl ModelFile {
    fn download(&self) -> Result<Download<'_>> {
        Ok(Download {
            name: self
                .path
                .file_name()
                .and_then(|s| s.to_str())
                .context("Invalid asset name")?,
            url: &self.url,
            size: self.size,
            sha256: &self.sha256,
        })
    }

    fn applies_to(&self, id: &str) -> bool {
        self.profile.as_deref().is_none_or(|profile| profile == id)
            && (self.profiles.is_empty() || self.profiles.iter().any(|profile| profile == id))
    }
}

fn model_files(id: &str, dir: &Path) -> Result<Vec<ModelFile>> {
    let files: Vec<ModelFile> = serde_json::from_slice(&fs::read(dir.join("models.json"))?)?;
    ensure!(
        files.iter().any(|f| {
            f.profile.as_deref() == Some(id) || f.profiles.iter().any(|profile| profile == id)
        }),
        "Missing modular model profile {id}"
    );
    Ok(files)
}

pub struct Requirements {
    pub min_macos: (u32, u32, u32),
    pub download: u64,
    pub disk: u64,
}

pub fn requirements(id: &str, cache: &Path) -> Result<Requirements> {
    estimate(id, cache, &release_dir()?)
}

// Estimates use sizes only. Preparation still verifies every selected file by SHA-256.
fn estimate(id: &str, cache: &Path, dir: &Path) -> Result<Requirements> {
    let runner = runner(dir)?;
    let Host::Mac { min_macos } = &runner.host else {
        bail!("Guided startup requires the macOS modular runtime.");
    };
    let files = model_files(id, dir)?;
    let root = cache.join("qwen");
    let mut download = 0;
    for file in files.iter().filter(|file| file.applies_to(id)) {
        integrity::safe_destination(&root, &file.path)?;
        if !root
            .join(&file.path)
            .metadata()
            .is_ok_and(|m| m.is_file() && m.len() == file.size)
        {
            download += file.size;
        }
    }
    let engine = runtime_dir(&root, &runner)?;
    // An installed runtime no longer keeps its archive. A corrupt one may still need
    // repair; preparation remains authoritative.
    let installed = engine.join("installed/bin/python3").is_file();
    if runner.url.is_some()
        && !installed
        && !engine
            .join("runner.tar.gz")
            .metadata()
            .is_ok_and(|m| m.is_file() && m.len() == runner.size)
    {
        download += runner.size;
    }
    // Allow unpacking and staging when the interpreter is not installed.
    let disk = download + if installed { 0 } else { 8 * runner.size };
    Ok(Requirements {
        min_macos: version(min_macos)?,
        download,
        disk,
    })
}

// None checks the cache in place. Some(offline) performs verified preparation.
fn model_assets(id: &str, root: &Path, dir: &Path, prepare: Option<bool>) -> Result<()> {
    let files = model_files(id, dir)?;
    let mut ready = true;
    let mut selected = Vec::new();
    let mut specs = Vec::new();
    for f in files.iter().filter(|f| f.applies_to(id)) {
        integrity::safe_destination(root, &f.path)?;
        selected.push(f.path.as_path());
        specs.push((root.join(&f.path), f.download()?));
    }
    // Check the cache on several threads first; only missing or damaged files are fetched.
    let verified = assets::verified_all(&specs)?;
    for ((path, spec), valid) in specs.iter().zip(verified) {
        if valid {
            continue;
        }
        if let Some(offline) = prepare {
            assets::fetch(
                spec,
                path.parent().context("Missing asset directory")?,
                offline,
            )?;
        } else {
            println!("Missing or invalid: {}", path.strip_prefix(root)?.display());
            ready = false;
        }
    }
    ensure!(ready, "Run `boubou pull {id}` while online.");
    let listed: Vec<&Path> = files.iter().map(|f| f.path.as_path()).collect();
    integrity::model_snapshots(root, &listed, &selected)
}

/// Talks through this computer's audio devices, or through a browser page served at `door`.
pub fn run(
    id: &str,
    cache: &Path,
    offline: bool,
    door: Option<&crate::serve::Options>,
    pipeline: &[String],
) -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let installed = prepare(id, cache, offline)?;
    let entry = runner(&release_dir()?)?.host.entry();
    // Pipeline options may name any model, so only they let the runner reach the Hub.
    let hub = !(offline || pipeline.is_empty());
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))?;
    let mut command = Command::new(installed.join("bin/python3"));
    command
        .args(["-I", "-B"])
        .arg(installed.join(entry))
        .arg(id)
        .arg(cache.join("qwen"))
        .arg(release_dir()?.join("models.json"))
        // Later options replace earlier ones, so these replace the model's own.
        .args(pipeline)
        .env("HF_HUB_OFFLINE", if hub { "0" } else { "1" });
    let (mut server, mut session) = {
        let _progress = crate::progress::Progress::new(&format!("Loading {id} locally"), None);
        // A model that the Hub client fetches first can take long to arrive.
        chat::Server::start(
            command,
            Duration::from_secs(if hub { 3600 } else { 300 }),
            &stopped,
        )?
    };
    let result = match door {
        Some(door) => {
            // Starting proved the runner ready; its one conversation goes to whoever visits.
            drop(session);
            chat::from_browser(&mut server, door, cache, &stopped)
        }
        None => chat::microphone(&mut server, &mut session, cache, &stopped, &language(id)),
    };
    crate::check_cancel(&stopped)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_manifests_name_their_host() {
        let manifest = |host: &str| {
            serde_json::from_str::<Runner>(&format!(
                r#"{{"url":null,"size":1,"sha256":"{0}","tree_sha256":"{0}"{host}}}"#,
                "a".repeat(64)
            ))
        };
        let mac = manifest(r#","min_macos":"15.0""#).unwrap();
        assert_eq!(mac.host.entry(), "voice.py");
        let cuda =
            manifest(r#","sources_sha256":"c","min_nvidia_driver":"580.65.06","min_glibc":"2.39""#)
                .unwrap();
        assert_eq!(cuda.host.entry(), "cuda.py");
        assert!(manifest("").is_err());
        // A runner built for the other system stops before anything is downloaded.
        let foreign = if cfg!(target_os = "macos") { cuda } else { mac };
        assert!(check_host(&foreign).is_err());
    }

    #[test]
    fn nvidia_driver_version_comes_from_the_loaded_kernel_module() {
        let text = "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  595.84  Release Build  (dvs-builder@U22-I3-AM02-24-3)  Thu Oct 30 2026\nGCC version:  gcc version 13.3.0 (Ubuntu 13.3.0-6ubuntu2~24.04)\n";
        assert_eq!(nvidia_driver(text), Some("595.84"));
        assert!(version("595.84").unwrap() >= version("580.65.06").unwrap());
        assert!(version("575.57.08").unwrap() < version("580.65.06").unwrap());
        assert_eq!(nvidia_driver("NVRM version: unavailable"), None);
    }

    #[test]
    fn asset_ownership_accepts_legacy_and_grouped_profiles() {
        let file = |ownership: &str| {
            serde_json::from_str::<ModelFile>(&format!(
                r#"{{"path":"hub/config.json","url":"https://example.invalid/config.json","size":1,"sha256":"digest"{ownership}}}"#
            )).unwrap()
        };
        assert!(file("").applies_to("chattts:whisper"));
        assert!(file(r#", "profile":"chattts:en""#).applies_to("chattts:en"));
        assert!(!file(r#", "profile":"chattts:en""#).applies_to("chattts:whisper"));
        let grouped =
            file(r#", "profiles":["chattts:whisper","qwen3-tts:0.6b-customvoice-whisper"]"#);
        assert!(grouped.applies_to("chattts:whisper"));
        assert!(grouped.applies_to("qwen3-tts:0.6b-customvoice-whisper"));
        assert!(!grouped.applies_to("chattts:en"));
    }

    #[test]
    fn resource_estimates_select_only_the_requested_pipeline_without_downloading() -> Result<()> {
        let root = env::temp_dir().join(format!("boubou-modular-estimate-{}", std::process::id()));
        fs::create_dir(&root)?;
        let result = (|| -> Result<()> {
            let cache = root.join("cache");
            let digest = "a".repeat(64);
            let manifest = |url| serde_json::json!({"url": url, "size": 100, "sha256": digest, "tree_sha256": digest, "min_macos": "15.0"});
            fs::write(
                root.join("runtime.json"),
                manifest(serde_json::Value::Null).to_string(),
            )?;
            fs::write(root.join("models.json"), serde_json::json!([
                {"path":"shared", "url":"https://example.invalid/shared", "size":200, "sha256":digest},
                {"path":"whisper", "url":"https://example.invalid/whisper", "size":300, "sha256":digest, "profiles":["chinese"]},
                {"path":"parakeet", "url":"https://example.invalid/parakeet", "size":400, "sha256":digest, "profile":"english"}
            ]).to_string())?;
            let chinese = estimate("chinese", &cache, &root)?;
            assert_eq!(chinese.min_macos, (15, 0, 0));
            assert_eq!(chinese.download, 500);
            assert_eq!(chinese.disk, 1300);
            assert!(
                !cache.exists(),
                "Estimation must not create or download assets"
            );
            assert!(estimate("missing", &cache, &root).is_err());
            fs::create_dir_all(cache.join("qwen"))?;
            fs::write(cache.join("qwen/shared"), vec![0; 200])?;
            assert_eq!(estimate("chinese", &cache, &root)?.download, 300);
            assert_eq!(estimate("english", &cache, &root)?.download, 400);
            fs::write(
                root.join("runtime.json"),
                manifest(serde_json::json!("https://example.invalid/runner.tar.gz")).to_string(),
            )?;
            let remote = estimate("chinese", &cache, &root)?;
            assert_eq!(remote.download, 400);
            assert_eq!(remote.disk, 1200);
            fs::remove_file(root.join("runtime.json"))?;
            assert!(estimate("chinese", &cache, &root).is_err());
            Ok(())
        })();
        fs::remove_dir_all(root)?;
        result
    }
}
