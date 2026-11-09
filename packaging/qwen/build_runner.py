"""Build a relocatable, hash-locked Qwen runner for this computer: Apple Silicon or x86-64 Linux with CUDA."""

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

HERE = Path(__file__).resolve().parent
RELEASE = "https://github.com/astral-sh/python-build-standalone/releases/download/20261026/"
TARGETS = {
    ("Darwin", "arm64"): {
        "python": RELEASE + "cpython-3.11.18%2B20261026-aarch64-apple-darwin-install_only_stripped.tar.gz",
        "python_sha256": "f3e1cd83a7f1d70e58b0d69102c18bd6867d2f973068dc5cbfd5822e445c66df",
        "lock": "requirements-macos.lock",
        "entries": ["voice.py"],
        "files": ["runtimes.json"],
        "imports": "kittenml.kittentts_legacy.onnx_model, mlx.core, omnivoice, pocket_tts, sounddevice, speech_to_speech.cli",
        "device": "mlx.core.default_device()",
        "host": {"min_macos": "15.0"},
    },
    ("Linux", "x86_64"): {
        "python": RELEASE + "cpython-3.11.18%2B20261026-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz",
        "python_sha256": "17c4978a36c496dc53c08899ef8dad1ad2734f6325f08d6033a1e66419e0af26",
        "lock": "requirements-linux.lock",
        "entries": ["voice.py", "cuda.py"],
        "files": [],
        # PortAudio is not installed with sounddevice here; Boubou owns audio and never imports it.
        "imports": "kittenml.kittentts_legacy.onnx_model, kokoro, nano_parakeet, omnivoice, pocket_tts, speech_to_speech.cli, torch",
        # Upstream's lighter CUDA setup serves the language model with llama.cpp, which uses PyTorch's CUDA libraries.
        "llama": ("https://github.com/ggml-org/llama.cpp/releases/download/b11960/llama-b11960-bin-ubuntu-cuda-13.4-x64.tar.gz",
                  "0cf758b28483593a5a15e0d2c9b686c40840d1f8be9b5bb2417f04a053113907"),
        "device": "'CUDA', torch.version.cuda",
        # CUDA 13 needs driver 580.65.06 or later. Ubuntu 24.04's glibc 2.39 is the oldest tested.
        "host": {"min_nvidia_driver": "580.65.06", "min_glibc": "2.39"},
    },
}


def digest(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def download(url, sha256, path):
    with urlopen(url, timeout=120) as response, path.open("wb") as f:
        shutil.copyfileobj(response, f)
    if digest(path) != sha256:
        raise ValueError(f"Checksum mismatch: {url}")


def sources_digest(target):
    """Hash what goes into this host's runner, so a published runner older than its sources is caught."""
    names = [*target["entries"], *target["files"], target["lock"], "bundle_data.py",
             *sorted(path.relative_to(HERE).as_posix() for path in (HERE / "voices").rglob("*") if path.is_file())]
    pins = [target["python"], target["python_sha256"], *target.get("llama", ())]
    content = hashlib.sha256("\n".join(pins).encode())
    for name in names:
        content.update(f"\n{name}\n".encode())
        content.update((HERE / name).read_bytes())
    return content.hexdigest()


def tree_digest(root):
    """Hash one sorted line per entry, as Boubou does to verify the installed runner without its archive."""
    lines = []
    for folder, directories, files in os.walk(root):
        for name in directories + files:
            path = Path(folder, name)
            relative = path.relative_to(root).as_posix()
            if "\t" in relative or "\n" in relative:
                raise ValueError(f"Unsupported file name: {relative!r}")
            if path.is_symlink():
                lines.append(f"{relative}\tlink\t{os.readlink(path)}\n")
            elif path.is_dir():
                lines.append(f"{relative}\tdir\n")
            else:
                kind = "exec" if path.stat().st_mode & 0o111 else "file"
                lines.append(f"{relative}\t{kind}\t{digest(path)}\n")
    return hashlib.sha256("".join(sorted(lines)).encode()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--url", help="immutable public HTTPS URL for a thin package; otherwise bundle the archive")
    args = parser.parse_args()
    target = TARGETS.get((platform.system(), platform.machine()))
    if target is None:
        parser.error("build on an Apple Silicon Mac or x86-64 Linux")
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
        download(target["python"], target["python_sha256"], cpython)
        with tarfile.open(cpython) as tar:
            tar.extractall(stage, filter="data")
        root = stage / "python"
        python = root / "bin/python3"
        # ChatTTS, EnCodec and SoX publish only pure-Python sdists. The verified interpreter's setuptools builds them.
        subprocess.run([python, "-I", "-B", "-m", "pip", "install", "--isolated", "--only-binary=:all:",
                        "--no-binary=chattts,encodec,sox", "--no-build-isolation",
                        "--require-hashes", "--no-deps", "--no-compile", "-r", HERE / target["lock"]], check=True)
        subprocess.run([python, "-I", "-B", "-m", "pip", "check"], check=True)
        for name in [*target["entries"], *target["files"], target["lock"]]:
            shutil.copy2(HERE / name, root)
        shutil.copytree(HERE / "voices", root / "voices")
        if "llama" in target:
            llama = stage / "llama.tar.gz"
            download(*target["llama"], llama)
            with tarfile.open(llama) as tar:
                tar.extractall(root, filter="data")
        provenance = install(root)
        provenance.update(python_url=target["python"], python_sha256=target["python_sha256"],
                          wheel_lock_sha256=digest(root / target["lock"]))
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
        check = f"""import runpy, sys, tempfile, {target["imports"]}
from pathlib import Path
from misaki import en, espeak
from omnivoice import VoiceClonePrompt
from speech_to_speech.LLM.chat import Chat, make_user_message
runner = runpy.run_path(sys.argv[1])
for entry in sys.argv[2:]:
    runpy.run_path(entry)
assert VoiceClonePrompt.load(str(runner["VOICES"] / "omnivoice.prompt")).ref_text
assert len((runner["VOICES"] / "chattts.txt").read_text().strip()) == 859
runner["preserve_qwen_user_turns"]()
chat = Chat(8)
for text in ("Tell me a long story.", "Stop. What is the capital of France?"):
    chat.add_item(make_user_message(text))
assert [message["role"] for message in chat.to_transformers_chat()] == ["user", "user"]
assert len(chat.buffer) == 2
with tempfile.TemporaryDirectory() as work:
    runner["link_espeak_data"](Path(work))
    assert en.G2P(fallback=espeak.EspeakFallback(british=False))("Boubou")[0]
print({target["device"]})
"""
        subprocess.run([moved / "bin/python3", "-I", "-B", "-c", check, *(moved / name for name in target["entries"])],
                       cwd=stage, check=True)
        bundle = stage / "runner.tar.gz"
        with tarfile.open(bundle, "w:gz", compresslevel=3) as tar:
            for path in sorted(moved.rglob("*")):
                if path.is_symlink() and (os.readlink(path).startswith("/") or not path.resolve(strict=True).is_relative_to(moved)):
                    raise ValueError(f"Non-relocatable link: {path.relative_to(moved)}")
                # The Rust verifier supports ordinary files and relative symlinks.
                tar.inodes.clear()
                tar.add(path, arcname=path.relative_to(moved), recursive=False)
        # Boubou checks the installed tree against this digest on every start, instead of re-reading the archive.
        result = {"url": args.url, "size": bundle.stat().st_size, "sha256": digest(bundle),
                  "tree_sha256": tree_digest(moved), "sources_sha256": sources_digest(target), **target["host"]}
        bundle.rename(archive)
        (output / "runtime.json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"Built {archive} ({result['size']} bytes, sha256 {result['sha256']})")


if __name__ == "__main__":
    main()
