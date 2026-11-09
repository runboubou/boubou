#!/usr/bin/env python3
"""Build the native macOS npm package from a clean commit."""

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
# npm publish fails above about 400 MB; the package itself is a few MB.
MAX_PACKED = 32 * 1024 * 1024
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--qwen", type=Path, metavar="DIR",
        help="include reviewed runtime.json and models.json from a Qwen candidate build",
    )
    args = parser.parse_args()
    if sys.platform != "darwin" or platform.machine() != "arm64":
        raise SystemExit("Build this candidate on an Apple Silicon Mac.")
    if output("git", "status", "--porcelain"):
        raise SystemExit(
            "Commit the reviewed packaging candidate before building its archive."
        )
    subprocess.run(
        [
            "cargo",
            "build",
            "--release",
            "--locked",
            "--offline",
            "--target-dir",
            str(ROOT / "target"),
        ],
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
    name = f"boubou-{version}"
    destination = ROOT / "target/dist" / source[:7]
    destination.mkdir(parents=True, exist_ok=True)
    archive = destination / f"{name}.tgz"
    if archive.exists():
        raise SystemExit(f"Candidate already exists; preserve its receipt: {archive}")
    metadata = json.loads(
        output(
            "cargo",
            "metadata",
            "--locked",
            "--offline",
            "--format-version",
            "1",
            "--filter-platform",
            "aarch64-apple-darwin",
        )
    )
    reachable = set()
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    pending = [metadata["resolve"]["root"]]
    while pending:
        node = pending.pop()
        if node not in reachable:
            reachable.add(node)
            pending.extend(dep["pkg"] for dep in nodes[node]["deps"])
    with tempfile.TemporaryDirectory(
        prefix="boubou-package-", dir=destination
    ) as temporary:
        folder = Path(temporary) / name
        folder.mkdir()
        shutil.copy2(binary, folder / "boubou")
        for file in ["cli.cjs", "package.json"]:
            shutil.copy2(ROOT / "packaging" / file, folder / file)
        if args.qwen:
            (folder / "qwen").mkdir()
            for file in ["runtime.json", "models.json"]:
                shutil.copyfile(args.qwen / file, folder / "qwen" / file)
            runtime = json.loads((folder / "qwen/runtime.json").read_text())
            # npm cannot publish the runner (about 650 MB); users download it on first pull.
            if not str(runtime.get("url") or "").startswith("https://"):
                raise SystemExit("Build the Qwen runner with --url; npm cannot carry it.")
            runner = args.qwen / "runner.tar.gz"
            if runner.stat().st_size != runtime["size"] or sha256(runner) != runtime["sha256"]:
                raise SystemExit("Qwen archive does not match its release manifest")
        shutil.copy2(ROOT / "README.md", folder / "README.md")
        shutil.copy2(ROOT / "LICENSE", folder / "LICENSE")
        (folder / "licenses").mkdir()
        for filename, url, expected in LICENSES:
            path = folder / "licenses" / filename
            subprocess.run(
                [
                    "curl",
                    "-fsSL",
                    "--connect-timeout",
                    "10",
                    "--max-time",
                    "40",
                    "--output",
                    str(path),
                    url,
                ],
                check=True,
            )
            if sha256(path) != expected:
                raise SystemExit(f"Upstream license checksum mismatch: {filename}")
        dependencies = []
        for package in metadata["packages"]:
            if package["id"] not in reachable or package["name"] == "boubou":
                continue
            directory = Path(package["manifest_path"]).parent
            license_files = {
                p
                for pattern in ["LICENSE*", "COPYING*", "NOTICE*"]
                for p in directory.glob(pattern)
                if p.is_file()
            }
            if package.get("license_file"):
                path = directory / package["license_file"]
                if path.is_file():
                    license_files.add(path)
            target = (
                folder
                / "licenses/dependencies"
                / f"{package['name']}-{package['version']}"
            )
            target.mkdir(parents=True)
            for path in sorted(license_files):
                shutil.copy2(path, target / path.name)
            dependencies.append(
                {
                    "name": package["name"],
                    "version": package["version"],
                    "license": package.get("license"),
                    "included_license_files": sorted(p.name for p in license_files),
                }
            )
        sysroot = Path(output("rustc", "--print", "sysroot"))
        for file in ["COPYRIGHT.html", "COPYRIGHT-library.html"]:
            source_file = sysroot / "share/doc/rust" / file
            if source_file.is_file():
                shutil.copy2(source_file, folder / "licenses" / f"Rust-{file}")
        build = {
            "version": version,
            "channel": "development",
            "profiles": output(str(folder / "boubou"), "list").splitlines(),
            "source_commit": source,
            "source_url": f"https://github.com/runboubou/boubou/commit/{source}",
            "target": "aarch64-apple-darwin",
            "rustc": output("rustc", "--version"),
            "node": output("node", "--version"),
            "npm": output("npm", "--version"),
            "clang": output("xcrun", "clang", "--version").splitlines()[0],
            "sdk": output("xcrun", "--show-sdk-version"),
            "cargo_lock_sha256": sha256(ROOT / "Cargo.lock"),
            "binary_sha256": sha256(binary),
            "signing": "development/ad-hoc; no Developer ID",
            "notarization": "NOT RUN",
            "another_mac_acceptance": "NOT RUN",
            "public_distribution_review": "pending",
            "license_sources": [
                {"name": filename, "url": url, "sha256": checksum}
                for filename, url, checksum in LICENSES
            ],
            "dependencies": sorted(
                dependencies, key=lambda p: (p["name"], p["version"])
            ),
        }
        (folder / "BUILD.json").write_text(json.dumps(build, indent=2) + "\n")
        (folder / "THIRD_PARTY_NOTICES.txt").write_text(
            "Third-party license texts are under licenses/. Dependency versions and declared licenses are in BUILD.json. Liquid model weights and the llama.cpp backend are downloaded and verified by the existing runtime on first use, not bundled in this archive. Public distribution review remains pending.\n"
        )
        files = sorted(p for p in folder.rglob("*") if p.is_file())
        (folder / "SHA256SUMS").write_text(
            "".join(f"{sha256(p)}  {p.relative_to(folder).as_posix()}\n" for p in files)
        )
        packed = json.loads(
            output(
                "npm",
                "pack",
                str(folder),
                "--ignore-scripts",
                "--json",
                "--pack-destination",
                str(Path(temporary)),
            )
        )
        size = (Path(temporary) / packed[0]["filename"]).stat().st_size
        if size > MAX_PACKED:
            raise SystemExit(
                f"npm tarball is {size} bytes; the limit is {MAX_PACKED}. Download large files at run time."
            )
        os.replace(Path(temporary) / packed[0]["filename"], archive)
    (destination / f"{archive.name}.sha256").write_text(
        f"{sha256(archive)}  {archive.name}\n"
    )
    print(f"Development candidate: {archive}")
    print(f"SHA-256: {sha256(archive)}")


if __name__ == "__main__":
    main()
