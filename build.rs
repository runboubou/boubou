use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=native/audio.m");
    println!("cargo:rerun-if-changed=native/Info.plist");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    let output =
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR")).join("boubou-audio");
    let plist = PathBuf::from("native/Info.plist")
        .canonicalize()
        .expect("native/Info.plist exists");
    let architecture = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86_64",
        _ => panic!("The native audio helper supports macOS arm64 and x86_64"),
    };
    let status = Command::new("clang")
        .args([
            "-O2",
            "-arch",
            architecture,
            "-mmacosx-version-min=13.0",
            "-fobjc-arc",
            "-framework",
            "AVFoundation",
            "-framework",
            "Foundation",
            "-framework",
            "CoreAudio",
            "native/audio.m",
            "-o",
        ])
        .arg(&output)
        .args([
            "-Xlinker",
            "-sectcreate",
            "-Xlinker",
            "__TEXT",
            "-Xlinker",
            "__info_plist",
            "-Xlinker",
        ])
        .arg(&plist)
        .status()
        .expect("Building Boubou on macOS requires the Xcode Command Line Tools (clang)");
    assert!(
        status.success(),
        "Failed to compile the native audio helper"
    );
    let status = Command::new("codesign")
        .args([
            "--force",
            "--sign",
            "-",
            "--identifier",
            "ai.entropy.boubou.audio",
        ])
        .arg(&output)
        .status()
        .expect("codesign is part of macOS");
    assert!(status.success(), "Failed to sign the native audio helper");
}
