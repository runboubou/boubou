#!/usr/bin/env python3
"""Build the single Liquid 2B macOS development archive from a clean commit."""
import gzip
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def output(*argv):
    return subprocess.check_output(argv, cwd=ROOT, text=True).strip()


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main():
    if sys.platform != "darwin" or platform.machine() != "arm64":
        raise SystemExit("Build this candidate on an Apple Silicon Mac.")
    if output("git", "status", "--porcelain"):
        raise SystemExit("Commit the reviewed packaging candidate before building its archive.")
    subprocess.run(["cargo", "build", "--release", "--locked", "--offline", "--target-dir", str(ROOT / "target")], cwd=ROOT, check=True)
    binary = ROOT / "target/release/boubou"
    subprocess.run(["lipo", "-verify_arch", "arm64", str(binary)], check=True)
    subprocess.run(["codesign", "--verify", "--strict", str(binary)], check=True)
    version = output(str(binary), "--version").split()[1]
    source = output("git", "rev-parse", "HEAD")
    epoch = int(output("git", "show", "-s", "--format=%ct", "HEAD"))
    name = f"boubou-{version}-dev-{source[:7]}-macos-arm64"
    destination = ROOT / "target/dist"
    destination.mkdir(parents=True, exist_ok=True)
    archive = destination / f"{name}.tar.gz"
    if archive.exists():
        raise SystemExit(f"Candidate already exists; preserve its receipt: {archive}")
    metadata = json.loads(output("cargo", "metadata", "--locked", "--offline", "--format-version", "1", "--filter-platform", "aarch64-apple-darwin"))
    reachable = set()
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    pending = [metadata["resolve"]["root"]]
    while pending:
        node = pending.pop()
        if node not in reachable:
            reachable.add(node)
            pending.extend(dep["pkg"] for dep in nodes[node]["deps"])
    with tempfile.TemporaryDirectory(prefix="boubou-package-", dir=destination) as temporary:
        folder = Path(temporary) / name
        folder.mkdir()
        shutil.copy2(binary, folder / "boubou")
        for file in ["install.sh", "uninstall.sh", "README.txt"]:
            shutil.copy2(ROOT / "packaging" / file, folder / file)
        shutil.copytree(ROOT / "packaging/licenses", folder / "licenses")
        dependencies = []
        for package in metadata["packages"]:
            if package["id"] not in reachable or package["name"] == "boubou":
                continue
            directory = Path(package["manifest_path"]).parent
            license_files = {p for pattern in ["LICENSE*", "COPYING*", "NOTICE*"] for p in directory.glob(pattern) if p.is_file()}
            if package.get("license_file"):
                path = directory / package["license_file"]
                if path.is_file():
                    license_files.add(path)
            target = folder / "licenses/dependencies" / f"{package['name']}-{package['version']}"
            target.mkdir(parents=True)
            for path in sorted(license_files):
                shutil.copy2(path, target / path.name)
            dependencies.append({"name": package["name"], "version": package["version"], "license": package.get("license"), "included_license_files": sorted(p.name for p in license_files)})
        sysroot = Path(output("rustc", "--print", "sysroot"))
        for file in ["COPYRIGHT.html", "COPYRIGHT-library.html"]:
            source_file = sysroot / "share/doc/rust" / file
            if source_file.is_file():
                shutil.copy2(source_file, folder / "licenses" / f"Rust-{file}")
        build = {"version": version, "channel": "development", "profile": "liquid", "source_commit": source, "source_url": f"https://github.com/entropyai-labs/boubou/commit/{source}", "target": "aarch64-apple-darwin", "rustc": output("rustc", "--version"), "clang": output("xcrun", "clang", "--version").splitlines()[0], "sdk": output("xcrun", "--show-sdk-version"), "cargo_lock_sha256": sha256(ROOT / "Cargo.lock"), "binary_sha256": sha256(binary), "signing": "development/ad-hoc; no Developer ID", "notarization": "NOT RUN", "another_mac_acceptance": "NOT RUN", "public_distribution_review": "pending", "dependencies": sorted(dependencies, key=lambda p: (p["name"], p["version"]))}
        (folder / "BUILD.json").write_text(json.dumps(build, indent=2) + "\n")
        (folder / "THIRD_PARTY_NOTICES.txt").write_text("Third-party license texts are under licenses/. Dependency versions and declared licenses are in BUILD.json. Liquid model weights and the llama.cpp backend are downloaded and verified by the existing runtime on first use, not bundled in this archive. Public distribution review remains pending.\n")
        files = sorted(p for p in folder.rglob("*") if p.is_file())
        (folder / "SHA256SUMS").write_text("".join(f"{sha256(p)}  {p.relative_to(folder).as_posix()}\n" for p in files))
        temporary_archive = Path(temporary) / archive.name
        with temporary_archive.open("wb") as raw:
            with gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=epoch) as compressed:
                with tarfile.open(fileobj=compressed, mode="w") as tar:
                    for path in [folder, *sorted(folder.rglob("*"))]:
                        entry = tar.gettarinfo(str(path), arcname=path.relative_to(folder.parent).as_posix())
                        entry.uid = entry.gid = 0
                        entry.uname = entry.gname = ""
                        entry.mtime = epoch
                        entry.mode = 0o755 if path.is_dir() or path.name in ["boubou", "install.sh", "uninstall.sh"] else 0o644
                        if path.is_file():
                            with path.open("rb") as stream:
                                tar.addfile(entry, stream)
                        else:
                            tar.addfile(entry)
        os.replace(temporary_archive, archive)
    (destination / f"{archive.name}.sha256").write_text(f"{sha256(archive)}  {archive.name}\n")
    print(f"Development candidate: {archive}")
    print(f"SHA-256: {sha256(archive)}")


if __name__ == "__main__":
    main()
