#[cfg(any(target_os = "macos", test))]
mod chat;
mod integrity;

use crate::{
    assets,
    profiles::{self, Download},
    progress::{self, Step},
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Deserialize)]
struct Runner {
    url: Option<String>,
    size: u64,
    sha256: String,
    min_macos: String,
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
        "This build does not include a Qwen runner manifest. Use a Qwen-enabled Boubou package.",
    )?;
    let runner: Runner = serde_json::from_slice(&bytes)?;
    ensure!(
        runner
            .url
            .as_ref()
            .is_none_or(|url| url.starts_with("https://"))
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

fn check_host(runner: &Runner) -> Result<()> {
    ensure!(
        cfg!(all(target_os = "macos", target_arch = "aarch64")),
        "Qwen requires an Apple Silicon Mac."
    );
    ensure!(
        macos_version()? >= version(&runner.min_macos)?,
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
    // Download everything first, then verify everything, so startup reads as two clear steps.
    progress::step(Step::Download);
    if runner.url.is_some() {
        assets::prefetch(&runner.archive(), &engine, offline)?;
    }
    let files = model_files(id, &dir)?;
    for f in files.iter().filter(|f| f.applies_to(id)) {
        integrity::safe_destination(&root, &f.path)?;
        let path = root.join(&f.path);
        assets::prefetch(
            &f.download(&path)?,
            path.parent().context("Missing asset directory")?,
            offline,
        )?;
    }
    progress::step(Step::Verify);
    let archive = if runner.url.is_some() {
        assets::fetch(&runner.archive(), &engine, offline)?
    } else {
        let archive = dir.join("runner.tar.gz");
        ensure!(
            assets::verified(&archive, &runner.archive())?,
            "The bundled Qwen interpreter failed verification. Reinstall the package."
        );
        archive
    };
    model_assets(id, &root, &dir, Some(offline))?;
    progress::step(Step::Start);
    let installed = engine.join("installed");
    let refresh = !integrity::runtime_tree(&archive, &installed)?;
    assets::install_runtime(&archive, &installed, "bin/python3", refresh)?;
    if refresh {
        ensure!(
            integrity::runtime_tree(&archive, &installed)?,
            "The Qwen runtime contains unexpected files"
        );
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
            files: vec![(
                if runner.url.is_some() {
                    runtime_dir(&root, &runner)?
                } else {
                    dir.clone()
                }
                .join("runner.tar.gz"),
                runner.size,
            )],
        },
    ];
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

/// Language modes are explicit; Pocket profiles name their stock language.
fn language(id: &str) -> String {
    if id.ends_with("-whisper") || id == "chattts:whisper" {
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
    let archive = if runner.url.is_some() { &engine } else { &dir }.join("runner.tar.gz");
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
    #[serde(default)]
    profiles: Vec<String>,
}

impl ModelFile {
    fn download<'a>(&'a self, path: &'a Path) -> Result<Download<'a>> {
        Ok(Download {
            name: path
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
        "Missing Qwen model profile {id}"
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
    if runner.url.is_some()
        && !engine
            .join("runner.tar.gz")
            .metadata()
            .is_ok_and(|m| m.is_file() && m.len() == runner.size)
    {
        download += runner.size;
    }
    // Allow unpacking and staging when the interpreter is not installed. A corrupt
    // cached runtime may still need repair; preparation remains authoritative.
    let disk = download
        + if engine.join("installed/bin/python3").is_file() {
            0
        } else {
            8 * runner.size
        };
    Ok(Requirements {
        min_macos: version(&runner.min_macos)?,
        download,
        disk,
    })
}

// None checks the cache in place. Some(offline) performs verified preparation.
fn model_assets(id: &str, root: &Path, dir: &Path, prepare: Option<bool>) -> Result<()> {
    let files = model_files(id, dir)?;
    let mut ready = true;
    let mut selected = Vec::new();
    for f in files.iter().filter(|f| f.applies_to(id)) {
        integrity::safe_destination(root, &f.path)?;
        selected.push(f.path.as_path());
        let path = root.join(&f.path);
        let spec = f.download(&path)?;
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
    let listed: Vec<&Path> = files.iter().map(|f| f.path.as_path()).collect();
    integrity::model_snapshots(root, &listed, &selected)
}

#[cfg(target_os = "macos")]
pub fn run(id: &str, cache: &Path, offline: bool) -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let installed = prepare(id, cache, offline)?;
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))?;
    let mut command = Command::new(installed.join("bin/python3"));
    command
        .args(["-I", "-B"])
        .arg(installed.join("voice.py"))
        .arg(id)
        .arg(cache.join("qwen"))
        .arg(release_dir()?.join("models.json"));
    let (mut server, mut session) = {
        let _progress = crate::progress::Progress::new(&format!("Loading {id} locally"), None);
        chat::Server::start(command, &stopped)?
    };
    let result = chat::converse(&mut server, &mut session, cache, &stopped, &language(id));
    crate::check_cancel(&stopped)?;
    result
}

#[cfg(not(target_os = "macos"))]
pub fn run(id: &str, cache: &Path, offline: bool) -> Result<()> {
    prepare(id, cache, offline)?;
    anyhow::bail!("Qwen requires an Apple Silicon Mac.")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let root = env::temp_dir().join(format!("boubou-qwen-estimate-{}", std::process::id()));
        fs::create_dir(&root)?;
        let result = (|| -> Result<()> {
            let cache = root.join("cache");
            let digest = "a".repeat(64);
            let manifest = |url| serde_json::json!({"url": url, "size": 100, "sha256": digest, "min_macos": "15.0"});
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
