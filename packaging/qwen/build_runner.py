"""Build a relocatable, hash-locked Qwen runner on Apple Silicon."""

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import tarfile
import tempfile
from pathlib import Path
from urllib.request import urlopen

from bundle_data import install
from build_drama3 import install as install_drama3

HERE = Path(__file__).resolve().parent
PYTHON = "https://github.com/astral-sh/python-build-standalone/releases/download/20261026/cpython-3.11.18%2B20261026-aarch64-apple-darwin-install_only_stripped.tar.gz"
PYTHON_SHA = "f3e1cd83a7f1d70e58b0d69102c18bd6867d2f973068dc5cbfd5822e445c66df"


def digest(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--url", help="immutable public HTTPS URL for a thin npm package; otherwise bundle the archive")
    args = parser.parse_args()
    if (platform.system(), platform.machine()) != ("Darwin", "arm64"):
        parser.error("build on an Apple Silicon Mac")
    if args.url is not None and not args.url.startswith("https://"):
        parser.error("supply an HTTPS archive URL")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    # The metadata is written last and must describe a completed build.
    archive = output / "runner.tar.gz"
    if archive.exists() or (output / "runtime.json").exists():
        parser.error("use a fresh output directory")
    with tempfile.TemporaryDirectory(dir=output) as tmp:
        stage = Path(tmp)
        cpython = stage / "cpython.tar.gz"
        with urlopen(PYTHON, timeout=120) as response, cpython.open("wb") as f:
            shutil.copyfileobj(response, f)
        if digest(cpython) != PYTHON_SHA:
            raise ValueError("Portable Python checksum mismatch")
        with tarfile.open(cpython) as tar:
            tar.extractall(stage, filter="data")
        root = stage / "python"
        python = root / "bin/python3"
        # ChatTTS and EnCodec publish only pure-Python sdists. The verified interpreter's setuptools builds them.
        subprocess.run([python, "-I", "-B", "-m", "pip", "install", "--isolated", "--only-binary=:all:",
                        "--no-binary=chattts,encodec", "--no-build-isolation",
                        "--require-hashes", "--no-deps", "--no-compile", "-r", HERE / "requirements-macos.lock"], check=True)
        subprocess.run([python, "-I", "-B", "-m", "pip", "check"], check=True)
        shutil.copy2(HERE / "voice.py", root)
        shutil.copy2(HERE / "runtimes.json", root)
        shutil.copy2(HERE / "requirements-macos.lock", root)
        provenance = install(root)
        provenance.update(install_drama3(root))
        provenance.update(python_url=PYTHON, python_sha256=PYTHON_SHA,
                          wheel_lock_sha256=digest(root / "requirements-macos.lock"))
        (root / "BUILD.json").write_text(json.dumps(provenance, indent=2) + "\n")
        for path in root.rglob("__pycache__"):
            shutil.rmtree(path)
        # Only the interpreter is executed. Generated entry scripts have build-time shebangs.
        for path in (root / "bin").iterdir():
            if not (path.name == "python3" or path.name == "python3.11"):
                if path.is_file() or path.is_symlink():
                    path.unlink()
        moved = stage / "relocated runtime"
        root.rename(moved)
        # Phonemize an unlisted word as Kokoro does: spaCy's English model, then espeak-ng.
        # KittenTTS phonemizes with the same espeak-ng data through its ONNX runtime.
        check = """import runpy, sys, tempfile, kittenml.kittentts_legacy.onnx_model, mlx.core, omnivoice, pocket_tts, sounddevice, speech_to_speech.cli
from pathlib import Path
from misaki import en, espeak
from speech_to_speech.LLM.chat import Chat, make_user_message
runner = runpy.run_path(sys.argv[1])
runner["preserve_qwen_user_turns"]()
chat = Chat(8)
for text in ("Tell me a long story.", "Stop. What is the capital of France?"):
    chat.add_item(make_user_message(text))
assert [message["role"] for message in chat.to_transformers_chat()] == ["user", "user"]
assert len(chat.buffer) == 2
with tempfile.TemporaryDirectory() as work:
    runner["link_espeak_data"](Path(work))
    assert en.G2P(fallback=espeak.EspeakFallback(british=False))("Boubou")[0]
print(mlx.core.default_device())
"""
        subprocess.run([moved / "bin/python3", "-I", "-B", "-c", check, moved / "voice.py"], cwd=stage, check=True)
        bundle = stage / "runner.tar.gz"
        with tarfile.open(bundle, "w:gz", compresslevel=3) as tar:
            for path in sorted(moved.rglob("*")):
                if path.is_symlink() and (os.readlink(path).startswith("/") or not path.resolve(strict=True).is_relative_to(moved)):
                    raise ValueError(f"Non-relocatable link: {path.relative_to(moved)}")
                # The Rust verifier supports ordinary files and relative symlinks.
                tar.inodes.clear()
                tar.add(path, arcname=path.relative_to(moved), recursive=False)
        result = {"url": args.url, "size": bundle.stat().st_size, "sha256": digest(bundle), "min_macos": "15.0"}
        bundle.rename(archive)
        (output / "runtime.json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"Built {archive} ({result['size']} bytes, sha256 {result['sha256']})")


if __name__ == "__main__":
    main()
