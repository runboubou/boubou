#![cfg(target_os = "macos")]

use std::{
    fs,
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Package(PathBuf);

impl Package {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "bobo package {} {} {}",
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

#[test]
fn npm_launcher_runs_the_native_cli_and_rejects_unknown_models() {
    let package = Package::new();
    let version = package.run(&["--version"]);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("bobo "));
    let help = package.run(&["run", "--help"]);
    assert!(help.status.success(), "{help:?}");
    assert!(String::from_utf8_lossy(&help.stdout).contains("LFM3-Audio-2B"));
    assert_eq!(package.run(&["run", "liquid"]).status.code(), Some(2));
    assert_eq!(package.run(&["run", "other"]).status.code(), Some(2));
    assert_eq!(package.run(&["run"]).status.code(), Some(2));
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
    assert!(String::from_utf8_lossy(&result.stderr).contains("Could not start bobo"));
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
