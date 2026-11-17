//! The voices a person cloned: `<name>.wav`, the recording, and `<name>.json`, its words and
//! language, in one folder outside the cache. The folder is the archive: a WAV put there becomes
//! a voice once the clone page writes down what it says, and renaming the file renames the voice.

use crate::{modular::LANGUAGES, profiles};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// The Qwen3-TTS entry the commands for a voice start, and whose size of Base weights reads
/// cloned voices on this computer: see `model_for`.
pub fn model() -> &'static str {
    static MODEL: OnceLock<&'static str> = OnceLock::new();
    MODEL.get_or_init(|| model_for(crate::onboarding::memory()))
}

/// The entry for a computer with `memory` bytes: the 1.7B one, whose clones misread fewer words
/// and cope better with a recording that does not match its words, where its conversation and
/// Base weights fit with room to spare; else the 0.6B one, which clones as alike.
fn model_for(memory: Option<u64>) -> &'static str {
    const LARGE: &str = "qwen3-tts:1.7b-customvoice";
    const SMALL: &str = "qwen3-tts:0.6b-customvoice";
    // The 1.7B Base weights take about 3 GiB beside the conversation's estimate, and 2 GiB more
    // keeps the rest of the computer usable, as the checklist asks.
    let needs = crate::profiles::Model::Modular(LARGE)
        .memory_gib()
        .map(|gib| (gib + 3 + 2) << 30);
    match (memory, needs) {
        (Some(memory), Some(needs)) if memory >= needs => LARGE,
        _ => SMALL,
    }
}
/// The file in Boubou's data folder that keeps the folder a person chose.
const SETTING: &str = "voices-folder";
/// The longest file an imported zip may hold: 30 seconds of 48 kHz stereo 32-bit audio is 12 MB.
const LARGEST: u64 = 64 << 20;
/// Audio a person may drop into the folder that only the clone page can turn into a voice.
const OTHER_AUDIO: [&str; 9] = [
    "mp3", "m4a", "aac", "aif", "aiff", "flac", "ogg", "opus", "caf",
];

static FOLDER: OnceLock<PathBuf> = OnceLock::new();

/// The folder this run keeps voices in: the one `open` chose, or, in tests that open none,
/// `cache`'s own.
pub fn folder(cache: &Path) -> PathBuf {
    FOLDER
        .get()
        .cloned()
        .unwrap_or_else(|| cache.join("voices"))
}

/// Boubou's own data folder, which no cleaner or sync service treats as disposable:
/// `~/Library/Application Support/boubou` on macOS, `~/.local/share/boubou` on Linux.
fn data() -> Result<PathBuf> {
    dirs::data_dir()
        .map(|data| data.join("boubou"))
        .context("Cannot find a folder for your voices. Set BOUBOU_VOICES_DIR or --voices-dir.")
}

/// Where voices stay unless `--voices-dir` names a folder: the folder a person chose, which
/// `data` keeps, or `data`'s own `voices`.
fn kept(data: &Path) -> PathBuf {
    fs::read_to_string(data.join(SETTING))
        .ok()
        .map(|text| PathBuf::from(text.trim_end_matches('\n')))
        .filter(|chosen| chosen.is_absolute())
        .unwrap_or_else(|| data.join("voices"))
}

/// Chooses the folder for this run: `chosen`, or the one kept in Boubou's data folder. Voices that
/// Boubou 0.1.0-beta.16 and earlier kept in `cache` move there first.
pub fn open(cache: &Path, chosen: Option<PathBuf>) -> Result<PathBuf> {
    let folder = match chosen {
        Some(chosen) => std::path::absolute(chosen)?,
        None => settle(cache, &data()?),
    };
    Ok(FOLDER.get_or_init(|| folder).clone())
}

/// The folder kept in `data`, holding the voices that `cache` held, as far as it can.
fn settle(cache: &Path, data: &Path) -> PathBuf {
    let folder = kept(data);
    let old = cache.join("voices");
    match gather(&old, &folder) {
        Ok(moved) => {
            if !moved.moved.is_empty() {
                eprintln!(
                    "Moved your voices from the cache to {}: {}.",
                    folder.display(),
                    moved.moved.join(", ")
                );
            }
            for name in &moved.kept {
                eprintln!(
                    "Kept the voice {name} in {}: {} already has a voice with that name.",
                    old.display(),
                    folder.display()
                );
            }
            // Launchers of voices that moved go, and the folder once empty, so the move is not
            // tried again.
            let _ = prune(&old, &scan(&old).voices);
            let _ = fs::remove_dir(&old);
        }
        Err(error) => eprintln!(
            "Could not move your voices from {} to {}: {error:#}",
            old.display(),
            folder.display()
        ),
    }
    folder
}

/// Makes `folder` ready to keep voices in. A chosen folder whose drive is not connected is not
/// made again in its place.
pub fn ready(folder: &Path) -> Result<()> {
    ready_in(&data()?, folder)
}

fn ready_in(data: &Path, folder: &Path) -> Result<()> {
    let own = folder == data.join("voices");
    ensure!(
        own || folder.parent().is_some_and(Path::is_dir),
        "The folder that keeps your voices, {}, is missing. Connect its drive, or choose another with `{} voices --folder <path>`.",
        folder.display(),
        crate::update::program()
    );
    fs::create_dir_all(folder)
        .with_context(|| format!("Cannot make the folder {}", folder.display()))
}

/// What `gather` did with each voice, by name.
#[derive(Debug, Default, PartialEq)]
pub struct Moved {
    pub moved: Vec<String>,
    /// Left where they were, as `to` has a file of the same name.
    pub kept: Vec<String>,
}

/// Moves each voice from `from` to `to` whole, its recording before its words, and never over a
/// file `to` already has. Files that are no voice's, and hidden ones, stay.
pub fn gather(from: &Path, to: &Path) -> Result<Moved> {
    let mut found = Moved::default();
    let Ok(entries) = fs::read_dir(from) else {
        return Ok(found);
    };
    if fs::canonicalize(from).ok() == fs::canonicalize(to).ok() {
        return Ok(found);
    }
    let mut voices: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some((stem, extension)) = parts(&path)
            && matches!(extension.as_str(), "wav" | "json")
            && entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            voices.entry(stem).or_default().push(path);
        }
    }
    if voices.is_empty() {
        return Ok(found);
    }
    fs::create_dir_all(to)?;
    for (stem, mut files) in voices {
        let target = |file: &PathBuf| to.join(file.file_name().unwrap_or_default());
        if files.iter().any(|file| target(file).exists()) {
            found.kept.push(stem);
            continue;
        }
        files.sort_by_key(|file| parts(file).is_some_and(|(_, extension)| extension == "json"));
        for file in &files {
            move_file(file, &target(file))?;
        }
        found.moved.push(stem);
    }
    Ok(found)
}

/// Moves a file, never over another, and on another volume removes the original only once the
/// copy is whole.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    match fs::hard_link(from, to) {
        Ok(()) => return Ok(fs::remove_file(from)?),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            bail!("{} already exists", to.display())
        }
        // Another volume, or one without links.
        Err(_) => {}
    }
    let partial = hidden(to);
    fs::copy(from, &partial)?;
    fs::File::open(&partial)?.sync_all()?;
    ensure!(!to.exists(), "{} already exists", to.display());
    fs::rename(&partial, to)?;
    Ok(fs::remove_file(from)?)
}

/// Where a file is written before it appears whole at `path`. Listing skips hidden files.
fn hidden(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.part"))
}

/// Writes `content` to `path` so that it appears whole or not at all.
fn write(path: &Path, content: &[u8]) -> Result<()> {
    let partial = hidden(path);
    fs::write(&partial, content)?;
    Ok(fs::rename(&partial, path)?)
}

/// Keeps voices in `to` from now on, with the ones in `from`. Returns the folder, as written.
pub fn choose(from: &Path, to: &Path) -> Result<(PathBuf, Moved)> {
    choose_in(&data()?, from, to)
}

fn choose_in(data: &Path, from: &Path, to: &Path) -> Result<(PathBuf, Moved)> {
    fs::create_dir_all(to).with_context(|| format!("Cannot make the folder {}", to.display()))?;
    let to = fs::canonicalize(to)?;
    let moved = gather(from, &to)?;
    prune(from, &scan(from).voices)?;
    fs::create_dir_all(data)?;
    if fs::canonicalize(data.join("voices")).ok().as_ref() == Some(&to) {
        match fs::remove_file(data.join(SETTING)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
    } else {
        write(
            &data.join(SETTING),
            format!("{}\n", to.display()).as_bytes(),
        )?;
    }
    Ok((to, moved))
}

/// Whether a sync service may copy `folder` off this computer: iCloud's Desktop, Documents and
/// Drive folders, and the services under `~/Library/CloudStorage`.
pub fn synced(folder: &Path) -> bool {
    dirs::home_dir().is_some_and(|home| synced_in(folder, &home))
}

fn synced_in(folder: &Path, home: &Path) -> bool {
    let folder = fs::canonicalize(folder).unwrap_or_else(|_| folder.to_owned());
    let home = fs::canonicalize(home).unwrap_or_else(|_| home.to_owned());
    [
        "Desktop",
        "Documents",
        "Library/Mobile Documents",
        "Library/CloudStorage",
    ]
    .iter()
    .any(|synced| folder.starts_with(home.join(synced)))
}

/// A file's name without its extension, and the extension in lowercase.
fn parts(path: &Path) -> Option<(String, String)> {
    let name = path.file_name()?.to_str()?;
    if name.starts_with('.') {
        return None;
    }
    let (stem, extension) = name.rsplit_once('.')?;
    Some((stem.to_owned(), extension.to_ascii_lowercase()))
}

/// Whether `name` is one the clone page gives: lowercase letters, digits, - and _, up to 32.
pub fn named(name: &str) -> bool {
    name.len() <= 32
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
}

/// Qwen3-TTS's own speakers keep their names.
fn built_in(name: &str) -> bool {
    profiles::VOICES
        .iter()
        .any(|(speaker, _)| speaker.eq_ignore_ascii_case(name))
}

/// A name the page gives, as close to `stem` as it can be.
fn suggest(stem: &str) -> String {
    let mut name = String::new();
    for c in stem.to_ascii_lowercase().chars() {
        match c {
            'a'..='z' | '0'..='9' | '_' => name.push(c),
            _ if !name.is_empty() && !name.ends_with('-') => name.push('-'),
            _ => {}
        }
    }
    name.truncate(32);
    let name = name.trim_end_matches('-');
    if name.is_empty() {
        "my-voice".into()
    } else {
        name.into()
    }
}

/// What to do about a file named `stem`, when it is not a name a voice can have.
fn misnamed(stem: &str) -> Option<String> {
    if !named(stem) {
        let name = suggest(stem);
        return Some(format!(
            "Rename it {name}.wav: lowercase letters, digits, - and _."
        ));
    }
    built_in(stem).then(|| format!("{stem} is a built-in voice. Rename it, such as {stem}-2.wav."))
}

/// Why a recording cannot be a voice, if it cannot: a WAV of 3 to 30 seconds.
fn unusable(wav: impl Read) -> Option<String> {
    let Ok(reader) = hound::WavReader::new(io::BufReader::new(wav)) else {
        return Some("Boubou cannot read this WAV file. Export it again as a 16-bit WAV.".into());
    };
    let seconds = f64::from(reader.duration()) / f64::from(reader.spec().sample_rate.max(1));
    (!(3.0..=30.0).contains(&seconds))
        .then(|| format!("It lasts {seconds:.0} s. Give 3 to 30 seconds of one voice."))
}

/// A voice's words and the code of its language, from its `.json`.
fn words(json: &[u8]) -> Option<(String, &'static str)> {
    let words: Value = serde_json::from_slice(json).ok()?;
    let text = words["text"]
        .as_str()
        .filter(|text| !text.trim().is_empty())?;
    let (code, _) = LANGUAGES
        .iter()
        .find(|(code, _)| words["language"] == *code)?;
    Some((text.to_owned(), code))
}

fn digest(content: &[u8]) -> String {
    crate::assets::hex(&Sha256::digest(content))
}

/// A voice in the folder, ready to speak.
#[derive(Debug, PartialEq)]
pub struct Voice {
    pub name: String,
    /// Its language's code, such as `zh`.
    pub language: &'static str,
    pub text: String,
}

impl Voice {
    /// The language's name, as `--language` and the voice lists give it.
    pub fn speaks(&self) -> &'static str {
        LANGUAGES
            .iter()
            .find(|(code, _)| *code == self.language)
            .map_or("English", |(_, title)| title)
    }
}

/// What the folder holds.
#[derive(Debug, Default, PartialEq)]
pub struct Scan {
    pub voices: Vec<Voice>,
    /// Recordings whose words nobody wrote down yet, by name: the clone page does.
    pub unheard: Vec<String>,
    /// Files that cannot be voices as they are, each with what to do.
    pub attention: Vec<(String, String)>,
}

/// Reads the folder without changing it.
pub fn scan(folder: &Path) -> Scan {
    let mut found = Scan::default();
    let Ok(entries) = fs::read_dir(folder) else {
        return found;
    };
    // Each name's files, by extension.
    let mut stems: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for entry in entries.flatten() {
        if let Some((stem, extension)) = parts(&entry.path())
            && entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            let file = entry.file_name().to_string_lossy().into_owned();
            stems.entry(stem).or_default().insert(extension, file);
        }
    }
    for (stem, files) in stems {
        let (wav, json) = (files.get("wav"), files.get("json"));
        let other = OTHER_AUDIO.iter().find_map(|kind| files.get(*kind));
        let Some(wav) = wav else {
            if let Some(audio) = other {
                let hint = "Only WAV files become voices. Export it as WAV, or upload it on the clone page.";
                found.attention.push((audio.clone(), hint.into()));
            } else if let Some(json) = json.filter(|_| named(&stem)) {
                let hint = format!(
                    "Its recording, {stem}.wav, is missing. Put it back, or delete this file."
                );
                found.attention.push((json.clone(), hint));
            }
            continue;
        };
        if let Some(hint) = misnamed(&stem) {
            found.attention.push((wav.clone(), hint));
            continue;
        }
        // Boubou finds a voice's files by their names, on file systems that tell case apart too.
        if let Some(file) = [Some(wav), json]
            .into_iter()
            .flatten()
            .find(|file| !(file.ends_with(".wav") || file.ends_with(".json")))
        {
            let lower = format!(
                "{stem}.{}",
                file.rsplit('.')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
            );
            found
                .attention
                .push((file.clone(), format!("Rename it {lower}, in lowercase.")));
            continue;
        }
        if let Some(hint) =
            fs::File::open(folder.join(wav)).map_or_else(|error| Some(error.to_string()), unusable)
        {
            found.attention.push((wav.clone(), hint));
            continue;
        }
        let Some(json) = json else {
            found.unheard.push(stem);
            continue;
        };
        match fs::read(folder.join(json)).ok().as_deref().and_then(words) {
            Some((text, language)) => found.voices.push(Voice {
                name: stem,
                language,
                text,
            }),
            None => {
                let hint = "Boubou cannot read these words. Delete this file, and the clone page writes them down again.";
                found.attention.push((json.clone(), hint.into()));
            }
        }
    }
    found
}

/// Gives each voice the digest of its recording, and the words of a renamed recording back to it
/// by that digest.
pub fn tidy(folder: &Path) -> Result<()> {
    let found = scan(folder);
    let path = |name: &str, extension| folder.join(format!("{name}.{extension}"));
    // Words left behind when their recording was renamed, by the digest they keep.
    let mut orphans = BTreeMap::new();
    for entry in fs::read_dir(folder).into_iter().flatten().flatten() {
        if let Some((stem, extension)) = parts(&entry.path())
            && extension == "json"
            && named(&stem)
            && !path(&stem, "wav").exists()
            && let Ok(words) = serde_json::from_slice::<Value>(&fs::read(entry.path())?)
            && let Some(sum) = words["sha256"].as_str()
        {
            orphans.insert(sum.to_owned(), entry.path());
        }
    }
    for name in &found.unheard {
        if let Some(words) = orphans.remove(&digest(&fs::read(path(name, "wav"))?)) {
            move_file(&words, &path(name, "json"))?;
        }
    }
    for voice in &found.voices {
        let mut words: Value = serde_json::from_slice(&fs::read(path(&voice.name, "json"))?)?;
        if words.get("sha256").is_none() {
            words["sha256"] = digest(&fs::read(path(&voice.name, "wav"))?).into();
            write(&path(&voice.name, "json"), words.to_string().as_bytes())?;
        }
    }
    if cfg!(target_os = "macos") {
        launchers(folder, &scan(folder).voices)?;
    }
    Ok(())
}

/// A path as a shell reads it.
fn quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    shlex::try_quote(&text).map_or_else(|_| text.to_string(), |quoted| quoted.into_owned())
}

/// What a command adds to find voices in `folder`: nothing for the folder Boubou keeps them in,
/// or `--voices-dir` for a folder one command chose.
fn elsewhere(folder: &Path) -> String {
    let same = |a: &Path, b: &Path| match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    match data() {
        Ok(data) if same(&kept(&data), folder) => String::new(),
        _ => format!(" --voices-dir {}", quote(folder)),
    }
}

/// The first lines of each launcher Boubou writes, by which it knows its own.
const MADE: &str = "#!/bin/sh\n# Made by Boubou for the voice";

/// What `<name>.command` holds: double-clicked in Finder, Terminal talks with the voice. The
/// person's own login shell starts Boubou, so it finds npx as their terminal does. `elsewhere`
/// finds the voice's folder.
fn launcher(name: &str, elsewhere: &str) -> String {
    let program = crate::update::program();
    // npx would first ask whether to install Boubou, in a window opened for talking.
    let program = match program.strip_prefix("npx ") {
        Some(package) => format!("npx --yes {package}"),
        None => program.to_owned(),
    };
    let talk = format!("{program} run {} --voice {name}{elsewhere}", model());
    let talk = shlex::try_quote(&talk).map_or_else(|_| talk.clone(), |quoted| quoted.into_owned());
    format!(
        "{MADE} {name}. Double-click it to talk with {name}; close the window to stop.\n\
         exec \"${{SHELL:-/bin/zsh}}\" -lic {talk}\n"
    )
}

/// Gives each voice a launcher beside its recording, and removes the launchers Boubou wrote for
/// voices that are gone. A person's own `.command` files stay as they are.
fn launchers(folder: &Path, voices: &[Voice]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let elsewhere = elsewhere(folder);
    for voice in voices {
        let path = folder.join(format!("{}.command", voice.name));
        let wanted = launcher(&voice.name, &elsewhere);
        let here = fs::read_to_string(&path);
        if here
            .as_deref()
            .is_ok_and(|here| here == wanted || !here.starts_with(MADE))
            || here
                .as_ref()
                .is_err_and(|error| error.kind() != io::ErrorKind::NotFound)
        {
            continue;
        }
        let partial = hidden(&path);
        fs::write(&partial, wanted)?;
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o755))?;
        fs::rename(&partial, &path)?;
    }
    prune(folder, voices)
}

/// Removes the launchers Boubou wrote in `folder` for voices other than `voices`.
fn prune(folder: &Path, voices: &[Voice]) -> Result<()> {
    for entry in fs::read_dir(folder).into_iter().flatten().flatten() {
        if let Some((stem, extension)) = parts(&entry.path())
            && extension == "command"
            && !voices.iter().any(|voice| voice.name == stem)
            && fs::read_to_string(entry.path()).is_ok_and(|text| text.starts_with(MADE))
        {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// The launcher of the voice `name` in `folder`, once there is one.
fn launcher_of(folder: &Path, name: &str) -> Option<PathBuf> {
    Some(folder.join(format!("{name}.command"))).filter(|path| path.is_file())
}

/// Writes the voices `names` names, or all of them, to the zip file `zip`.
pub fn export(folder: &Path, names: &[String], zip: &Path) -> Result<Vec<String>> {
    let voices = scan(folder).voices;
    for name in names {
        ensure!(
            voices.iter().any(|voice| voice.name == *name),
            "No voice is named {name}."
        );
    }
    let chosen: Vec<_> = voices
        .iter()
        .filter(|voice| names.is_empty() || names.contains(&voice.name))
        .map(|voice| voice.name.clone())
        .collect();
    ensure!(!chosen.is_empty(), "There are no voices to export yet.");
    let partial = hidden(zip);
    let mut writer = zip::ZipWriter::new(fs::File::create(&partial)?);
    let options = zip::write::SimpleFileOptions::default();
    for name in &chosen {
        for extension in ["wav", "json"] {
            let file = format!("{name}.{extension}");
            writer.start_file(file.as_str(), options)?;
            writer.write_all(&fs::read(folder.join(&file))?)?;
        }
    }
    writer.finish()?.sync_all()?;
    fs::rename(&partial, zip)?;
    Ok(chosen)
}

/// What `import` did with each voice in a zip file.
#[derive(Debug, Default, PartialEq)]
pub struct Imported {
    /// Each voice added, by its name in the zip file and its name here.
    pub added: Vec<(String, String)>,
    /// Each file left out, and why.
    pub skipped: Vec<(String, String)>,
}

/// Adds the voices in the zip file `zip` to `folder`. A voice whose name is taken by another
/// gets the next free name, such as mom-2; one that is already here is left out.
pub fn import(folder: &Path, zip: &Path) -> Result<Imported> {
    let mut archive =
        zip::ZipArchive::new(fs::File::open(zip)?).context("Not a zip file of voices")?;
    let mut found = Imported::default();
    // The recording and words of each name, by extension.
    let mut voices: BTreeMap<String, BTreeMap<String, Vec<u8>>> = BTreeMap::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let Some(path) = entry.enclosed_name().filter(|_| !entry.is_dir()) else {
            continue;
        };
        if path.starts_with("__MACOSX") {
            continue;
        }
        let Some((stem, extension)) = parts(&path) else {
            continue;
        };
        if !matches!(extension.as_str(), "wav" | "json") {
            continue;
        }
        let file = format!("{stem}.{extension}");
        if entry.size() > LARGEST {
            found
                .skipped
                .push((file, "It is too large to be a voice.".into()));
            continue;
        }
        let mut content = Vec::new();
        entry.take(LARGEST + 1).read_to_end(&mut content)?;
        voices.entry(stem).or_default().insert(extension, content);
    }
    fs::create_dir_all(folder)?;
    let path = |name: &str, extension| folder.join(format!("{name}.{extension}"));
    'voices: for (stem, mut files) in voices {
        let json = files.remove("json");
        let Some(wav) = files.remove("wav") else {
            let hint = "Its recording is not in the zip file.";
            found.skipped.push((format!("{stem}.json"), hint.into()));
            continue;
        };
        if let Some(hint) = unusable(wav.as_slice()) {
            found.skipped.push((format!("{stem}.wav"), hint));
            continue;
        }
        let sum = digest(&wav);
        let base = if named(&stem) {
            stem.clone()
        } else {
            suggest(&stem)
        };
        let mut name = base.clone();
        for number in 2.. {
            if !built_in(&name) {
                let here = fs::read(path(&name, "wav"));
                if here.as_ref().is_ok_and(|here| digest(here) == sum) {
                    found
                        .skipped
                        .push((format!("{stem}.wav"), format!("{name} is already here.")));
                    continue 'voices;
                }
                if here.is_err() && !path(&name, "json").exists() {
                    break;
                }
            }
            let suffix = format!("-{number}");
            let mut start = base.clone();
            start.truncate(32 - suffix.len());
            name = format!("{start}{suffix}");
        }
        write(&path(&name, "wav"), &wav)?;
        // Words that cannot be read are left out: the clone page writes them down again.
        if let Some(mut words) = json
            .and_then(|json| serde_json::from_slice::<Value>(&json).ok())
            .filter(|words| words["text"].is_string())
        {
            words["sha256"] = sum.into();
            write(&path(&name, "json"), words.to_string().as_bytes())?;
        }
        found.added.push((stem, name));
    }
    Ok(found)
}

/// What a person types to use the voice `name`: to talk with it, to share it with phones and
/// apps, and to read a text aloud through the API while `serve` runs. `elsewhere` finds the
/// voice's folder.
pub fn commands(name: &str, elsewhere: &str) -> [(&'static str, String); 3] {
    let program = crate::update::program();
    let body = json!({"voice": name, "input": "Hello."});
    [
        (
            "Talk",
            format!("{program} run {} --voice {name}{elsewhere}", model()),
        ),
        (
            "Share",
            format!("{program} serve {} --voice {name}{elsewhere}", model()),
        ),
        (
            "API",
            format!(
                "curl http://127.0.0.1:2682/v1/audio/speech -H 'Content-Type: application/json' -d '{body}' -o {name}.wav"
            ),
        ),
    ]
}

/// The folder and what it holds, as the clone page reads it.
pub fn listing(folder: &Path) -> Value {
    let scan = scan(folder);
    let elsewhere = elsewhere(folder);
    let voices: Vec<_> = scan
        .voices
        .iter()
        .map(|voice| {
            let commands: Vec<_> = commands(&voice.name, &elsewhere)
                .into_iter()
                .map(|(title, command)| json!({"title": title, "command": command}))
                .collect();
            json!({
                "name": voice.name,
                "language": voice.language,
                "text": voice.text,
                "commands": commands,
                "launcher": launcher_of(folder, &voice.name),
            })
        })
        .collect();
    let attention: Vec<_> = scan
        .attention
        .iter()
        .map(|(file, hint)| json!({"file": file, "hint": hint}))
        .collect();
    json!({
        "folder": folder,
        "synced": synced(folder),
        "voices": voices,
        "unheard": scan.unheard,
        "attention": attention,
    })
}

/// A warning for a folder that a sync service may copy off this computer.
pub const SYNCED: &str = "This folder may sync to iCloud or another cloud service, so your voices may leave this computer.";

/// Prints the folder and what it holds, for `boubou voices`.
pub fn print(folder: &Path) {
    let scan = scan(folder);
    println!("Voices in {}", folder.display());
    if synced(folder) {
        println!("{SYNCED}");
    }
    if scan.voices.is_empty() && scan.unheard.is_empty() {
        println!(
            "\nNo voices yet. Clone one with `{} clone`, or put a WAV file of one voice here.",
            crate::update::program()
        );
    }
    for voice in &scan.voices {
        println!("\n{} · {}", voice.name, voice.speaks());
        for (title, command) in commands(&voice.name, &elsewhere(folder)) {
            println!("  {title:<6} {command}");
        }
        if let Some(launcher) = launcher_of(folder, &voice.name) {
            println!("  Or double-click {} in Finder.", launcher.display());
        }
    }
    if !scan.unheard.is_empty() {
        println!(
            "\nWaiting for their words: {}. Open `{} clone` to write down what each says.",
            scan.unheard.join(", "),
            crate::update::program()
        );
    }
    if !scan.attention.is_empty() {
        println!("\nNeeds attention:");
        for (file, hint) in &scan.attention {
            println!("  {file}: {hint}");
        }
    }
}

/// `boubou voices`: chooses another folder, exports or imports when asked, then lists the
/// folder, for a person or, in JSON, for the clone page.
pub fn command(
    folder: &Path,
    to: Option<PathBuf>,
    zip_to: Option<PathBuf>,
    zip_from: Option<PathBuf>,
    names: &[String],
    json: bool,
) -> Result<()> {
    let mut done = serde_json::Map::new();
    let mut said = Vec::new();
    let mut folder = folder.to_owned();
    if let Some(to) = to {
        let (chosen, moved) = choose(&folder, &to)?;
        if !moved.moved.is_empty() {
            said.push(format!(
                "Moved {} to {}.",
                moved.moved.join(", "),
                chosen.display()
            ));
        }
        for name in &moved.kept {
            said.push(format!(
                "Kept {name} in {}: {} already has a voice with that name.",
                folder.display(),
                chosen.display()
            ));
        }
        done.insert("moved".into(), json!(moved.moved));
        done.insert("kept".into(), json!(moved.kept));
        folder = chosen;
    }
    tidy(&folder)?;
    if let Some(zip) = zip_to {
        let exported = export(&folder, names, &zip)?;
        said.push(format!(
            "Wrote {} to {}.",
            exported.join(", "),
            zip.display()
        ));
        done.insert("exported".into(), json!(exported));
    }
    if let Some(zip) = zip_from {
        let imported = import(&folder, &zip)?;
        for (from, to) in &imported.added {
            said.push(match from == to {
                true => format!("Added {to}."),
                false => format!("Added {from} as {to}."),
            });
        }
        for (file, why) in &imported.skipped {
            said.push(format!("Left out {file}: {why}"));
        }
        let added: Vec<_> = imported
            .added
            .iter()
            .map(|(from, to)| json!({"from": from, "to": to}))
            .collect();
        let skipped: Vec<_> = imported
            .skipped
            .iter()
            .map(|(file, why)| json!({"file": file, "why": why}))
            .collect();
        done.insert("added".into(), json!(added));
        done.insert("skipped".into(), json!(skipped));
    }
    if json {
        let mut listed = listing(&folder);
        listed
            .as_object_mut()
            .expect("a listing is an object")
            .extend(done);
        println!("{listed}");
    } else {
        for line in &said {
            println!("{line}");
        }
        if !said.is_empty() {
            println!();
        }
        print(&folder);
    }
    Ok(())
}

/// Writes a voice for tests: three seconds of a tone, and its words in `language`.
#[cfg(test)]
pub fn keep(folder: &Path, name: &str, language: &str) {
    fs::create_dir_all(folder).unwrap();
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 24000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = hound::WavWriter::create(folder.join(format!("{name}.wav")), spec).unwrap();
    for i in 0..72000 {
        // A different tone for each name, so their recordings differ.
        let pitch = 200 + name.bytes().map(usize::from).sum::<usize>() % 50 * 10;
        let phase = i as f32 * pitch as f32 / 24000.0;
        wav.write_sample((phase.sin() * 8000.0) as i16).unwrap();
    }
    wav.finalize().unwrap();
    let words = json!({"text": "Hello.", "language": language});
    fs::write(folder.join(format!("{name}.json")), words.to_string()).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("boubou-library-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(fs::canonicalize(dir).unwrap())
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn files(folder: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(folder)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn voices_move_out_of_the_cache_without_replacing_any() {
        let dir = TestDir::new("settle");
        let (cache, data) = (dir.0.join("cache"), dir.0.join("data"));
        let old = cache.join("voices");
        keep(&old, "me", "en");
        keep(&old, "mom", "zh");
        fs::write(old.join("notes.txt"), "mine").unwrap();
        fs::write(old.join(".me.wav.part"), "half").unwrap();
        // The new folder already has a voice named mom, which stays as it is.
        fs::create_dir_all(data.join("voices")).unwrap();
        fs::write(data.join("voices/mom.json"), "theirs").unwrap();

        let folder = settle(&cache, &data);
        assert_eq!(folder, data.join("voices"));
        assert_eq!(
            files(&folder),
            ["me.json", "me.wav", "mom.json"].map(String::from)
        );
        assert_eq!(
            fs::read_to_string(folder.join("mom.json")).unwrap(),
            "theirs"
        );
        assert_eq!(
            files(&old),
            [".me.wav.part", "mom.json", "mom.wav", "notes.txt"].map(String::from)
        );
        assert_eq!(scan(&folder).voices[0].name, "me");

        // Once the cache has nothing left, its folder goes, and nothing moves again.
        for file in [".me.wav.part", "mom.json", "mom.wav", "notes.txt"] {
            fs::remove_file(old.join(file)).unwrap();
        }
        assert_eq!(settle(&cache, &data), folder);
        assert!(!old.exists());
        assert_eq!(settle(&cache, &data), folder);
    }

    #[test]
    fn the_chosen_folder_is_kept_and_takes_the_voices_along() {
        let dir = TestDir::new("choose");
        let data = dir.0.join("data");
        assert_eq!(kept(&data), data.join("voices"));
        keep(&data.join("voices"), "me", "en");

        let archive = dir.0.join("Archive/Boubou voices");
        let (folder, moved) = choose_in(&data, &data.join("voices"), &archive).unwrap();
        assert_eq!(folder, archive);
        assert_eq!(moved.moved, ["me"]);
        assert_eq!(kept(&data), archive);
        assert_eq!(scan(&archive).voices.len(), 1);
        assert!(files(&data.join("voices")).is_empty());

        // Choosing the default folder again forgets the choice.
        let (folder, _) = choose_in(&data, &archive, &data.join("voices")).unwrap();
        assert_eq!(folder, data.join("voices"));
        assert!(!data.join(SETTING).exists());
        assert_eq!(kept(&data), data.join("voices"));
        assert_eq!(scan(&folder).voices.len(), 1);

        // A chosen folder on a drive that is not connected is not made again.
        let unplugged = dir.0.join("Volumes/Drive/voices");
        assert!(
            ready_in(&data, &unplugged)
                .unwrap_err()
                .to_string()
                .contains("is missing")
        );
        assert!(!dir.0.join("Volumes").exists());
        ready_in(&data, &archive.join("more")).unwrap();
        fs::remove_dir_all(&data).unwrap();
        ready_in(&data, &data.join("voices")).unwrap();
        assert!(data.join("voices").is_dir());

        // A relative path written by hand is not a folder Boubou can find again.
        fs::write(data.join(SETTING), "voices\n").unwrap();
        assert_eq!(kept(&data), data.join("voices"));
    }

    #[test]
    fn computers_with_room_clone_with_the_larger_model() {
        let gib = |gib: u64| Some(gib << 30);
        assert_eq!(model_for(gib(128)), "qwen3-tts:1.7b-customvoice");
        assert_eq!(model_for(gib(24)), "qwen3-tts:1.7b-customvoice");
        assert_eq!(model_for(gib(16)), "qwen3-tts:0.6b-customvoice");
        assert_eq!(model_for(gib(8)), "qwen3-tts:0.6b-customvoice");
        assert_eq!(model_for(None), "qwen3-tts:0.6b-customvoice");
        assert!(commands("me", "")[0].1.contains(model()));
    }

    #[test]
    fn synced_folders_are_named() {
        let home = Path::new("/Users/someone");
        for folder in [
            "Desktop/voices",
            "Documents",
            "Library/Mobile Documents/com~apple~CloudDocs/voices",
            "Library/CloudStorage/Dropbox/voices",
        ] {
            assert!(synced_in(&home.join(folder), home), "{folder}");
        }
        for folder in [
            "Library/Application Support/boubou/voices",
            "Voices",
            "DesktopVoices",
        ] {
            assert!(!synced_in(&home.join(folder), home), "{folder}");
        }
    }

    #[test]
    fn names_are_lowercase_letters_digits_dashes_and_underscores() {
        for name in ["me", "mom-2", "uncle_bo", "7", &"a".repeat(32)] {
            assert!(named(name), "{name}");
        }
        for name in ["", "Mom", "my voice", "-me", "_me", "妈妈", &"a".repeat(33)] {
            assert!(!named(name), "{name}");
        }
        assert_eq!(suggest("My Voice (take 2)"), "my-voice-take-2");
        assert_eq!(suggest("--Grandpa--"), "grandpa");
        assert_eq!(suggest("妈妈"), "my-voice");
        assert_eq!(suggest(&"Ab".repeat(20)).len(), 32);
        assert_eq!(misnamed("me"), None);
        assert!(misnamed("Ryan").unwrap().starts_with("Rename it ryan.wav"));
        assert!(misnamed("ryan").unwrap().contains("built-in"));
    }

    #[test]
    fn dropped_files_are_listed_with_what_to_do() {
        let dir = TestDir::new("scan");
        let folder = &dir.0;
        keep(folder, "me", "en");
        keep(folder, "mom", "zh");
        // A recording dropped in, waiting for its words.
        fs::copy(folder.join("me.wav"), folder.join("dad.wav")).unwrap();
        // A name the page would not give, a built-in voice's, and a clip that is not WAV.
        fs::copy(folder.join("me.wav"), folder.join("My Voice.wav")).unwrap();
        fs::copy(folder.join("me.wav"), folder.join("aiden.wav")).unwrap();
        fs::write(folder.join("kid.m4a"), "m4a").unwrap();
        // Extensions in capitals, which file systems that tell case apart would not find.
        fs::copy(folder.join("me.wav"), folder.join("loud.WAV")).unwrap();
        fs::copy(folder.join("me.wav"), folder.join("cat.wav")).unwrap();
        fs::copy(folder.join("me.json"), folder.join("cat.JSON")).unwrap();
        // Unreadable audio, a recording too short, words without a recording, and words
        // that cannot be read.
        fs::write(folder.join("bad.wav"), "not audio").unwrap();
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut short = hound::WavWriter::create(folder.join("short.wav"), spec).unwrap();
        for _ in 0..48000 * 2 {
            short.write_sample(1000i16).unwrap();
        }
        short.finalize().unwrap();
        fs::copy(folder.join("me.json"), folder.join("gone.json")).unwrap();
        fs::copy(folder.join("me.wav"), folder.join("pet.wav")).unwrap();
        fs::write(
            folder.join("pet.json"),
            r#"{"text":"Kaixo.","language":"eu"}"#,
        )
        .unwrap();
        // Hidden and other files are not voices.
        fs::write(folder.join(".kid.wav.part"), "half").unwrap();
        fs::write(folder.join("notes.txt"), "mine").unwrap();

        let found = scan(folder);
        let names: Vec<_> = found
            .voices
            .iter()
            .map(|voice| voice.name.as_str())
            .collect();
        assert_eq!(names, ["me", "mom"]);
        assert_eq!(found.voices[1].speaks(), "Chinese");
        assert_eq!(found.unheard, ["dad"]);
        let attention: Vec<_> = found
            .attention
            .iter()
            .map(|(file, _)| file.as_str())
            .collect();
        assert_eq!(
            attention,
            [
                "My Voice.wav",
                "aiden.wav",
                "bad.wav",
                "cat.JSON",
                "gone.json",
                "kid.m4a",
                "loud.WAV",
                "pet.json",
                "short.wav"
            ]
        );
        let hint = |file| {
            &found
                .attention
                .iter()
                .find(|(name, _)| name == file)
                .unwrap()
                .1
        };
        assert!(hint("My Voice.wav").starts_with("Rename it my-voice.wav"));
        assert!(hint("bad.wav").starts_with("Boubou cannot read"));
        assert!(hint("short.wav").starts_with("It lasts 1 s. Give 3 to 30"));
        assert!(hint("gone.json").contains("gone.wav, is missing"));
        assert!(hint("kid.m4a").starts_with("Only WAV files"));
        assert_eq!(hint("loud.WAV"), "Rename it loud.wav, in lowercase.");
        assert_eq!(hint("cat.JSON"), "Rename it cat.json, in lowercase.");
        // Tidying passes them by, and the voices stay as they are.
        tidy(folder).unwrap();
        assert_eq!(scan(folder).voices.len(), 2);
        assert!(scan(&folder.join("nowhere")).voices.is_empty());
    }

    #[test]
    fn a_renamed_recording_keeps_its_words() {
        let dir = TestDir::new("rename");
        let folder = &dir.0;
        keep(folder, "me", "zh");
        tidy(folder).unwrap();
        let words: Value =
            serde_json::from_slice(&fs::read(folder.join("me.json")).unwrap()).unwrap();
        assert_eq!(words["text"], "Hello.");
        assert_eq!(
            words["sha256"],
            digest(&fs::read(folder.join("me.wav")).unwrap())
        );

        fs::rename(folder.join("me.wav"), folder.join("alice.wav")).unwrap();
        assert_eq!(scan(folder).unheard, ["alice"]);
        tidy(folder).unwrap();
        let found = scan(folder);
        assert_eq!(found.voices.len(), 1);
        assert_eq!(
            (found.voices[0].name.as_str(), found.voices[0].language),
            ("alice", "zh")
        );
        assert!(found.attention.is_empty());

        // A recording of something else gets no words that are not its own.
        keep(folder, "bob", "en");
        tidy(folder).unwrap();
        fs::rename(folder.join("bob.wav"), folder.join("robert.wav")).unwrap();
        fs::copy(folder.join("alice.wav"), folder.join("carol.wav")).unwrap();
        fs::remove_file(folder.join("alice.json")).unwrap();
        tidy(folder).unwrap();
        let found = scan(folder);
        assert_eq!(found.voices[0].name, "robert");
        assert_eq!(found.unheard, ["alice", "carol"]);
    }

    #[test]
    fn each_voice_gets_a_launcher_to_double_click() {
        use std::os::unix::fs::PermissionsExt;

        // A folder that only this test names, with characters a shell would misread.
        let dir = TestDir::new("launcher's room");
        let folder = &dir.0;
        keep(folder, "mom", "zh");
        keep(folder, "dad", "en");
        // A person's own launcher keeps its content, even under a voice's name.
        fs::write(folder.join("dad.command"), "#!/bin/sh\necho mine\n").unwrap();
        launchers(folder, &scan(folder).voices).unwrap();
        let mom = fs::read_to_string(folder.join("mom.command")).unwrap();
        assert!(mom.starts_with(MADE));
        assert!(mom.contains("exec \"${SHELL:-/bin/zsh}\" -lic "), "{mom}");
        let mode = fs::metadata(folder.join("mom.command"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111);
        assert_eq!(
            fs::read_to_string(folder.join("dad.command")).unwrap(),
            "#!/bin/sh\necho mine\n"
        );
        assert_eq!(launcher_of(folder, "mom"), Some(folder.join("mom.command")));
        assert_eq!(launcher_of(folder, "kid"), None);

        // A renamed voice's launcher follows it; a person's own stays when its voice is gone.
        fs::rename(folder.join("mom.wav"), folder.join("mother.wav")).unwrap();
        fs::rename(folder.join("mom.json"), folder.join("mother.json")).unwrap();
        fs::remove_file(folder.join("dad.wav")).unwrap();
        launchers(folder, &scan(folder).voices).unwrap();
        assert!(!folder.join("mom.command").exists());
        assert!(folder.join("mother.command").is_file());
        assert!(folder.join("dad.command").is_file());

        // The launcher runs the command it names in the person's own login shell.
        let shell = folder.join("shell");
        fs::write(&shell, "#!/bin/sh\nprintf '%s|' \"$@\" > \"$0.args\"\n").unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o755)).unwrap();
        let ran = std::process::Command::new(folder.join("mother.command"))
            .env("SHELL", &shell)
            .status()
            .unwrap();
        assert!(ran.success());
        let flag = format!(" --voices-dir {}", quote(folder));
        let talk = format!("boubou run {} --voice mother{flag}", model());
        assert_eq!(
            fs::read_to_string(folder.join("shell.args")).unwrap(),
            format!("-lic|{talk}|")
        );
        let words = shlex::split(&talk).unwrap();
        assert_eq!(words.last().map(PathBuf::from).as_ref(), Some(folder));

        // The commands to type find the folder too; the folder Boubou keeps voices in needs no flag.
        assert_eq!(elsewhere(folder), flag);
        assert!(commands("mother", &elsewhere(folder))[1].1.ends_with(&flag));
        assert_eq!(elsewhere(&kept(&data().unwrap())), "");
    }

    #[test]
    fn exported_voices_import_under_free_names() {
        let dir = TestDir::new("zip");
        let (here, there) = (dir.0.join("here"), dir.0.join("there"));
        keep(&here, "me", "en");
        keep(&here, "mom", "zh");
        let zip = dir.0.join("voices.zip");
        assert_eq!(export(&here, &[], &zip).unwrap(), ["me", "mom"]);
        assert!(
            export(&here, &["dad".into()], &zip)
                .unwrap_err()
                .to_string()
                .contains("dad")
        );
        assert!(export(&there, &[], &zip).is_err());

        // Into an empty folder, every voice keeps its name.
        let added = import(&there, &zip).unwrap();
        assert_eq!(
            added.added,
            [("me".into(), "me".into()), ("mom".into(), "mom".into())]
        );
        assert_eq!(scan(&there).voices, scan(&here).voices);

        // A voice already here is left out; another voice with a taken name gets the next free one.
        // Here, mom and mom-2 are recordings of someone else.
        keep(&there, "dad", "en");
        fs::copy(there.join("dad.wav"), there.join("mom.wav")).unwrap();
        fs::rename(there.join("dad.wav"), there.join("mom-2.wav")).unwrap();
        fs::rename(there.join("dad.json"), there.join("mom.json")).unwrap();
        let added = import(&there, &zip).unwrap();
        assert_eq!(added.added, [("mom".into(), "mom-3".into())]);
        assert_eq!(
            added.skipped,
            [("me.wav".into(), "me is already here.".into())]
        );
        let mom = scan(&there)
            .voices
            .into_iter()
            .find(|voice| voice.name == "mom-3")
            .unwrap();
        assert_eq!(mom.language, "zh");

        // A zip file from elsewhere: names to fix, a built-in voice's, folders, recordings
        // without words, words without recordings and files that are not voices.
        let zip = dir.0.join("other.zip");
        let mut writer = zip::ZipWriter::new(fs::File::create(&zip).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        let recording = fs::read(here.join("me.wav")).unwrap();
        let mut other = recording.clone();
        *other.last_mut().unwrap() ^= 1;
        for (file, content) in [
            ("Voices/My Voice.wav", other.as_slice()),
            ("ryan.wav", &recording[..recording.len() - 2]),
            ("__MACOSX/._ryan.wav", b"resource"),
            ("lone.json", b"{}"),
            ("noise.wav", b"noise"),
            ("readme.txt", b"hi"),
        ] {
            writer.start_file(file, options).unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap();
        let added = import(&there, &zip).unwrap();
        assert_eq!(
            added.added,
            [
                ("My Voice".into(), "my-voice".into()),
                ("ryan".into(), "ryan-2".into())
            ]
        );
        let skipped: Vec<_> = added
            .skipped
            .iter()
            .map(|(file, _)| file.as_str())
            .collect();
        assert_eq!(skipped, ["lone.json", "noise.wav"]);
        assert_eq!(scan(&there).unheard, ["mom-2", "my-voice", "ryan-2"]);
        assert!(import(&there, &here.join("me.wav")).is_err());
    }
}
