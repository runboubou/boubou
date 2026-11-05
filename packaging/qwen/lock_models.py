"""Write the candidate's models.json using exact upstream Git objects."""

import argparse
import hashlib
import json
import re
import subprocess
import tempfile
from pathlib import Path
from urllib.parse import quote

# Model files are stored as normal Git blobs or Git LFS pointers on the Hub.
LFS = re.compile(
    rb"version https://git-lfs.github.com/spec/v1\noid sha256:([0-9a-f]{64})\nsize ([0-9]+)\n?"
)

# The MLX repositories contain one conversion each, including the nested TTS codec.
# Kokoro ships its card, weights and the selected voice, not samples or other voices.
MODELS = [
    ("parakeet-tdt-0.6b-v3", "ed2b7e8c15f9aaa0b5772e2efb986255eaef7e15", None, None),
    ("Qwen3.5-4B-4bit", "0e7ffd5c629ef7719d4cbc04069232580bfa9d9c", None, None),
    ("Qwen3-TTS-12Hz-0.6B-CustomVoice-6bit", "7dc92af14613355896fcab13b268c19ede233139", "qwen3-tts:0.6b-customvoice", None),
    ("Qwen3-TTS-12Hz-1.7B-CustomVoice-6bit", "1c6c0ff58c43afa8df571facde2efa077efd85e2", "qwen3-tts:1.7b-customvoice", None),
    ("Kokoro-82M-bf16", "a71e4d38b236d968966a2002c4c895dbd12b1c3c", "kokoro:82m",
     {"README.md", "config.json", "kokoro-v1_0.safetensors", "voices/af_heart.safetensors"}),
]
# Retain original attribution alongside the MLX model cards.
ORIGINALS = [
    ("nvidia/parakeet-tdt-0.6b-v3", "541d1f99c6b0c3cd0b11a95167540bb8edefd82b", {"README.md"}, None),
    ("Qwen/Qwen3.5-4B", "851bf6e806efd8d0a36b00ddf55e13ccb7b8cd0a", {"LICENSE", "README.md"}, None),
    ("Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice", "85e237c12c027371202489a0ec509ded67b5e4b5", {"README.md"}, "qwen3-tts:0.6b-customvoice"),
    ("Qwen/Qwen3-TTS-12Hz-1.7B-CustomVoice", "0c0e3051f131929182e2c023b9537f8b1c68adfe", {"README.md"}, "qwen3-tts:1.7b-customvoice"),
    ("hexgrad/Kokoro-82M", "f3ff3571791e39611d31c381e3a41a3af07b4987", {"README.md"}, "kokoro:82m"),
]


def snapshot(repo, revision, destination, profile, only=None):
    url = f"https://huggingface.co/{repo}"
    with tempfile.TemporaryDirectory() as folder:
        def git(*args):
            return subprocess.check_output(["git", *args], cwd=folder)

        git("init", "--bare", "--quiet")
        git("-c", "fetch.fsckObjects=true", "fetch", "--quiet", "--depth=1", url, revision)
        if git("rev-parse", "FETCH_HEAD").decode().strip() != revision:
            raise ValueError(f"Unexpected revision for {repo}")
        included = set()
        for entry in git("ls-tree", "-rz", revision).split(b"\0"):
            if not entry:
                continue
            header, name = entry.decode().split("\t", 1)
            mode, kind, oid = header.split()
            if name == ".gitattributes" or (only and name not in only):
                continue
            if mode != "100644" or kind != "blob":
                raise ValueError(f"Unexpected Git entry at {repo}/{name}")
            data = git("cat-file", "blob", oid)
            pointer = LFS.fullmatch(data)
            if pointer:
                checksum, size = pointer[1].decode(), int(pointer[2])
            elif data.startswith(b"version https://git-lfs.github.com/"):
                raise ValueError(f"Invalid LFS pointer at {repo}/{name}")
            else:
                checksum, size = hashlib.sha256(data).hexdigest(), len(data)
            file = {
                "path": f"{destination}/{name}",
                "url": f"{url}/resolve/{revision}/{quote(name)}",
                "size": size,
                "sha256": checksum,
            }
            if profile:
                file["profile"] = profile
            included.add(name)
            yield file
        required = only or {"config.json", "model.safetensors"}
        if profile and not only:
            required |= {"speech_tokenizer/config.json", "speech_tokenizer/model.safetensors"}
        if missing := required - included:
            raise ValueError(f"Missing files in {repo}: {sorted(missing)}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="output path for models.json")
    args = parser.parse_args()
    files = []
    for name, revision, profile, only in MODELS:
        directory = f"hub/models--mlx-community--{name}/snapshots/{revision}"
        files.extend(snapshot(f"mlx-community/{name}", revision, directory, profile, only))
    for repo, revision, only, profile in ORIGINALS:
        files.extend(snapshot(repo, revision, f"licenses/{repo}/{revision}", profile, only))
    files.extend(snapshot(
        "pipecat-ai/smart-turn-v3", "f766f81d3cfdf7737ac64aad813d91bbfd56bf93",
        "smart-turn", None, {"README.md", "smart-turn-v3.2-cpu.onnx"},
    ))
    files.append({
        "path": "licenses/parakeet/CC-BY-4.0.txt",
        "url": "https://raw.githubusercontent.com/creativecommons/cc-legal-tools-data/a0dac18d1a773b4cdb7d8dd5d9bf4efab7901030/docs/licenses/by/4.0/legalcode.txt",
        "size": 18657,
        "sha256": "9ba9550ad48438d0836ddab3da480b3b69ffa0aac7b7878b5a0039e7ab429411",
    })
    args.output.write_text(json.dumps(files, indent=2) + "\n")


if __name__ == "__main__":
    main()
