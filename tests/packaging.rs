#![cfg(target_os = "macos")]

use sha2::{Digest, Sha256};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

struct Package {
    root: PathBuf,
    source: PathBuf,
    bin: PathBuf,
}

impl Package {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "boubou-package-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = root.join("package with spaces");
        let bin = root.join("bin with spaces");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("install.sh"),
            include_str!("../packaging/install.sh"),
        )
        .unwrap();
        fs::write(
            source.join("uninstall.sh"),
            include_str!("../packaging/uninstall.sh"),
        )
        .unwrap();
        fs::copy(env!("CARGO_BIN_EXE_boubou"), source.join("boubou")).unwrap();
        let checksum = format!(
            "{:x}  boubou\n",
            Sha256::digest(fs::read(source.join("boubou")).unwrap())
        );
        fs::write(source.join("SHA256SUMS"), checksum).unwrap();
        Self { root, source, bin }
    }

    fn command(&self, script: &str, arguments: &[&str]) -> Output {
        Command::new("/bin/sh")
            .arg(self.source.join(script))
            .args(arguments)
            .env("BOUBOU_INSTALL_DIR", &self.bin)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .output()
            .unwrap()
    }
}

impl Drop for Package {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn install_upgrade_and_remove_keep_model_data_without_build_tools() {
    let package = Package::new();
    let models = package.root.join("models");
    fs::create_dir_all(&models).unwrap();
    fs::write(models.join("keep"), b"cached model").unwrap();
    let installed = package.command("install.sh", &[]);
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let binary = package.bin.join("boubou");
    let version = Command::new(&binary)
        .arg("--version")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("boubou "));
    fs::write(&binary, b"previous candidate").unwrap();
    assert!(package.command("install.sh", &[]).status.success());
    assert!(
        Command::new(&binary)
            .arg("--version")
            .status()
            .unwrap()
            .success()
    );
    assert!(package.command("uninstall.sh", &[]).status.success());
    assert!(!binary.exists());
    assert_eq!(fs::read(models.join("keep")).unwrap(), b"cached model");
}

#[test]
fn failed_integrity_or_unsafe_destination_does_not_replace_files() {
    use std::os::unix::fs::symlink;
    let package = Package::new();
    fs::create_dir_all(&package.bin).unwrap();
    let installed = package.bin.join("boubou");
    fs::write(&installed, b"existing candidate").unwrap();
    fs::write(package.source.join("boubou"), b"damaged archive").unwrap();
    assert!(!package.command("install.sh", &[]).status.success());
    assert_eq!(fs::read(&installed).unwrap(), b"existing candidate");
    fs::remove_file(&installed).unwrap();
    let unrelated = package.root.join("unrelated");
    fs::write(&unrelated, b"keep this file").unwrap();
    symlink(&unrelated, &installed).unwrap();
    let rejected = package.command("install.sh", &[]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("symlink"));
    assert!(!package.command("uninstall.sh", &[]).status.success());
    assert_eq!(fs::read(&unrelated).unwrap(), b"keep this file");
}

#[test]
fn invalid_installer_arguments_fail_before_installation() {
    let package = Package::new();
    assert_eq!(
        package.command("install.sh", &["--unknown"]).status.code(),
        Some(2)
    );
    assert_eq!(
        package
            .command("install.sh", &["--run", "extra"])
            .status
            .code(),
        Some(2)
    );
    assert!(!package.bin.exists());
}
