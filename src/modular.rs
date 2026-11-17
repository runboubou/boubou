//! Installs and launches the packaged modular speech-to-speech runtime.

mod chat;
mod incremental;
mod integrity;

use crate::{
    assets, library,
    profiles::{self, Download},
    progress::{self, Step},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::json;
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
    #[serde(default)]
    entries: Vec<incremental::Entry>,
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
            Self::Mac { .. } => "mac.py",
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
    incremental::validate(&runner.entries, runner.size)?;
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

pub fn prepare(
    id: &str,
    voice: Option<&profiles::Voice>,
    cache: &Path,
    offline: bool,
) -> Result<PathBuf> {
    let dir = release_dir()?;
    let runner = runner(&dir)?;
    check_host(&runner)?;
    let root = cache.join("qwen");
    integrity::safe_destination(cache, Path::new("qwen/install.lock"))?;
    let _lock = assets::preparation_lock(&root)?;
    let engine = runtime_dir(&root, &runner)?;
    let installed = engine.join("installed");
    // The tree digest covers every installed file, so the archive is needed only to install.
    let mut current = tree_matches(&installed, &runner.tree_sha256)?;
    // Download everything first, then verify everything, so startup reads as two clear steps.
    progress::step(Step::Download);
    if !current {
        current = incremental::install(&runner, &root, &installed, offline);
    }
    if !current && runner.url.is_some() {
        assets::prefetch(&runner.archive(), &engine, offline)?;
    }
    for f in needed(id, voice, cache, &dir)? {
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
    for id in owners(id, voice, cache) {
        model_assets(&id, &root, &dir, Some(offline))?;
    }
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
pub fn parts(
    id: &str,
    voice: Option<&profiles::Voice>,
    cache: &Path,
) -> Result<Vec<progress::Part>> {
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
        let size = incremental::estimate(&runner, &root, &engine.join("installed"));
        parts[3].files.push((archive, size.unwrap_or(runner.size)));
    }
    for f in needed(id, voice, cache, &dir)? {
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

/// Whisper entries hear languages Parakeet does not, such as Chinese; the others use Parakeet.
pub fn whisper(id: &str) -> bool {
    id.ends_with("-whisper") || id == "chattts:whisper"
}

/// Language modes are explicit; Pocket profiles name their stock language.
fn language(id: &str) -> String {
    if whisper(id) {
        // Kokoro's Whisper entry speaks any of its languages; the others were tested in these two.
        let kokoro = id.starts_with("kokoro:");
        return if kokoro {
            "Multilingual (Whisper)"
        } else {
            "Chinese / English (Whisper)"
        }
        .into();
    }
    let Some(config) = id.strip_prefix("pocket-tts:") else {
        return "English".into();
    };
    let mut name = config.split('_').next().unwrap_or(config).chars();
    name.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(name).collect()
    })
}

/// The languages of the voices a voice list names, by the codes it gives them: the ten Qwen3-TTS
/// speaks, then the twenty more VoxCPM2 reads cloned voices in, Dutch and Hindi first.
/// Supertonic's are `profiles::SUPERTONIC_LANGUAGES`.
pub const LANGUAGES: [(&str, &str); 30] = [
    ("en", "English"),
    ("ja", "Japanese"),
    ("zh", "Chinese"),
    ("ko", "Korean"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("de", "German"),
    ("pt", "Portuguese"),
    ("it", "Italian"),
    ("ru", "Russian"),
    ("nl", "Dutch"),
    ("hi", "Hindi"),
    ("ar", "Arabic"),
    ("my", "Burmese"),
    ("da", "Danish"),
    ("fi", "Finnish"),
    ("el", "Greek"),
    ("he", "Hebrew"),
    ("id", "Indonesian"),
    ("km", "Khmer"),
    ("lo", "Lao"),
    ("ms", "Malay"),
    ("no", "Norwegian"),
    ("pl", "Polish"),
    ("sw", "Swahili"),
    ("sv", "Swedish"),
    ("tl", "Tagalog"),
    ("th", "Thai"),
    ("tr", "Turkish"),
    ("vi", "Vietnamese"),
];

/// VoxCPM2's weights, which read the voices cloned in languages Qwen3-TTS does not speak, from
/// the recording and its words. A Qwen3-TTS conversation hands it those voices' sentences.
pub const VOXCPM: &str = "voxcpm2:2b";

/// Whether Qwen3-TTS reads a voice cloned in the language `speaks`, one of its ten; VoxCPM2
/// reads the others.
pub fn qwen_reads(speaks: &str) -> bool {
    LANGUAGES[..10].iter().any(|(_, title)| *title == speaks)
}

/// The voices cloned on this computer, by name, each with the language it speaks. A recording
/// renamed in the folder is found by its new name.
pub fn clones(cache: &Path) -> Vec<profiles::Voice> {
    let folder = library::folder(cache);
    // A folder Boubou cannot write to still lists the voices it holds.
    let _ = library::tidy(&folder);
    let voices = library::scan(&folder).voices;
    voices
        .iter()
        .map(|voice| (voice.name.clone(), voice.speaks()))
        .collect()
}

/// The Base weights of a Qwen3-TTS entry's size, which clone. CustomVoice weights cannot.
fn base(id: &str) -> Option<&'static str> {
    match id.get(..14)? {
        "qwen3-tts:0.6b" => Some("qwen3-tts:0.6b-base"),
        "qwen3-tts:1.7b" => Some("qwen3-tts:1.7b-base"),
        _ => None,
    }
}

/// The Base weights that read the voices cloned in `cache`, for a Qwen3-TTS entry, once one
/// speaks a language Qwen3-TTS does, and for `boubou clone`, those of the size this computer
/// clones with.
fn cloner(id: &str, cache: &Path) -> Option<&'static str> {
    if id == CLONE {
        return base(library::model());
    }
    let reads = || clones(cache).iter().any(|(_, speaks)| qwen_reads(speaks));
    base(id).filter(|_| reads())
}

/// VoxCPM2, for a Qwen3-TTS entry or `boubou clone`, once a voice cloned in `cache` speaks a
/// language Qwen3-TTS does not. The clone page fetches it when such a voice first speaks.
fn voxcpm(id: &str, cache: &Path) -> Option<&'static str> {
    let qwen = id == CLONE || base(id).is_some();
    let other = || clones(cache).iter().any(|(_, speaks)| !qwen_reads(speaks));
    (qwen && other()).then_some(VOXCPM)
}

/// What `GET /v1/audio/voices` answers for a model: the voices a text to read aloud can name,
/// first the one that reads when none is named. Qwen3-TTS has its speakers and the voices
/// `cloned` on this computer, each with the language Boubou talks with it in; every other model
/// has the one voice it starts with.
fn voices(id: &str, voice: Option<&profiles::Voice>, cloned: &[profiles::Voice]) -> String {
    let named = |name: &str, speaks: &[&str]| {
        let languages: Vec<_> = speaks
            .iter()
            .map(|&title| {
                let mut codes = LANGUAGES.iter().chain(&profiles::SUPERTONIC_LANGUAGES);
                let code = codes.find(|(_, speaks)| *speaks == title);
                json!({"code": code.map_or("en", |(code, _)| *code), "title": title})
            })
            .collect();
        json!({"id": name.to_lowercase(), "name": name, "languages": languages})
    };
    let voices: Vec<_> = if id.starts_with("qwen3-tts:") {
        // The speaker the runner starts this entry with, unless --voice names another.
        let own = voice.map_or(if whisper(id) { "Vivian" } else { "Aiden" }, |voice| {
            &voice.0
        });
        let speakers = profiles::VOICES
            .iter()
            .map(|&(name, speaks)| (name, speaks));
        let mut speakers: Vec<_> = speakers
            .chain(cloned.iter().map(|(name, speaks)| (name.as_str(), *speaks)))
            .collect();
        speakers.sort_by_key(|(name, _)| *name != own);
        speakers
            .iter()
            .map(|(name, speaks)| match voice {
                // The speaker that speaks first does so in the language --language named.
                Some((first, language)) if first == name => named(name, &[language]),
                _ => named(name, &[speaks]),
            })
            .collect()
    } else if let Some((name, speaks)) = voice {
        // The Kokoro, KittenTTS or Supertonic voice that --voice named.
        vec![named(name, &[speaks])]
    } else if id == "chattts:whisper" {
        vec![named("Default", &["Chinese", "English"])]
    } else {
        // Kokoro's Whisper entry starts as its Parakeet entry does, in English.
        let speaks = if whisper(id) {
            "English".into()
        } else {
            language(id)
        };
        vec![named("Default", &[&speaks])]
    };
    json!({ "voices": voices }).to_string()
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
    ensure!(
        ready,
        "Run `{} pull {id}` while online.",
        crate::update::program()
    );
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

/// The profiles whose files `id` reads with `voice`: its own, the Base weights that read voices
/// cloned in `cache`, and the voice state of a Pocket TTS preset it does not start with. The
/// runner reads one voice state per Pocket TTS profile, so each other preset has a profile of
/// its own, `<id>/<voice>`, and only the voice chosen is downloaded.
fn owners(id: &str, voice: Option<&profiles::Voice>, cache: &Path) -> Vec<String> {
    let preset = profiles::pocket(id)
        .zip(voice)
        .filter(|((_, own), (name, _))| own != name)
        .map(|(_, (name, _))| format!("{id}/{name}"));
    [id.to_owned()]
        .into_iter()
        .chain(cloner(id, cache).map(str::to_owned))
        .chain(voxcpm(id, cache).map(str::to_owned))
        .chain(preset)
        .collect()
}

/// The files `id` reads with `voice`.
fn needed(
    id: &str,
    voice: Option<&profiles::Voice>,
    cache: &Path,
    dir: &Path,
) -> Result<Vec<ModelFile>> {
    let ids = owners(id, voice, cache);
    let mut files = model_files(id, dir)?;
    files.retain(|f| ids.iter().any(|id| f.applies_to(id)));
    Ok(files)
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
    let root = cache.join("qwen");
    let mut download = 0;
    for file in needed(id, None, cache, dir)? {
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
    if let Some(offline) = prepare {
        assets::fetch_all(&specs, offline)?;
    } else {
        for ((path, _), valid) in specs.iter().zip(assets::verified_all(&specs)?) {
            if !valid {
                println!("Missing or invalid: {}", path.strip_prefix(root)?.display());
                ready = false;
            }
        }
    }
    ensure!(
        ready,
        "Run `{} pull {id}` while online.",
        crate::update::program()
    );
    let listed: Vec<&Path> = files.iter().map(|f| f.path.as_path()).collect();
    integrity::model_snapshots(root, &listed, &selected)
}

/// Talks through this computer's audio devices, or through a browser page served at `door`.
pub fn run(
    id: &str,
    cache: &Path,
    offline: bool,
    door: Option<&crate::serve::Options>,
    voice: Option<&crate::profiles::Voice>,
    pipeline: &[String],
) -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let installed = prepare(id, voice, cache, offline)?;
    let entry = runner(&release_dir()?)?.host.entry();
    // Pipeline options may name any model, so only they let the runner reach the Hub.
    let hub = !(offline || pipeline.is_empty());
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))?;
    let cloned = clones(cache);
    let mut command = Command::new(installed.join("bin/python3"));
    command
        .args(["-I", "-B"])
        .arg(installed.join(entry))
        .arg(id)
        .arg(cache.join("qwen"))
        .arg(release_dir()?.join("models.json"));
    // The runner reads cloned voices with the Base weights or VoxCPM2 that preparing verified.
    let (base, other) = (cloner(id, cache), voxcpm(id, cache));
    if base.is_some() || other.is_some() {
        command.env("BOUBOU_VOICES", library::folder(cache));
    }
    if let Some(base) = base {
        command.env("BOUBOU_CLONER", base);
    }
    if let Some(other) = other {
        command.env("BOUBOU_VOXCPM", other);
    }
    // Later options replace earlier ones: the voice replaces the model's own speaker, and
    // pipeline options replace both.
    if let Some(voice) = voice {
        if id.starts_with("qwen3-tts:") && cloned.iter().any(|(clone, _)| *clone == voice.0) {
            command.env("BOUBOU_VOICE", &voice.0);
        }
        command.args(speech(id, voice, &cloned));
    }
    command
        .args(pipeline)
        .env("HF_HUB_OFFLINE", if hub { "0" } else { "1" });
    let (mut server, mut session) = {
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
            chat::from_browser(
                &mut server,
                door,
                voices(id, voice, &cloned),
                cache,
                &stopped,
            )
        }
        None => chat::microphone(
            &mut server,
            &mut session,
            cache,
            &stopped,
            &voice.map_or_else(|| language(id), |(_, language)| (*language).into()),
        ),
    };
    crate::check_cancel(&stopped)?;
    result
}

/// The system prompt of upstream v1.1.0, which the runner's `voice.py` extends the same way.
const PROMPT: &str = "You are a helpful and friendly AI assistant. You are polite, respectful, \
                      and aim to provide concise responses of less than 20 words.";

/// The pipeline options that make `voice` speak for `id` in its language. The runner finds a
/// cloned voice by its name instead of a speaker option.
fn speech(id: &str, voice: &profiles::Voice, cloned: &[profiles::Voice]) -> Vec<String> {
    let (name, language) = (voice.0.as_str(), voice.1);
    let lowercase = language.to_lowercase();
    // Whether the language model is told the language, for a voice that speaks several.
    let told;
    let mut options = if id.starts_with("kokoro:") {
        told = false;
        // A Kokoro voice's first letter is its language code.
        vec!["--kokoro_voice", name, "--kokoro_lang_code", &name[..1]]
    } else if id.starts_with("kitten-tts:") {
        told = false;
        vec!["--kitten_voice", name]
    } else if id.starts_with("pocket-tts:") {
        // The SDK finds a preset's voice state for the profile's config in the pinned snapshot.
        // The profile's own prompt already names the config's language.
        told = false;
        vec!["--pocket_tts_voice", name]
    } else if id.starts_with("phonon:") {
        told = false;
        vec!["--phonon_tts_voice", name]
    } else if id.starts_with("supertonic:") {
        let code = profiles::SUPERTONIC_LANGUAGES
            .iter()
            .find(|(_, speaks)| *speaks == language)
            .map_or("en", |(code, _)| *code);
        // Upstream reads each reply in the language Parakeet heard, and in this one when it
        // could not tell.
        told = code != "en";
        vec![
            "--supertonic_tts_voice",
            name,
            "--supertonic_tts_lang",
            code,
        ]
    } else {
        // In its own language a speaker keeps its entry's way with languages; --language named
        // any other. VoxCPM2 reads a voice in a language Qwen3-TTS does not speak, so only the
        // language model is told it.
        let other = cloned.contains(voice) && !qwen_reads(language);
        told = other || !(profiles::VOICES.contains(&(name, language)) || cloned.contains(voice));
        let mut options = Vec::new();
        if !cloned.iter().any(|(clone, _)| clone == name) {
            options.extend(["--qwen3_tts_speaker", name]);
        }
        if told && !other {
            options.extend(["--qwen3_tts_language", &lowercase]);
        }
        options
    };
    let prompt = format!("{PROMPT} Always reply in {language}.");
    if told {
        options.extend(["--init_chat_prompt", &prompt]);
    }
    options.into_iter().map(String::from).collect()
}

/// What `boubou clone` prepares: Qwen3-TTS 0.6B Base weights, which clone, and Whisper, which
/// writes down what an uploaded clip says.
pub const CLONE: &str = "qwen3-tts:clone";

/// Opens the runner's page in a browser, where a person clones a voice into `voices`, until
/// Ctrl+C. The page lists the folder through `boubou voices`, with the commands for each voice.
pub fn clone(cache: &Path, voices: &Path) -> Result<()> {
    use std::{
        io::Read,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    library::ready(voices)?;
    let installed = prepare(CLONE, None, cache, false)?;
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst))?;
    // Only the browser Boubou opens holds the key to what the page does.
    let mut key = [0; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
    let key = assets::hex(&key);
    let mut command = Command::new(installed.join("bin/python3"));
    command
        .args(["-I", "-B"])
        .arg(installed.join("mac.py"))
        .args(["clone", CLONE])
        .arg(cache.join("qwen"))
        .arg(release_dir()?.join("models.json"))
        .env("BOUBOU_VOICES", voices)
        .env(
            "BOUBOU_CLONER",
            cloner(CLONE, cache).context("No Base weights clone here.")?,
        )
        // Fetched by `boubou voices --prepare` when a voice it reads first speaks.
        .env("BOUBOU_VOXCPM", VOXCPM)
        .env("BOUBOU_BIN", std::env::current_exe()?)
        .env("BOUBOU_KEY", &key);
    let (mut server, _) = chat::Server::launch(command, Duration::from_secs(300), &stopped)?;
    let link = format!("http://127.0.0.1:{}/#{key}", server.address.port());
    let opened = Command::new("open").arg(&link).status();
    let note = if opened.is_ok_and(|status| status.success()) {
        "Opened the voice page in your browser."
    } else {
        "Open the voice page in a browser:"
    };
    if !progress::finish(note) {
        eprintln!("{note}");
    }
    eprintln!("  {link}\n  Press Ctrl+C when you are done.");
    while !stopped.load(Ordering::SeqCst) {
        server.check()?;
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_model_lists_the_voices_a_text_can_name_with_its_own_first() {
        let cloned = [("mom".to_owned(), "Chinese")];
        let ids = |id, voice: Option<(&str, &'static str)>, cloned: &[profiles::Voice]| {
            let voice = voice.map(|(name, speaks)| (name.to_owned(), speaks));
            let listed = voices(id, voice.as_ref(), cloned);
            let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
            let voices = listed["voices"].as_array().unwrap().iter();
            let ids = voices.map(|voice| voice["id"].as_str().unwrap().to_owned());
            ids.collect::<Vec<_>>()
        };
        let small = "qwen3-tts:0.6b-customvoice";
        let whisper = "qwen3-tts:0.6b-customvoice-whisper";
        assert_eq!(ids(small, None, &[]).len(), 9);
        assert_eq!(ids(small, None, &[])[..3], ["aiden", "ryan", "ono_anna"]);
        assert_eq!(ids(whisper, None, &[])[..2], ["vivian", "aiden"]);
        let anna = ids(whisper, Some(("Ono_Anna", "Japanese")), &[]);
        assert_eq!(anna[..3], ["ono_anna", "aiden", "ryan"]);
        // Cloned voices follow the speakers, unless one speaks first.
        assert_eq!(ids(small, None, &cloned)[8..], ["eric", "mom"]);
        assert_eq!(
            ids(small, Some(("mom", "Chinese")), &cloned)[..2],
            ["mom", "aiden"]
        );
        assert!(voices(small, None, &cloned).ends_with(
            r#"{"id":"mom","languages":[{"code":"zh","title":"Chinese"}],"name":"mom"}]}"#
        ));
        assert_eq!(
            voices(whisper, None, &[]).split_once(',').unwrap().0,
            r#"{"voices":[{"id":"vivian""#
        );
        assert!(voices(whisper, None, &[]).contains(
            r#"{"id":"ono_anna","languages":[{"code":"ja","title":"Japanese"}],"name":"Ono_Anna"}"#
        ));
        assert_eq!(
            voices("pocket-tts:french_24l", None, &cloned),
            r#"{"voices":[{"id":"default","languages":[{"code":"fr","title":"French"}],"name":"Default"}]}"#
        );
        let both =
            r#""languages":[{"code":"zh","title":"Chinese"},{"code":"en","title":"English"}]"#;
        assert!(voices("chattts:whisper", None, &[]).contains(both));
        assert_eq!(ids("kokoro:82m", None, &cloned), ["default"]);
        assert_eq!(
            voices("kokoro:82m-whisper", None, &cloned),
            voices("kokoro:82m", None, &cloned)
        );
        assert_eq!(
            voices(
                "kokoro:82m-whisper",
                Some(&("hf_alpha".into(), "Hindi")),
                &cloned
            ),
            r#"{"voices":[{"id":"hf_alpha","languages":[{"code":"hi","title":"Hindi"}],"name":"hf_alpha"}]}"#
        );
        // The speaker that speaks first is listed in the language --language named.
        assert!(
            voices(whisper, Some(&("Ryan".into(), "German")), &[]).starts_with(
                r#"{"voices":[{"id":"ryan","languages":[{"code":"de","title":"German"}],"name":"Ryan"},{"id":"aiden","languages":[{"code":"en","title":"English"}]"#
            )
        );
        // Supertonic lists the voice --voice named, in the language --language named.
        assert_eq!(
            voices("supertonic:3", Some(&("F1".into(), "Swedish")), &cloned),
            r#"{"voices":[{"id":"f1","languages":[{"code":"sv","title":"Swedish"}],"name":"F1"}]}"#
        );
        // Kokoro lists the voice --voice named, in that voice's language.
        assert_eq!(
            voices(
                "kokoro:82m-9b",
                Some(&("ef_dora".into(), "Spanish")),
                &cloned
            ),
            r#"{"voices":[{"id":"ef_dora","languages":[{"code":"es","title":"Spanish"}],"name":"ef_dora"}]}"#
        );
    }

    #[test]
    fn a_voice_reaches_the_pipeline_with_its_language() {
        let cloned = [("mom".to_owned(), "Chinese"), ("nok".to_owned(), "Thai")];
        let speech = |id, name: &str, language| speech(id, &(name.to_owned(), language), &cloned);
        let reply = |language| format!("{PROMPT} Always reply in {language}.");
        assert_eq!(
            speech("kokoro:82m-whisper", "jf_alpha", "Japanese"),
            ["--kokoro_voice", "jf_alpha", "--kokoro_lang_code", "j"]
        );
        assert_eq!(
            speech("kitten-tts:mini-0.8", "Luna", "English"),
            ["--kitten_voice", "Luna"]
        );
        // The profile's own prompt names a Pocket TTS config's language.
        assert_eq!(
            speech("pocket-tts:french_24l", "lola", "French"),
            ["--pocket_tts_voice", "lola"]
        );
        assert_eq!(
            speech("phonon:31m-en", "Old Mortlake", "English"),
            ["--phonon_tts_voice", "Old Mortlake"]
        );
        // Supertonic is told the language, and so is the language model unless it is English.
        let english = [
            "--supertonic_tts_voice",
            "F1",
            "--supertonic_tts_lang",
            "en",
        ];
        assert_eq!(speech("supertonic:3", "F1", "English"), english);
        let options = [
            "--supertonic_tts_voice",
            "M2",
            "--supertonic_tts_lang",
            "sv",
        ];
        assert_eq!(
            speech("supertonic:3", "M2", "Swedish"),
            [&options[..], &["--init_chat_prompt", &reply("Swedish")]].concat()
        );
        // A Qwen3-TTS speaker in its own language keeps its entry's way with languages.
        let whisper = "qwen3-tts:0.6b-customvoice-whisper";
        let anna = ["--qwen3_tts_speaker", "Ono_Anna"];
        assert_eq!(speech(whisper, "Ono_Anna", "Japanese"), anna);
        assert!(speech(whisper, "mom", "Chinese").is_empty());
        // VoxCPM2 reads a voice cloned in a language Qwen3-TTS does not speak: only the language
        // model is told it.
        assert_eq!(
            speech(whisper, "nok", "Thai"),
            ["--init_chat_prompt", &reply("Thai")]
        );
        // In another, Qwen3-TTS and the language model are told which.
        let german = ["--qwen3_tts_language", "german", "--init_chat_prompt"];
        assert_eq!(
            speech(whisper, "Ono_Anna", "German"),
            [&anna[..], &german, &[&reply("German")]].concat()
        );
        assert_eq!(
            speech(whisper, "mom", "German"),
            [&german[..], &[&reply("German")]].concat()
        );
        // The prompt Boubou extends is the one the runner extends.
        let runner = Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/qwen/voice.py");
        let runner = fs::read_to_string(runner).unwrap();
        let (_, prompt) = runner.split_once("PROMPT = (").unwrap();
        let (prompt, _) = prompt.split_once(')').unwrap();
        let prompt: String = prompt.split('"').skip(1).step_by(2).collect();
        assert_eq!(prompt, PROMPT);
    }

    #[test]
    fn cloned_voices_are_listed_and_bring_the_base_weights_of_their_size() {
        let cache = std::env::temp_dir().join(format!("boubou-clones-{}", std::process::id()));
        let small = "qwen3-tts:0.6b-customvoice-whisper";
        assert_eq!(cloner(small, &cache), None);
        let voices = cache.join("voices");
        library::keep(&voices, "mom", "zh");
        library::keep(&voices, "dad", "en");
        // Half-written, in a language no model here speaks, not a name the page gives, or words
        // without a recording.
        library::keep(&voices, "pet", "eu");
        fs::copy(voices.join("dad.json"), voices.join(".kid.json")).unwrap();
        fs::copy(voices.join("dad.json"), voices.join("Big.json")).unwrap();
        fs::copy(voices.join("dad.json"), voices.join("gone.json")).unwrap();
        let both = vec![("dad".into(), "English"), ("mom".into(), "Chinese")];
        assert_eq!(clones(&cache), both);
        // A renamed recording is its voice under the new name, without another step.
        fs::rename(voices.join("mom.wav"), voices.join("mother.wav")).unwrap();
        let renamed = vec![("dad".into(), "English"), ("mother".into(), "Chinese")];
        assert_eq!(clones(&cache), renamed);
        fs::rename(voices.join("mother.wav"), voices.join("mom.wav")).unwrap();
        fs::rename(voices.join("mother.json"), voices.join("mom.json")).unwrap();
        assert_eq!(clones(&cache), both);
        assert_eq!(cloner(small, &cache), Some("qwen3-tts:0.6b-base"));
        let large = "qwen3-tts:1.7b-customvoice-27b";
        assert_eq!(cloner(large, &cache), Some("qwen3-tts:1.7b-base"));
        assert_eq!(cloner("kokoro:82m", &cache), None);
        // `boubou clone` speaks with the Base weights of the size this computer clones with.
        let size = library::model();
        assert_eq!(cloner(CLONE, &cache), base(size));
        assert!(owners(CLONE, None, &cache).contains(&base(size).unwrap().to_owned()));
        // VoxCPM2 comes once a voice speaks a language Qwen3-TTS does not, and only then.
        assert_eq!(voxcpm(small, &cache), None);
        library::keep(&voices, "nok", "th");
        assert_eq!(clones(&cache)[2], ("nok".into(), "Thai"));
        for id in [small, large, CLONE] {
            assert_eq!(voxcpm(id, &cache), Some(VOXCPM), "{id}");
        }
        assert_eq!(voxcpm("kokoro:82m", &cache), None);
        let owned = owners(small, None, &cache);
        assert!(
            owned.contains(&"qwen3-tts:0.6b-base".to_owned()) && owned.contains(&VOXCPM.to_owned())
        );
        // With only such voices, no Base weights are fetched.
        for name in ["mom", "dad"] {
            for extension in ["wav", "json"] {
                fs::remove_file(voices.join(format!("{name}.{extension}"))).unwrap();
            }
        }
        assert_eq!(cloner(small, &cache), None);
        assert_eq!(voxcpm(small, &cache), Some(VOXCPM));
        assert!(qwen_reads("Chinese") && !qwen_reads("Thai"));
        fs::remove_dir_all(&cache).unwrap();
    }

    #[test]
    fn runner_manifests_name_their_host() {
        let manifest = |host: &str| {
            serde_json::from_str::<Runner>(&format!(
                r#"{{"url":null,"size":1,"sha256":"{0}","tree_sha256":"{0}"{host}}}"#,
                "a".repeat(64)
            ))
        };
        let mac = manifest(r#","min_macos":"15.0""#).unwrap();
        assert_eq!(mac.host.entry(), "mac.py");
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
    fn a_pocket_preset_downloads_only_its_own_voice_state() -> Result<()> {
        let root = env::temp_dir().join(format!("boubou-pocket-voice-{}", std::process::id()));
        let cache = root.join("cache");
        fs::create_dir_all(&root)?;
        let id = "pocket-tts:french_24l";
        let file = |path: &str, profile: &str| {
            let url = format!("https://example.invalid/{path}");
            serde_json::json!({"path": path, "url": url, "size": 1, "sha256": "a".repeat(64), "profiles": [profile]})
        };
        let files = serde_json::json!([
            file("model", id),
            file("estelle", id),
            file("lola", "pocket-tts:french_24l/lola"),
            file("jean", "pocket-tts:french_24l/jean"),
            file("other", "pocket-tts:german/lola"),
        ]);
        fs::write(root.join("models.json"), files.to_string())?;
        let paths = |voice: Option<&str>| -> Result<Vec<String>> {
            let voice = voice.map(|name| (name.to_owned(), "French"));
            let files = needed(id, voice.as_ref(), &cache, &root)?;
            Ok(files
                .into_iter()
                .map(|f| f.path.display().to_string())
                .collect())
        };
        let result = (|| -> Result<()> {
            assert_eq!(paths(None)?, ["model", "estelle"]);
            assert_eq!(paths(Some("estelle"))?, ["model", "estelle"]);
            assert_eq!(paths(Some("lola"))?, ["model", "estelle", "lola"]);
            let phonon = ("Zoey".to_owned(), "English");
            assert_eq!(
                owners("phonon:31m-en", Some(&phonon), &cache),
                ["phonon:31m-en"]
            );
            Ok(())
        })();
        fs::remove_dir_all(root)?;
        result
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
