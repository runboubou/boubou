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

struct Prompt {
    child: Child,
    master: fs::File,
    _slave: fs::File,
    output: String,
    original: libc::termios,
}

impl Prompt {
    fn new(package: &Package, no_color: bool, args: &[&str]) -> Self {
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
        let mut command = package.command();
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
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY.into(), 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
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
        let deadline = Instant::now() + Duration::from_secs(5);
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
                "Missing {choices:?}: {}",
                self.output
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn send(&mut self, text: &[u8]) {
        self.output.clear();
        self.master.write_all(text).unwrap();
    }

    fn finish(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            let mut buffer = [0; 8192];
            let _ = self.master.read(&mut buffer);
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "Prompt did not exit");
            thread::sleep(Duration::from_millis(10));
        };
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
fn guided_start_navigates_recommendations_and_restores_the_terminal() {
    let package = Package::new();
    let mut prompt = Prompt::new(&package, false, &[]);
    prompt.expect(&["Recommend a model for my Mac"]);
    assert!(prompt.output.contains("Use a model I already chose"));
    assert!(prompt.output.contains("\x1b[38;"), "No color emphasis");
    prompt.send(b"\r");
    prompt.expect(&[
        "Recommended for your Mac",
        "No suitable model found",
        "Device check unavailable",
    ]);
    prompt.send(b"\x1b");
    prompt.expect(&["How would you like to start?"]);
    prompt.send(b"\x03");
    assert_eq!(prompt.finish().code(), Some(130));
    assert!(!package.0.join("menu-cache").exists());
}

#[test]
fn guided_start_accepts_a_pasted_offline_command_after_an_error() {
    let package = Package::new();
    let mut prompt = Prompt::new(&package, true, &[]);
    prompt.expect(&["Recommend a model for my Mac"]);
    assert!(!prompt.output.contains("\x1b[38;"), "NO_COLOR was ignored");
    prompt.send(b"\x1b[B\r");
    prompt.expect(&["Paste a model name"]);
    prompt.send(b"unknown\r");
    prompt.expect(&["Model not supported"]);
    assert!(!package.0.join("menu-cache").exists());
    prompt.send(b"\x15\x1b[200~npx --yes --cache \"$HOME/a cache\" boubou@beta run LFM3-Audio-2B --offline\x1b[201~\r");
    prompt.expect(&["offline mode is enabled"]);
    assert_eq!(prompt.finish().code(), Some(1));
    assert!(!package.0.join("menu-cache/native").exists());
    assert!(!package.0.join("menu-cache/models").exists());
}

#[test]
fn guided_start_restores_the_terminal_when_the_launcher_is_terminated() {
    let package = Package::new();
    let mut prompt = Prompt::new(&package, false, &[]);
    prompt.expect(&["Recommend a model for my Mac"]);
    assert_eq!(
        unsafe { libc::kill(prompt.child.id() as i32, libc::SIGTERM) },
        0
    );
    assert_eq!(prompt.finish().signal(), Some(15));
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
fn packaged_qwen_recovers_partial_assets_and_launches_offline() {
    use flate2::{Compression, write::GzEncoder};
    use sha2::{Digest, Sha256};

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
    let digest = format!("{:x}", Sha256::digest(&bytes));
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
    let files = serde_json::json!([{
        "profile": id, "path": "models/tts.bin", "size": asset.len(),
        "sha256": format!("{:x}", Sha256::digest(asset)), "url": "https://example.invalid/tts.bin"
    }]);
    fs::write(release.join("models.json"), files.to_string()).unwrap();
    fs::create_dir(cache.join("models")).unwrap();
    fs::write(cache.join("models/tts.bin.part"), asset).unwrap();
    let mut prompt = Prompt::new(&package, true, &["run", id, "--offline"]);
    prompt.expect(&["selected: qwen3-tts:1.7b-customvoice"]);
    assert!(prompt.finish().success());
    assert_eq!(fs::read(cache.join("models/tts.bin")).unwrap(), asset);
    assert!(!cache.join("models/tts.bin.part").exists());
}

#[test]
fn qwen_package_lists_both_models_and_doctor_is_read_only() {
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
    let ids = ["qwen3-tts:1.7b-customvoice", "qwen3-tts:0.6b-customvoice"];
    let files: Vec<_> = ids
        .iter()
        .map(|id| {
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
    let cache = package.0.join("unused-cache");
    for id in ids {
        assert!(model_list.lines().any(|line| line == id));
        let doctor = package.run(&["--cache-dir", cache.to_str().unwrap(), "doctor", id]);
        assert_eq!(doctor.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&doctor.stdout).contains(id));
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
    for id in ["qwen3-tts:1.7b-customvoice", "qwen3-tts:0.6b-customvoice"] {
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
        b"LFM3-Audio-2B\nLFM3-Omni-2B\nLFM2.5-Audio-1.5B\nLFM2.5-Audio-1.5B-JP\n"
    );
    let cache = package.0.join("unused-cache");
    let cache_arg = cache.to_str().unwrap();
    for command in ["run", "pull", "doctor"] {
        assert_eq!(
            package
                .run(&["--cache-dir", cache_arg, command, "unknown"])
                .status
                .code(),
            Some(2)
        );
        assert!(!cache.exists(), "Unknown model created a cache");
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
    for model in ["LFM2.5-Audio-1.5B", "LFM2.5-Audio-1.5B-JP"] {
        let doctor = package.run(&["--cache-dir", cache_arg, "doctor", model]);
        assert_eq!(doctor.status.code(), Some(1));
        let text = String::from_utf8_lossy(&doctor.stdout);
        assert!(text.contains(&format!("Profile: Liquid AI {model}")));
        assert!(text.contains(&format!("tokenizer-{model}-Q8_0.gguf")));
        assert!(text.contains(&format!("vocoder-{model}-Q8_0.gguf")));
    }
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
