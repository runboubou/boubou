#!/usr/bin/env python3
"""Build the npm packages from a clean commit: the `boubou` launcher, the Apple Silicon package and,
if given, the x86-64 Linux package."""

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "packaging/qwen"))
from build_runner import TARGETS, ship_app, sources_digest  # noqa: E402
# npm publish fails above about 400 MB; each package is a few MB.
MAX_PACKED = 32 * 1024 * 1024
# Each computer installs only its own binary. cli.cjs names the same packages.
PLATFORMS = {"darwin-arm64": "@boubou/darwin-arm64", "linux-x64": "@boubou/linux-x64"}
DESCRIPTIONS = {"darwin-arm64": "Apple Silicon Macs", "linux-x64": "x86-64 Linux"}
LICENSES = [
    (
        "LFM-Open-License.txt",
        "https://huggingface.co/LiquidAI/LFM3-Audio-2B-GGUF/resolve/02355c1a141e464a064f2ab2c276638dbcb7f38a/LICENSE",
        "4d28ca14dedc0b3d0fcc2b3339f0e79931faa33874f3d24f522183a8fc70068c",
    ),
    (
        "llama.cpp-MIT.txt",
        "https://raw.githubusercontent.com/ggml-org/llama.cpp/8642541b255bfb5dc39cd351a178be09013a55d6/LICENSE",
        "94f29bbed6a22c35b992c5c6ebf0e7c92f13b836b90f36f461c9cf2f0f1d010d",
    ),
]


def output(*argv):
    return subprocess.check_output(argv, cwd=ROOT, text=True).strip()


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()



def fetch_license(path, url, expected):
    subprocess.run(
        ["curl", "-fsSL", "--connect-timeout", "10", "--max-time", "40", "--output", str(path), url],
        check=True,
    )
    if sha256(path) != expected:
        raise SystemExit(f"Upstream license checksum mismatch: {path.name}")


def reachable_packages(target):
    """Cargo packages compiled into the binary for this target, other than Boubou itself."""
    metadata = json.loads(output("cargo", "metadata", "--locked", "--offline", "--format-version", "1",
                                 "--filter-platform", target))
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    seen, pending = set(), [metadata["resolve"]["root"]]
    while pending:
        node = pending.pop()
        if node not in seen:
            seen.add(node)
            pending.extend(dep["pkg"] for dep in nodes[node]["deps"])
    return [p for p in metadata["packages"] if p["id"] in seen and p["name"] != "boubou"]


def write_licenses(folder, packages):
    """Upstream and dependency license texts for one binary; returns BUILD.json's dependency list."""
    (folder / "licenses").mkdir()
    for filename, url, expected in LICENSES:
        fetch_license(folder / "licenses" / filename, url, expected)
    dependencies = []
    for package in packages:
        directory = Path(package["manifest_path"]).parent
        license_files = {
            p for pattern in ["LICENSE*", "COPYING*", "NOTICE*"] for p in directory.glob(pattern) if p.is_file()
        }
        if package.get("license_file") and (directory / package["license_file"]).is_file():
            license_files.add(directory / package["license_file"])
        target = folder / "licenses/dependencies" / f"{package['name']}-{package['version']}"
        target.mkdir(parents=True)
        for path in sorted(license_files):
            shutil.copy2(path, target / path.name)
        dependencies.append({
            "name": package["name"],
            "version": package["version"],
            "license": package.get("license"),
            "included_license_files": sorted(p.name for p in license_files),
        })
    sysroot = Path(output("rustc", "--print", "sysroot"))
    for file in ["COPYRIGHT.html", "COPYRIGHT-library.html"]:
        source_file = sysroot / "share/doc/rust" / file
        if source_file.is_file():
            shutil.copy2(source_file, folder / "licenses" / f"Rust-{file}")
    return sorted(dependencies, key=lambda p: (p["name"], p["version"]))


def checksums(folder):
    files = sorted(p for p in folder.rglob("*") if p.is_file())
    (folder / "SHA256SUMS").write_text(
        "".join(f"{sha256(p)}  {p.relative_to(folder).as_posix()}\n" for p in files)
    )


def pack(folder, temporary, destination):
    packed = json.loads(output("npm", "pack", str(folder), "--ignore-scripts", "--json",
                               "--pack-destination", str(temporary)))
    path = Path(temporary) / packed[0]["filename"]
    if path.stat().st_size > MAX_PACKED:
        raise SystemExit(
            f"{path.name} is {path.stat().st_size} bytes; the limit is {MAX_PACKED}. Download large files at run time."
        )
    archive = destination / path.name
    os.replace(path, archive)
    (destination / f"{archive.name}.sha256").write_text(f"{sha256(archive)}  {archive.name}\n")
    print(f"Development candidate: {archive}")
    print(f"SHA-256: {sha256(archive)}")


def platform_package(folder, key, manifest, executable, qwen, target, build):
    """One computer's npm package: its binary, the Qwen manifests and scripts beside it, and its licenses."""
    os_name, cpu = key.split("-")
    folder.mkdir()
    (folder / "package.json").write_text(json.dumps({
        "name": PLATFORMS[key],
        "version": manifest["version"],
        "description": f"The Boubou binary for {DESCRIPTIONS[key]}. Install `boubou`, which uses it.",
        "homepage": manifest["homepage"],
        "bugs": manifest["bugs"],
        "license": manifest["license"],
        "repository": manifest["repository"],
        "os": [os_name],
        "cpu": [cpu],
        "files": ["boubou", "qwen", "LICENSE", "licenses", "BUILD.json", "SHA256SUMS", "THIRD_PARTY_NOTICES.txt"],
    }, indent=2) + "\n")
    shutil.copy2(executable, folder / "boubou")
    (folder / "boubou").chmod(0o755)
    shutil.copy2(ROOT / "LICENSE", folder / "LICENSE")
    if qwen:
        (folder / "qwen").mkdir()
        for file in ["runtime.json", "models.json"]:
            shutil.copyfile(qwen / file, folder / "qwen" / file)
        ship_app(folder / "qwen")
    dependencies = write_licenses(folder, reachable_packages(target))
    if key == "darwin-arm64":
        # The packaged binary lists the profiles its Qwen manifests add.
        build = {**build, "profiles": output(str(folder / "boubou"), "list").splitlines()}
    (folder / "BUILD.json").write_text(json.dumps({
        **build,
        "target": target,
        "binary_sha256": sha256(executable),
        "license_sources": [{"name": n, "url": u, "sha256": s} for n, u, s in LICENSES],
        "dependencies": dependencies,
    }, indent=2) + "\n")
    (folder / "THIRD_PARTY_NOTICES.txt").write_text(
        "Third-party license texts are under licenses/. Dependency versions and declared licenses are in BUILD.json. Liquid model weights and the llama.cpp backend are downloaded and verified by the existing runtime on first use, not bundled in this archive. Public distribution review remains pending.\n"
    )
    checksums(folder)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--qwen", type=Path, metavar="DIR",
        help="include reviewed runtime.json and models.json from a Qwen candidate build",
    )
    parser.add_argument(
        "--linux", type=Path, metavar="DIR",
        help="include an x86-64 Linux release build (DIR/boubou) and its Qwen manifests (DIR/qwen)",
    )
    args = parser.parse_args()
    if sys.platform != "darwin" or platform.machine() != "arm64":
        raise SystemExit("Build this candidate on an Apple Silicon Mac.")
    if output("git", "status", "--porcelain"):
        raise SystemExit(
            "Commit the reviewed packaging candidate before building its archive."
        )
    subprocess.run(
        ["cargo", "build", "--release", "--locked", "--offline", "--target-dir", str(ROOT / "target")],
        cwd=ROOT,
        check=True,
    )
    binary = ROOT / "target/release/boubou"
    subprocess.run(["lipo", str(binary), "-verify_arch", "arm64"], check=True)
    subprocess.run(["codesign", "--verify", "--strict", str(binary)], check=True)
    version = output(str(binary), "--version").split()[1]
    source = output("git", "rev-parse", "HEAD")
    manifest = json.loads((ROOT / "packaging/package.json").read_text())
    if manifest["version"] != version:
        raise SystemExit("Cargo and npm package versions must match.")
    destination = ROOT / "target/dist" / source[:7]
    destination.mkdir(parents=True, exist_ok=True)
    if (destination / f"boubou-{version}.tgz").exists():
        raise SystemExit(f"Candidate already exists; preserve its receipt: {destination}")
    if args.qwen:
        runtime = json.loads((args.qwen / "runtime.json").read_text())
        # npm cannot publish the runner (about 650 MB); users download it on first pull.
        if not str(runtime.get("url") or "").startswith("https://"):
            raise SystemExit("Build the Qwen runner with --url; npm cannot carry it.")
        runner = args.qwen / "runner.tar.gz"
        if runner.stat().st_size != runtime["size"] or sha256(runner) != runtime["sha256"]:
            raise SystemExit("Qwen archive does not match its release manifest")
    if args.linux:
        with (args.linux / "boubou").open("rb") as executable:
            header = executable.read(20)
        # An ELF64 executable for x86-64 (e_machine 0x3E).
        if header[:5] != b"\x7fELF\x02" or header[18:20] != b"\x3e\x00":
            raise SystemExit("The Linux build must be an x86-64 executable.")
        runtime = json.loads((args.linux / "qwen/runtime.json").read_text())
        if not str(runtime.get("url") or "").startswith("https://"):
            raise SystemExit("The Linux Qwen runner must be published at an HTTPS URL.")
        # The Linux runner is rebuilt only when its sources change, so check it is current.
        if runtime.get("sources_sha256") != sources_digest(TARGETS["Linux", "x86_64"]):
            raise SystemExit(
                "The published Linux runner is older than its sources. Rebuild and publish it "
                "as packaging/qwen/README.md describes."
            )
    build = {
        "version": version,
        "channel": "development",
        "source_commit": source,
        "source_url": f"https://github.com/runboubou/boubou/commit/{source}",
        "public_distribution_review": "pending",
    }
    platforms = ["darwin-arm64"] + (["linux-x64"] if args.linux else [])
    with tempfile.TemporaryDirectory(prefix="boubou-package-", dir=destination) as temporary:
        temporary = Path(temporary)
        mac = temporary / "darwin-arm64"
        platform_package(mac, "darwin-arm64", manifest, binary, args.qwen, "aarch64-apple-darwin", {
            **build,
            "rustc": output("rustc", "--version"),
            "node": output("node", "--version"),
            "npm": output("npm", "--version"),
            "clang": output("xcrun", "clang", "--version").splitlines()[0],
            "sdk": output("xcrun", "--show-sdk-version"),
            "cargo_lock_sha256": sha256(ROOT / "Cargo.lock"),
            "signing": "development/ad-hoc; no Developer ID",
            "notarization": "NOT RUN",
            "another_mac_acceptance": "NOT RUN",
        })
        if args.linux:
            platform_package(temporary / "linux-x64", "linux-x64", manifest, args.linux / "boubou",
                             args.linux / "qwen", "x86_64-unknown-linux-gnu",
                             {**build, "cargo_lock_sha256": sha256(ROOT / "Cargo.lock")})
        # The launcher package. npm installs only the platform package whose os and cpu match.
        launcher = temporary / "boubou"
        launcher.mkdir()
        for file in ["cli.cjs", "README.md", "LICENSE"]:
            shutil.copy2(ROOT / ("packaging" if file == "cli.cjs" else "") / file, launcher / file)
        manifest["optionalDependencies"] = {PLATFORMS[p]: version for p in platforms}
        (launcher / "package.json").write_text(json.dumps(manifest, indent=2) + "\n")
        # Launchers before the split accept an update only when its binary paths are executable,
        # then run the update's cli.cjs. These stubs keep that path working.
        for stub, launch in [("boubou", "cli.cjs"), ("linux-x64/boubou", "../cli.cjs")]:
            (launcher / stub).parent.mkdir(exist_ok=True)
            (launcher / stub).write_text(f'#!/bin/sh\nexec node "$(dirname "$0")/{launch}" "$@"\n')
            (launcher / stub).chmod(0o755)
        for folder in [temporary / p for p in platforms] + [launcher]:
            pack(folder, temporary, destination)


if __name__ == "__main__":
    main()
