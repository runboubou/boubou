#![cfg(target_os = "macos")]

use std::os::unix::process::CommandExt;
use std::{
    fs,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Package(PathBuf);

impl Package {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "boubou package {} {} {}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("cli.cjs"), include_str!("../packaging/cli.cjs")).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_boubou"), root.join("boubou")).unwrap();
        Self(root)
    }

    fn command(&self) -> Command {
        let mut command = Command::new("node");
        command.arg(self.0.join("cli.cjs"));
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn script(&self, text: &str) {
        let path = self.0.join("boubou");
        fs::write(&path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

impl Drop for Package {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const MENU: &[&str] = &[
    "Choose an English voice for this Mac",
    "No English voice fits",
    "Device check unavailable",
];

struct Prompt {
    child: Child,
    master: fs::File,
    _slave: fs::File,
    output: String,
    original: libc::termios,
}

impl Prompt {
    fn new(package: &Package, no_color: bool, args: &[&str]) -> Self {
        Self::start(package, package.command(), no_color, args, true)
    }

    /// Starts the native binary; without `controlling`, closing the terminal sends no SIGHUP.
    fn native(package: &Package, controlling: bool) -> Self {
        let command = Command::new(package.0.join("boubou"));
        Self::start(package, command, false, &[], controlling)
    }

    fn start(
        package: &Package,
        mut command: Command,
        no_color: bool,
        args: &[&str],
        controlling: bool,
    ) -> Self {
        let (mut master, mut slave) = (-1, -1);
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size,
                )
            },
            0
        );
        let master = unsafe { fs::File::from_raw_fd(master) };
        let slave = unsafe { fs::File::from_raw_fd(slave) };
        let mut original = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut original) },
            0
        );
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        // Only this harness may hold the terminal, so closing `master` really closes it.
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        command
            .args([
                "--cache-dir",
                package.0.join("menu-cache").to_str().unwrap(),
            ])
            .args(args)
            .env("TERM", "xterm-256color")
            .env_remove("NO_COLOR")
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        if no_color {
            command.env("NO_COLOR", "1");
        }
        if controlling {
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY.into(), 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let child = command.spawn().unwrap();
        Self {
            child,
            master,
            _slave: slave,
            output: String::new(),
            original,
        }
    }

    fn expect(&mut self, choices: &[&str]) {
        // Parallel fixtures launch fresh Node/native copies on busy macOS workers.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let mut buffer = [0; 8192];
            match self.master.read(&mut buffer) {
                Ok(n) => self.output.push_str(&String::from_utf8_lossy(&buffer[..n])),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("PTY read failed: {e}; {}", self.output),
            }
            if choices.iter().any(|text| self.output.contains(text)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Missing {choices:?} (launcher status: {:?}): {}",
                self.child.try_wait().unwrap(),
                self.output
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&mut self, text: &[u8]) {
        self.output.clear();
        self.master.write_all(text).unwrap();
    }

    fn exit_within(&mut self, limit: Duration) -> std::process::ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            let mut buffer = [0; 8192];
            let _ = self.master.read(&mut buffer);
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "Prompt did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish(&mut self) -> std::process::ExitStatus {
        let status = self.exit_within(Duration::from_secs(5));
        let mut restored = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(
            unsafe { libc::tcgetattr(self.master.as_raw_fd(), &mut restored) },
            0
        );
        assert_eq!(
            self.original.c_lflag, restored.c_lflag,
            "Terminal flags were not restored"
        );
        assert_eq!(self.original.c_iflag, restored.c_iflag);
        status
    }
}

impl Drop for Prompt {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
            for _ in 0..100 {
                let mut buffer = [0; 8192];
                let _ = self.master.read(&mut buffer);
                if self.child.try_wait().unwrap().is_some() {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn guided_start_opens_english_voices_and_restores_the_terminal() {
    let package = Package::new();
    let mut prompt = Prompt::new(&package, false, &[]);
    prompt.expect(MENU);
    assert!(!prompt.output.contains("Chinese"));
    assert!(!prompt.output.contains("Choose your conversation language"));
    assert!(prompt.output.contains("Developer: choose a model"));
    assert!(prompt.output.contains("\x1b[38;"), "No color emphasis");
    // The developer entry is last regardless of the number of eligible voices.
    prompt.send(b"\x1b[A\r");
    prompt.expect(&["Paste a model name"]);
    prompt.send(b"\x1b");
    prompt.expect(MENU);
    prompt.send(b"\x03");
    assert_eq!(prompt.finish().code(), Some(130));
    assert!(!package.0.join("menu-cache").exists());
}

#[test]
fn guided_start_accepts_a_pasted_offline_command_after_an_error() {
    let package = Package::new();
    let mut prompt = Prompt::new(&package, true, &[]);
    prompt.expect(MENU);
    assert!(!prompt.output.contains("\x1b[38;"), "NO_COLOR was ignored");
    prompt.send(b"\x1b[A\r");
    prompt.expect(&["Paste a model name"]);
    prompt.send(b"unknown\r");
    prompt.expect(&["Model not supported"]);
    assert!(!package.0.join("menu-cache").exists());
    prompt.send(b"\x15\x1b[200~npx --yes --cache \"$HOME/a cache\" boubou@beta run LFM3-Audio-2B --offline\x1b[201~\r");
    prompt.expect(&["offline mode is enabled"]);
    assert!(prompt.output.contains("run `boubou` again"));
    assert_eq!(prompt.finish().code(), Some(1));
    assert!(!package.0.join("menu-cache/native").exists());
    assert!(!package.0.join("menu-cache/models").exists());
}

#[test]
fn guided_start_restores_the_terminal_when_the_launcher_is_terminated() {
    let package = Package::new();
    let mut prompt = Prompt::new(&package, false, &[]);
    prompt.expect(MENU);
    assert_eq!(
        unsafe { libc::kill(prompt.child.id() as i32, libc::SIGTERM) },
        0
    );
    assert_eq!(prompt.finish().signal(), Some(15));
    assert!(!package.0.join("menu-cache").exists());
}

#[test]
fn guided_start_exits_when_its_terminal_closes() {
    let package = Package::new();
    // A closed window hangs up its session; an orphaned process may keep a dead terminal instead.
    for controlling in [true, false] {
        let mut prompt = Prompt::native(&package, controlling);
        prompt.expect(MENU);
        prompt._slave = fs::File::open("/dev/null").unwrap();
        prompt.master = fs::File::open("/dev/null").unwrap();
        let status = prompt.exit_within(Duration::from_secs(2));
        assert!(
            !status.success(),
            "{status:?} with controlling={controlling}"
        );
    }
}

#[test]
fn guided_start_exits_promptly_when_terminated() {
    let package = Package::new();
    let mut prompt = Prompt::native(&package, true);
    prompt.expect(MENU);
    assert_eq!(
        unsafe { libc::kill(prompt.child.id() as i32, libc::SIGTERM) },
        0
    );
    assert_eq!(prompt.exit_within(Duration::from_secs(2)).code(), Some(130));
}

#[test]
fn guided_start_redraws_when_the_terminal_resizes() {
    let package = Package::new();
    let mut prompt = Prompt::native(&package, true);
    prompt.expect(MENU);
    prompt.output.clear();
    let size = libc::winsize {
        ws_row: 30,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe { libc::ioctl(prompt.master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
        0
    );
    // A resize sends no input, so the picker must notice it without a key press.
    prompt.expect(MENU);
}

#[test]
fn guided_start_filters_incompatible_packaged_voices_before_download() {
    let package = Package::new();
    let release = package.0.join("qwen");
    fs::create_dir(&release).unwrap();
    fs::write(
        release.join("runtime.json"),
        serde_json::json!({
            "url":"https://example.invalid/runner.tar.gz", "size":1,
            "sha256":"a".repeat(64), "min_macos":"99.0"
        })
        .to_string(),
    )
    .unwrap();
    let files: Vec<_> = [
        "qwen3-tts:0.6b-customvoice",
        "qwen3-tts:1.7b-customvoice",
        "pocket-tts:english_2026-10",
        "phonon:31m-en",
        "mms-tts:eng",
    ]
    .iter()
    .enumerate()
    .map(|(index, id)| {
        serde_json::json!({
            "profile": id, "path": format!("models/{index}.bin"),
            "url": "https://example.invalid/model.bin", "size": 1, "sha256": "b".repeat(64)
        })
    })
    .collect();
    fs::write(
        release.join("models.json"),
        serde_json::to_vec(&files).unwrap(),
    )
    .unwrap();
    let mut prompt = Prompt::new(&package, true, &[]);
    prompt.expect(MENU);
    for label in [
        "Qwen 0.6B / Aiden",
        "Qwen 1.7B / Aiden",
        "Pocket TTS English",
        "Phonon 31M English",
    ] {
        assert!(!prompt.output.contains(label));
    }
    assert!(!prompt.output.contains("Meta MMS / English"));
    assert!(!prompt.output.contains("Chinese"));
    prompt.send(b"\x1b");
    assert_eq!(prompt.finish().code(), Some(130));
    assert!(!package.0.join("menu-cache").exists());
}

#[test]
fn cached_model_verification_shows_live_progress_before_loading() {
    let package = Package::new();
    let model_dir = package
        .0
        .join("menu-cache/models/lfm3-audio-2b/02355c1a141e464a064f2ab2c276638dbcb7f38a");
    fs::create_dir_all(&model_dir).unwrap();
    // Sparse, deliberately unverified fixture: exercise the real hash-reading wait without weights.
    fs::File::create(model_dir.join("LFM3-Audio-2B-Q4_K_M.gguf"))
        .unwrap()
        .set_len(1_099_626_016)
        .unwrap();
    let mut prompt = Prompt::new(&package, true, &["doctor", "LFM3-Audio-2B"]);
    prompt.expect(&["Verifying LFM3-Audio-2B-Q4_K_M.gguf"]);
    prompt.expect(&["%"]);
    prompt.send(b"");
    prompt.expect(&["%"]); // A second frame arrives while the checksum is still running.
    assert!(!prompt.output.contains("\x1b[38;"), "NO_COLOR was ignored");
    assert_eq!(
        unsafe { libc::kill(prompt.child.id() as i32, libc::SIGTERM) },
        0
    );
    assert_eq!(prompt.finish().signal(), Some(15));
    assert!(!package.0.join("menu-cache/downloads").exists());
}

#[test]
fn packaged_qwen_recovers_partial_assets_and_starts_its_server_offline() {
    use flate2::{Compression, write::GzEncoder};

    if !cfg!(target_arch = "aarch64") {
        return;
    }
    let package = Package::new();
    let release = package.0.join("qwen");
    let cache = package.0.join("menu-cache/qwen");
    fs::create_dir(&release).unwrap();
    let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    let script = b"#!/bin/sh\nprintf 'selected: %s\\n' \"$@\"\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(script.len() as u64);
    header.set_mode(0o755);
    archive
        .append_data(&mut header, "bin/python3", &script[..])
        .unwrap();
    let bytes = archive.into_inner().unwrap().finish().unwrap();
    let digest = sha256_hex(&bytes);
    let engine = cache.join("runtimes").join(&digest);
    fs::create_dir_all(&engine).unwrap();
    fs::write(engine.join("runner.tar.gz"), &bytes).unwrap();
    fs::write(
        release.join("runtime.json"),
        serde_json::json!({
            "url": "https://example.invalid/runner.tar.gz",
            "sha256": digest, "size": bytes.len(), "min_macos": "13.3"
        })
        .to_string(),
    )
    .unwrap();
    let id = "qwen3-tts:1.7b-customvoice";
    let asset = b"pinned fixture";
    let snapshot = "hub/models--fixture--qwen/snapshots/abc";
    let files = serde_json::json!([{
        "profile": id, "path": format!("{snapshot}/tts.bin"), "size": asset.len(),
        "sha256": sha256_hex(asset), "url": "https://example.invalid/tts.bin"
    }]);
    fs::write(release.join("models.json"), files.to_string()).unwrap();
    let snapshot = cache.join(snapshot);
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join("tts.bin.part"), asset).unwrap();
    // The fixture runner exits instead of serving; Boubou reports its arguments and output.
    let mut prompt = Prompt::new(&package, true, &["run", id, "--offline"]);
    for line in [
        "The Qwen voice runner stopped",
        "selected: qwen3-tts:1.7b-customvoice",
        "selected: --port",
    ] {
        prompt.expect(&[line]);
    }
    assert!(!prompt.finish().success());
    assert_eq!(fs::read(snapshot.join("tts.bin")).unwrap(), asset);
    assert!(!snapshot.join("tts.bin.part").exists());
}

#[test]
fn qwen_package_lists_its_models_and_doctor_is_read_only() {
    let package = Package::new();
    let release = package.0.join("qwen");
    fs::create_dir(&release).unwrap();
    fs::write(
        release.join("runtime.json"),
        serde_json::json!({
            "url": "https://example.invalid/runner.tar.gz",
            "sha256": "a".repeat(64), "size": 1, "min_macos": "13.3"
        })
        .to_string(),
    )
    .unwrap();
    let ids = [
        ("qwen3-tts:1.7b-customvoice", "English"),
        ("qwen3-tts:0.6b-customvoice", "English"),
        ("kokoro:82m", "English"),
        ("kokoro:82m-9b", "English"),
        ("kokoro:82m-27b", "English"),
        ("phonon:31m-en", "English"),
        ("supertonic:3", "English"),
        ("chattts:en", "English"),
        ("chattts:whisper", "Chinese / English (Whisper)"),
        (
            "qwen3-tts:1.7b-customvoice-whisper",
            "Chinese / English (Whisper)",
        ),
        (
            "qwen3-tts:0.6b-customvoice-whisper",
            "Chinese / English (Whisper)",
        ),
        ("pocket-tts:english_2m_2026-10", "English"),
        ("pocket-tts:french_24l", "French"),
        ("kitten-tts:nano-0.8-int8", "English"),
    ];
    let files: Vec<_> = ids
        .iter()
        .map(|(id, _)| {
            serde_json::json!({
                "profile": id, "path": format!("models/{id}/config.json"), "size": 7,
                "url": "https://example.invalid/config.json", "sha256": "b".repeat(64)
            })
        })
        .collect();
    fs::write(
        release.join("models.json"),
        serde_json::to_vec(&files).unwrap(),
    )
    .unwrap();
    let models = package.run(&["list"]);
    let model_list = String::from_utf8(models.stdout).unwrap();
    let pocket = model_list.lines().filter(|l| l.starts_with("pocket-tts:"));
    assert_eq!(pocket.count(), 26);
    let kitten = model_list.lines().filter(|l| l.starts_with("kitten-tts:"));
    assert_eq!(kitten.count(), 7);
    let cache = package.0.join("unused-cache");
    for (id, language) in ids {
        assert!(model_list.lines().any(|line| line == id));
        let doctor = package.run(&["--cache-dir", cache.to_str().unwrap(), "doctor", id]);
        assert_eq!(doctor.status.code(), Some(1));
        let report = String::from_utf8_lossy(&doctor.stdout);
        assert!(
            report.contains(&format!("{id} · {language} voice chat")),
            "{report}"
        );
        assert!(!cache.exists());
    }
}

#[test]
fn qwen_microphone_profiles_reject_liquid_file_input_before_downloading() {
    let package = Package::new();
    let release = package.0.join("qwen");
    fs::create_dir(&release).unwrap();
    fs::write(release.join("runtime.json"), "{}").unwrap();
    fs::write(release.join("models.json"), "[]").unwrap();
    let cache = package.0.join("qwen-cache");
    for id in [
        "qwen3-tts:1.7b-customvoice",
        "qwen3-tts:0.6b-customvoice",
        "kokoro:82m",
        "phonon:31m-en",
        "supertonic:3",
        "chattts:en",
    ] {
        let output = package.run(&[
            "--cache-dir",
            cache.to_str().unwrap(),
            "run",
            id,
            "--input",
            "missing.wav",
        ]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Qwen voice chat"));
        assert!(!cache.exists());
    }
}

#[test]
fn npm_launcher_runs_the_native_cli_and_rejects_unknown_models() {
    let package = Package::new();
    let version = package.run(&["--version"]);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("boubou "));
    let help = package.run(&["run", "--help"]);
    assert!(help.status.success(), "{help:?}");
    assert!(String::from_utf8_lossy(&help.stdout).contains("LFM3-Audio-2B"));
    let models = package.run(&["list"]);
    assert!(models.status.success());
    assert_eq!(
        models.stdout,
        b"LFM3-Audio-2B\nLFM3-Omni-2B\nLFM2.5-Audio-1.5B\n"
    );
    let cache = package.0.join("unused-cache");
    let cache_arg = cache.to_str().unwrap();
    for command in ["run", "pull", "doctor"] {
        for model in ["unknown", "LFM2.5-Audio-1.5B-JP"] {
            assert_eq!(
                package
                    .run(&["--cache-dir", cache_arg, command, model])
                    .status
                    .code(),
                Some(2)
            );
            assert!(!cache.exists(), "Rejected model created a cache");
        }
    }
    let doctor = package.run(&["--cache-dir", cache_arg, "doctor"]);
    let explicit = package.run(&["--cache-dir", cache_arg, "doctor", "LFM3-Audio-2B"]);
    assert_eq!(doctor.status.code(), Some(1));
    assert_eq!(doctor.stdout, explicit.stdout);
    assert_eq!(doctor.stderr, explicit.stderr);
    let omni = package.run(&["--cache-dir", cache_arg, "doctor", "LFM3-Omni-2B"]);
    assert_eq!(omni.status.code(), Some(1));
    let text = String::from_utf8_lossy(&omni.stdout);
    assert!(text.contains("Profile: Liquid AI LFM3-Omni-2B"));
    assert!(text.contains("mmproj-LFM3-Omni-2B-Q8_0.gguf"));
    assert!(!text.contains("mmproj-LFM3-Audio"));
    let doctor = package.run(&["--cache-dir", cache_arg, "doctor", "LFM2.5-Audio-1.5B"]);
    assert_eq!(doctor.status.code(), Some(1));
    let text = String::from_utf8_lossy(&doctor.stdout);
    assert!(text.contains("Profile: Liquid AI LFM2.5-Audio-1.5B"));
    assert!(text.contains("tokenizer-LFM2.5-Audio-1.5B-Q8_0.gguf"));
    assert!(text.contains("vocoder-LFM2.5-Audio-1.5B-Q8_0.gguf"));
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("LFM3-Omni-2B"));
    assert!(!cache.exists(), "Doctor mutated the cache");
    let help = package.run(&["--cache-dir", cache_arg]);
    assert!(help.status.success());
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("Usage: boubou"));
    assert!(!text.contains('\u{1b}'));
    assert!(!cache.exists(), "Headless startup mutated the cache");
    for command in ["run", "pull"] {
        let result = package.run(&[
            "--cache-dir",
            cache_arg,
            command,
            "LFM3-Omni-2B",
            "--offline",
        ]);
        assert_eq!(result.status.code(), Some(1));
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            error.contains(if command == "run" {
                "needs a terminal"
            } else {
                "offline mode is enabled"
            }),
            "{error}"
        );
    }
    assert!(!cache.join("models").exists());
    assert!(!cache.join("native").exists());
    assert!(!cache.join("runtimes").exists());
    assert_eq!(package.run(&["run", "liquid"]).status.code(), Some(2));
    assert_eq!(package.run(&["run", "other"]).status.code(), Some(2));
    assert_eq!(package.run(&["run"]).status.code(), Some(2));
}

#[test]
fn npm_exposes_the_product_name() {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../packaging/package.json")).unwrap();
    assert_eq!(manifest["bin"], serde_json::json!({"boubou":"cli.cjs"}));
}

#[test]
fn npm_package_leaves_the_qwen_runner_to_the_release() {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../packaging/package.json")).unwrap();
    let files = manifest["files"].as_array().unwrap();
    assert!(
        files
            .iter()
            .all(|file| !file.as_str().unwrap().ends_with(".tar.gz"))
    );
    assert!(include_str!("../packaging/build-macos.py").contains("if size > MAX_PACKED:"));
}

#[test]
fn media_questions_reject_invalid_selection_without_cache_or_microphone_access() {
    let package = Package::new();
    let cache = package.0.join("unused-media-cache");
    for (model, code, message) in [
        ("unknown", 2, "invalid value"),
        ("LFM3-Audio-2B", 1, "does not support image/video"),
        ("LFM3-Omni-2B", 1, "Cannot read"),
    ] {
        let output = package.run(&[
            "--cache-dir",
            cache.to_str().unwrap(),
            "ask",
            model,
            "missing.png",
            "What is here?",
        ]);
        assert_eq!(output.status.code(), Some(code));
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
        assert!(!cache.exists());
    }
}

#[test]
fn npm_launcher_preserves_arguments_with_spaces_and_exit_codes() {
    let package = Package::new();
    package.script("#!/usr/bin/env node\nconsole.log(JSON.stringify(process.argv.slice(2))); process.exit(7);\n");
    let result = package.run(&["--input", "a question.wav", "--offline"]);
    assert_eq!(result.status.code(), Some(7), "{result:?}");
    let arguments: Vec<String> = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(arguments, ["--input", "a question.wav", "--offline"]);
}

#[test]
fn npm_launcher_reports_a_missing_executable() {
    let package = Package::new();
    fs::remove_file(package.0.join("boubou")).unwrap();
    let result = package.run(&["--version"]);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("Could not start boubou"));
}

#[test]
fn npm_launcher_waits_for_cleanup_before_propagating_termination() {
    let package = Package::new();
    package.script(
        "#!/usr/bin/env node\n\
        const fs = require('node:fs');\n\
        process.on('SIGINT', () => { fs.appendFileSync(process.argv[2], ':clean'); process.exit(0); });\n\
        fs.writeFileSync(process.argv[2], String(process.pid));\n\
        setTimeout(() => process.exit(99), 5000);\n",
    );
    let ready = package.0.join("ready");
    let mut launcher = package
        .command()
        .arg(&ready)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if ready.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !ready.exists() {
        let _ = launcher.kill();
        let _ = launcher.wait();
        panic!("The fixture child did not become ready");
    }
    let child_pid = fs::read_to_string(&ready).unwrap();
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &launcher.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(launcher.wait().unwrap().signal(), Some(15));
    assert!(fs::read_to_string(ready).unwrap().ends_with(":clean"));
    assert!(
        !Command::new("/bin/kill")
            .args(["-0", &child_pid])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn qwen_runner_preserves_user_turns_and_reaches_loopback_with_a_proxy() {
    let output = Command::new("python3")
        .args(["-I", "-B", "-c"])
        .arg(
            r#"import json, os, runpy, sys, types, urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from tempfile import TemporaryDirectory
from threading import Thread

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(204)
        self.end_headers()

    def log_message(self, *args):
        pass

for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY",
            "http_proxy", "https_proxy", "all_proxy"):
    os.environ[key] = "http://127.0.0.1:1"
os.environ["NO_PROXY"] = os.environ["no_proxy"] = "kept.invalid"

with HTTPServer(("127.0.0.1", 0), Handler) as server:
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        def run_pipeline(command, argv):
            assert command == "serve"
            messages = []
            for text in ("Tell me a long story.", "Stop. What is the capital of France?"):
                chat._append_user_message(messages, text)
            assert [message.role for message in messages] == ["user", "user"], messages
            assert [message.content for message in messages] == [
                "Tell me a long story.", "Stop. What is the capital of France?"
            ], messages
            with urllib.request.urlopen(
                f"http://127.0.0.1:{server.server_port}/v1/realtime", timeout=2
            ) as response:
                assert response.status == 204
            for host in ("localhost", "::1", "kept.invalid"):
                assert urllib.request.proxy_bypass(host), host

        package = types.ModuleType("speech_to_speech")
        package.__path__ = []
        pipeline = types.ModuleType("speech_to_speech.s2s_pipeline")
        pipeline.run_pipeline_command = run_pipeline
        llm = types.ModuleType("speech_to_speech.LLM")
        llm.__path__ = []
        chat = types.ModuleType("speech_to_speech.LLM.chat")
        class UserMessage:
            role = "user"
            def __init__(self, content):
                self.content = content
        def append_user_message(messages, content):
            # The pinned SDK coalesces adjacent user turns for strict chat templates.
            if messages and messages[-1].role == "user":
                messages[-1].content += "\n" + content
            else:
                messages.append(UserMessage(content))
        chat.TransformersUserMessage = UserMessage
        chat._append_user_message = append_user_message
        llm.chat = chat
        sys.modules["speech_to_speech"] = package
        sys.modules["speech_to_speech.s2s_pipeline"] = pipeline
        sys.modules["speech_to_speech.LLM"] = llm
        sys.modules["speech_to_speech.LLM.chat"] = chat
        with TemporaryDirectory() as folder:
            cache = Path(folder)
            manifest = cache / "models.json"
            manifest.write_text(json.dumps([
                {"path": f"hub/models--mlx-community--{name}/snapshots/pinned/config.json"}
                for name in ("parakeet-tdt-0.6b-v3", "Qwen3.5-4B-4bit")
            ] + [{
                "path": "hub/models--mlx-community--Qwen3-TTS-12Hz-1.7B-CustomVoice-6bit/snapshots/pinned/config.json",
                "profile": "qwen3-tts:1.7b-customvoice",
            }]))
            runpy.run_path(sys.argv[1])["run"](
                "serve", "qwen3-tts:1.7b-customvoice", cache, manifest
            )
    finally:
        server.shutdown()
        thread.join()
"#,
        )
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/packaging/qwen/voice.py"
        ))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn curated_runtimes_prepare_and_launch_only_their_selected_components() {
    let output = Command::new("python3")
        .args(["-I", "-B", "-c"])
        .arg(
            r#"import json, runpy, sys
from pathlib import Path
from tempfile import TemporaryDirectory

root = Path(sys.argv[1])
presets = json.loads((root / 'runtimes.json').read_text())
assert len({p['id'] for p in presets}) == len(presets)
assert {p['recommended_memory_gib'] for p in presets} == {16, 32, 64, 128}
for preset in presets:
    assert preset['memory_gib'] + 2 <= preset['recommended_memory_gib']

snapshot = lambda repo: f'hub/models--{repo}/snapshots/pinned/config.json'
legacy = ['kokoro:82m', 'qwen3-tts:1.7b-customvoice', 'qwen3-tts:0.6b-customvoice', 'mms-tts:eng', 'phonon:31m-en', 'pocket-tts:english_2026-10']
files = [
    {'path': snapshot('mlx-community--parakeet-tdt-0.6b-v3'), 'profiles': legacy},
    *[{'path': snapshot(f'mlx-community--Qwen3.5-{n}B-4bit')} for n in [2, 4, 9, 27]],
    {'path': snapshot('mlx-community--Kokoro-82M-bf16'), 'profile': legacy[0]},
    {'path': snapshot('mlx-community--Qwen3-TTS-12Hz-1.7B-CustomVoice-6bit'), 'profile': legacy[1]},
    {'path': snapshot('mlx-community--Qwen3-TTS-12Hz-0.6B-CustomVoice-6bit'), 'profile': legacy[2]},
    {'path': snapshot('facebook--mms-tts-eng'), 'profile': legacy[3]},
    {'path': snapshot('gradium--phonon-31m-en'), 'profile': legacy[4]},
    {'path': 'hub/models--kyutai--pocket-tts-without-voice-cloning/snapshots/pinned/languages/english_2026-10/embeddings/alba.safetensors', 'profile': legacy[5]},
]
lock = runpy.run_path(str(root / 'lock_models.py'))['with_runtime_profiles']
lock.__globals__['snapshot'] = lambda *args: iter([{'path': snapshot('openai--whisper-tiny')}])
files = lock(files)
arguments = runpy.run_path(str(root / 'voice.py'))['arguments']
with TemporaryDirectory() as folder:
    cache = Path(folder)
    manifest = cache / 'models.json'
    manifest.write_text(json.dumps(files))
    for i, preset in enumerate(presets):
        selected = [f for f in files if preset['id'] in f['profiles']]
        assert len(selected) == 3, (preset['id'], selected)
        assert len([f for f in selected if 'Qwen3.5-' in f['path']]) == 1
        work = cache / str(i)
        work.mkdir()
        argv = arguments(preset['id'], cache, manifest, work)
        llm = argv[argv.index('--model_name') + 1]
        assert Path(llm).name == preset['llm'], argv
        if preset['stt'] == 'whisper-tiny':
            assert argv[argv.index('--stt') + 1] == 'whisper'
            assert '--parakeet_tdt_model_name' not in argv
            assert Path(argv[argv.index('--stt_model_name') + 1]).name == 'models--openai--whisper-tiny'
        else:
            assert argv[argv.index('--stt') + 1] == 'parakeet-tdt'
        assert argv[argv.index('--tts') + 1] == {'kokoro': 'kokoro', 'qwen3-tts': 'qwen3', 'mms-tts': 'facebookMMS', 'phonon': 'phonon', 'pocket-tts': 'pocket'}[preset['tts'].split(':')[0]]
        assert len(list(work.iterdir())) == (2 if preset['tts'].startswith('pocket-tts:') else 3)
    for id in legacy:
        selected = [f for f in files if id in f['profiles']]
        assert len(selected) == 3 and any('Qwen3.5-4B-4bit' in f['path'] for f in selected)
"#,
        )
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/packaging/qwen"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn runner_speaks_with_the_model_each_profile_owns() {
    let output = Command::new("python3")
        .args(["-I", "-B", "-c"])
        .arg(
            r#"import json, os, runpy, sys
from pathlib import Path
from tempfile import TemporaryDirectory

arguments = runpy.run_path(sys.argv[1])["arguments"]
owned = {
    "qwen3-tts:1.7b-customvoice": ("qwen3", "mlx-community--Qwen3-TTS-12Hz-1.7B-CustomVoice-6bit",
                                   ["--qwen3_tts_speaker", "Aiden", "--qwen3_tts_language", "english"]),
    "kokoro:82m": ("kokoro", "mlx-community--Kokoro-82M-bf16", ["--kokoro_lang_code", "a"]),
    "kitten-tts:nano-0.1": ("kitten", "KittenML--kitten-tts-nano-0.1", ["--kitten_voice", "expr-voice-3-m"]),
    "phonon:31m-en": ("phonon", "gradium--phonon-31m-en", ["--phonon_tts_language", "en"]),
}
snapshot = "hub/models--{}/snapshots/pinned".format
files = [{"path": f"{snapshot('mlx-community--' + name)}/config.json"}
         for name in ("parakeet-tdt-0.6b-v3", "Qwen3.5-4B-4bit")]
files += [{"path": f"{snapshot(repo)}/config.json", "profile": id} for id, (_, repo, _) in owned.items()]
files.append({"path": f"{snapshot('gradium--phonon-voices')}/voices.jsonl", "profile": "phonon:31m-en"})

with TemporaryDirectory() as folder:
    root = Path(folder).resolve()
    manifest = root / "models.json"
    manifest.write_text(json.dumps(files))
    for id, (tts, repo, options) in owned.items():
        work = root / tts
        work.mkdir()
        argv = arguments(id, root / "cache", manifest, work)
        at = argv.index("--tts")
        assert argv[at:at + 2] == ["--tts", tts] and argv[at + 2].endswith("_model_name"), argv
        assert os.readlink(argv[at + 3]) == str(root / "cache" / snapshot(repo)), argv
        assert argv[at + 4:at + 4 + len(options)] == options, argv
        if tts == "kokoro":
            assert argv[-1] == str(work / "Kokoro-82M-bf16/voices/af_heart.safetensors"), argv
        # Shared speech recognition and language models, plus only this profile's speech model.
        assert len(list(work.iterdir())) == 3, argv
"#,
        )
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/packaging/qwen/voice.py"
        ))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn supertonic_profile_points_the_sdk_at_its_pinned_snapshot() {
    let output = Command::new("python3")
        .args(["-I", "-B", "-c"])
        .arg(
            r#"import json, os, runpy, sys
from pathlib import Path
from tempfile import TemporaryDirectory

arguments = runpy.run_path(sys.argv[1])["arguments"]
snapshot = "hub/models--{}/snapshots/pinned".format
entries = [{"path": f"{snapshot('mlx-community--' + name)}/config.json"}
           for name in ("parakeet-tdt-0.6b-v3", "Qwen3.5-4B-4bit")]
entries.append({"path": f"{snapshot('Supertone--supertonic-3')}/config.json", "profile": "supertonic:3"})
with TemporaryDirectory() as folder:
    cache = Path(folder).resolve()
    manifest = cache / "models.json"
    manifest.write_text(json.dumps(entries))
    work = cache / "supertonic"
    work.mkdir()
    argv = arguments("supertonic:3", cache, manifest, work)
    tts = argv[argv.index("--tts"):]
    assert tts == ["--tts", "supertonic", "--supertonic_tts_voice", "M1", "--supertonic_tts_lang", "en"], argv
    # The SDK downloads only when this directory lacks its ONNX files.
    assert os.readlink(os.environ["SUPERTONIC_CACHE_DIR"]) == str(cache / snapshot("Supertone--supertonic-3"))
    assert len(list(work.iterdir())) == 3, argv
"#,
        )
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/packaging/qwen/voice.py"
        ))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn chattts_profile_loads_its_snapshot_from_a_temporary_directory() {
    let output = Command::new("python3")
        .args(["-I", "-B", "-c"])
        .arg(
            r#"import json, os, runpy, sys, types
from pathlib import Path
from tempfile import TemporaryDirectory

class Normalizer:
    def register(self, language, normalize):
        self.registered = language, normalize

class Chat:
    def __init__(self):
        self.normalizer = Normalizer()

sys.modules["ChatTTS"] = types.SimpleNamespace(Chat=Chat)
words = {1000: "one thousand", 2: "two", 3.5: "three point five", 45: "forty-five"}
sys.modules["num2words"] = types.SimpleNamespace(num2words=words.__getitem__)
arguments = runpy.run_path(sys.argv[1])["arguments"]
snapshot = "hub/models--{}/snapshots/pinned".format
entries = [{"path": f"{snapshot('mlx-community--' + name)}/config.json"}
           for name in ("parakeet-tdt-0.6b-v3", "Qwen3.5-4B-4bit")]
assets = f"{snapshot('2Noise--ChatTTS')}/asset"
entries += [{"path": f"{assets}/{name}", "profile": "chattts:en"} for name in ("DVAE.safetensors", "gpt/config.json")]
with TemporaryDirectory() as folder:
    cache = Path(folder).resolve()
    manifest = cache / "models.json"
    manifest.write_text(json.dumps(entries))
    work = cache / "chattts"
    work.mkdir()
    argv = arguments("chattts:en", cache, manifest, work)
    tts = ["--tts", "chatTTS", "--chat_tts_device", "cpu", "--stream_batch_sentences", "1"]
    assert argv[argv.index("--tts"):] == tts, argv
    # ChatTTS reads asset/ from the working directory, which must not be the verified snapshot.
    assert Path.cwd() == work and os.readlink("asset") == str(cache / assets), argv
    assert len(list(work.iterdir())) == 3, argv
    # ChatTTS would drop the digits; the handler's Chat spells them out.
    chat = sys.modules["ChatTTS"].Chat()
    language, normalize = chat.normalizer.registered
    assert language == "en"
    assert normalize("2 of 1,000 is 3.5%, or 45.") == "two of one thousand is three point five percent, or forty five."
    # The handler's per-start speaker sample returns the chosen speaker instead.
    speaker = (Path(sys.argv[1]).parent / "voices/chattts.txt").read_text().strip()
    assert len(speaker) == 859 and chat.sample_random_speaker() == chat.sample_random_speaker() == speaker
"#,
        )
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/packaging/qwen/voice.py"
        ))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn pocket_profile_loads_its_stock_config_and_preset_voice() {
    let output = Command::new("python3")
        .args(["-I", "-B", "-c"])
        .arg(
            r#"import json, runpy, sys
from pathlib import Path
from tempfile import TemporaryDirectory

arguments = runpy.run_path(sys.argv[1])["arguments"]
config, voice = "french_24l", "estelle"
languages = "hub/models--kyutai--pocket-tts-without-voice-cloning/snapshots/pinned/languages"
entries = [{"path": f"hub/models--mlx-community--{name}/snapshots/pinned/config.json"}
           for name in ("parakeet-tdt-0.6b-v3", "Qwen3.5-4B-4bit")]
entries.append({"path": f"{languages}/{config}/embeddings/{voice}.safetensors",
                "profile": f"pocket-tts:{config}"})
with TemporaryDirectory() as folder:
    cache = Path(folder).resolve()
    manifest = cache / "models.json"
    manifest.write_text(json.dumps(entries))
    work = cache / "pocket"
    work.mkdir()
    argv = arguments(f"pocket-tts:{config}", cache, manifest, work)
    option = lambda key: argv[argv.index(key) + 1]
    assert option("--tts") == "pocket" and option("--pocket_tts_language") == config
    assert option("--pocket_tts_voice") == str(cache / languages / config / "embeddings" / f"{voice}.safetensors")
    assert option("--init_chat_prompt").endswith(" Always reply in French.")
    assert sorted(p.name for p in work.iterdir()) == ["Qwen3.5-4B-4bit", "parakeet-tdt-0.6b-v3"]
    # The production lock groups ownership after sharing recipe assets.
    entries[-1]["profiles"] = [entries[-1].pop("profile")]
    entries.append({"path": f"{languages}/english_2026-10/embeddings/alba.safetensors",
                    "profiles": ["pocket-tts:english_2026-10"]})
    manifest.write_text(json.dumps(entries))
    grouped = cache / "grouped"
    grouped.mkdir()
    argv = arguments(f"pocket-tts:{config}", cache, manifest, grouped)
    assert argv[argv.index("--pocket_tts_voice") + 1] == str(cache / languages / config / "embeddings" / f"{voice}.safetensors")
"#,
        )
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/packaging/qwen/voice.py"
        ))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn remaining_families_select_only_their_pinned_local_assets() {
    let output = Command::new("python3")
        .args(["-I", "-B", "-c"])
        .arg(r#"import json, runpy, sys
from pathlib import Path
from tempfile import TemporaryDirectory

arguments = runpy.run_path(sys.argv[1])["arguments"]
snapshot = lambda repo: f"hub/models--{repo}/snapshots/pinned"
cases = [
    ("mms-tts:eng", "facebook--mms-tts-eng", "facebookMMS", "--facebook_mms_model_name", "--facebook_mms_device", "cpu"),
    ("omnivoice:0.6b", "k2-fsa--OmniVoice", "omnivoice", "--omnivoice_model_name", "--omnivoice_device", "mps"),
]
files = [{"path": f"{snapshot('mlx-community--' + name)}/config.json"}
         for name in ("Qwen3.5-4B-4bit", "parakeet-tdt-0.6b-v3")]
files += [{"path": f"{snapshot(repo)}/{name}", "profiles": [id]}
          for id, repo, *_ in cases for name in ("config.json", "audio_tokenizer/config.json")]
with TemporaryDirectory() as folder:
    root = Path(folder)
    manifest = root / "models.json"
    manifest.write_text(json.dumps(files))
    for id, repo, handler, model_arg, device_arg, device in cases:
        work = root / id
        work.mkdir()
        argv = arguments(id, root / "cache", manifest, work)
        option = lambda key: argv[argv.index(key) + 1]
        assert option("--tts") == handler, argv
        assert Path(option(model_arg)).resolve() == (root / "cache" / snapshot(repo)).resolve()
        assert option(device_arg) == device
        if handler == "omnivoice":
            # Without a clone prompt OmniVoice picks a new voice for every sentence.
            assert Path(option("--omnivoice_voice_clone_prompt")).name == "omnivoice.prompt", argv
            assert Path(option("--omnivoice_voice_clone_prompt")).is_file(), argv
        assert len(list(work.iterdir())) == 3, argv
        assert option("--llm_backend") == "mlx-lm"
        assert Path(option("--model_name")).name == "Qwen3.5-4B-4bit"
"#)
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/packaging/qwen/voice.py"))
        .output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn dual_entries_select_their_recognizer_and_share_tts_assets() {
    let output = Command::new("python3")
        .args(["-I", "-B", "-c"])
        .arg(r#"import json, os, runpy, sys, types
from pathlib import Path
from tempfile import TemporaryDirectory
sys.modules["ChatTTS"] = types.SimpleNamespace(Chat=type("Chat", (), {}))
arguments = runpy.run_path(sys.argv[1])["arguments"]
snapshot = "hub/models--{}/snapshots/pinned".format
pairs = [("qwen3-tts:0.6b-customvoice", "qwen3-tts:0.6b-customvoice-whisper", "mlx-community--Qwen3-TTS-12Hz-0.6B-CustomVoice-6bit"),
         ("qwen3-tts:1.7b-customvoice", "qwen3-tts:1.7b-customvoice-whisper", "mlx-community--Qwen3-TTS-12Hz-1.7B-CustomVoice-6bit"),
         ("chattts:en", "chattts:whisper", "2Noise--ChatTTS")]
defaults = [p[0] for p in pairs]
optional = [p[1] for p in pairs]
files = [{"path": snapshot("mlx-community--Qwen3.5-4B-4bit") + "/config.json"},
         {"path": snapshot("mlx-community--parakeet-tdt-0.6b-v3") + "/config.json", "profiles": defaults}]
for default, whisper, repo in pairs:
    name = "/asset/gpt/config.json" if repo == "2Noise--ChatTTS" else "/config.json"
    files.append({"path": snapshot(repo) + name, "profiles": [default, whisper]})
files += [{"path": snapshot("mlx-community--whisper-large-v3-turbo") + "/" + name, "profiles": optional}
          for name in ["config.json", "weights.safetensors", "README.md"]]
files += [{"path": snapshot("openai--whisper-large-v3") + "/" + name, "profiles": optional}
          for name in ["preprocessor_config.json", "tokenizer.json", "README.md"]]
with TemporaryDirectory() as folder:
    root = Path(folder)
    manifest = root / "models.json"
    manifest.write_text(json.dumps(files))
    for default, whisper, repo in pairs:
        targets = []
        for model in [default, whisper]:
            work = root / model.replace(":", "-")
            work.mkdir()
            argv = arguments(model, root / "cache", manifest, work)
            assert argv[argv.index("--stt") + 1] == ("mlx-audio-whisper" if model == whisper else "parakeet-tdt"), argv
            assert (work / "parakeet-tdt-0.6b-v3").is_symlink() == (model == default)
            assert (work / "whisper").is_dir() == (model == whisper)
            if model == whisper:
                assert argv[argv.index("--language") + 1] == "auto", argv
                assert "Reply in the language of the user's latest message." in argv[-1], argv
                assert sorted(p.name for p in (work / "whisper").iterdir()) == ["config.json", "preprocessor_config.json", "tokenizer.json", "weights.safetensors"]
            if repo == "2Noise--ChatTTS":
                targets.append(os.readlink(work / "asset"))
            else:
                targets.append(os.readlink(argv[argv.index("--qwen3_tts_model_name") + 1]))
                assert argv[argv.index("--qwen3_tts_language") + 1] == ("auto" if model == whisper else "english"), argv
                assert argv[argv.index("--qwen3_tts_speaker") + 1] == ("Vivian" if model == whisper else "Aiden"), argv
        assert targets[0] == targets[1], targets
"#)
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/packaging/qwen/voice.py"))
        .output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
