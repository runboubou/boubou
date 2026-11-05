"""Write the candidate's models.json using exact upstream Git objects."""

import argparse
import hashlib
import io
import json
import re
import subprocess
import tempfile
import zipfile
from pathlib import Path
from urllib.parse import quote
from urllib.request import urlopen

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
# ptts 0.4.0 defaults: its checkpoint, read locally (q8 weights, tokenizer, default voice), and its
# voice catalog, which the upstream handler lists at startup. The Hub client ptts uses ignores
# HF_HUB_OFFLINE, but returns a file cached at an exact commit without connecting.
PHONON = [
    ("gradium/phonon-31m-en", "8c64b55ec301728e2d1695bfea87a08a267d0c2d",
     {"README.md", "config.json", "default-voice.safetensors", "model.q8.gguf", "tokenizer.json"}, "phonon:31m-en"),
    ("gradium/phonon-voices", "da6c9ec0d47be8df5c87c730a3aa97187f3229c3", {"voices.jsonl"}, "phonon:31m-en"),
]
# Every Pocket TTS stock config except `english`, an alias of english_2026-10.
POCKET = "kyutai/pocket-tts-without-voice-cloning"
POCKET_CONFIGS = [
    "english_2026-10", "english_2026-09_2", "english_2026-09", "english_2026-09_24l",
    "english_drifting_26-09", "english_2026-04", "english_2026-04_24l", "english_2026-01",
    "english_18m_2026-10", "english_18m_refl_2026-10", "english_7m_2026-10",
    "english_7m_refl_2026-10", "english_2m_2026-10", "english_2m_refl_2026-10",
    "dutch", "dutch_24l", "french", "french_24l", "german", "german_24l",
    "italian", "italian_24l", "portuguese", "portuguese_24l", "spanish", "spanish_24l",
]
# The SDK's default preset voice for each language, from the revision the SDK reads them at.
POCKET_VOICES = {"english": "alba", "dutch": "daan", "french": "estelle", "german": "juergen",
                 "italian": "giovanni", "portuguese": "rafael", "spanish": "lola"}
POCKET_VOICE_REVISION = "a7056f505a4a8ca72d8b5fb1440071d6f1a35739"


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


def pocket_configs():
    """Read the stock configs from the hash-locked SDK wheel that the runner installs."""
    lock = (Path(__file__).resolve().parent / "requirements-macos.lock").read_text()
    version, digest = re.search(r"^pocket-tts==(\S+) --hash=sha256:([0-9a-f]{64})$", lock, re.M).groups()
    url = f"https://files.pythonhosted.org/packages/py3/p/pocket_tts/pocket_tts-{version}-py3-none-any.whl"
    with urlopen(url, timeout=90) as response:
        wheel = response.read()
    if hashlib.sha256(wheel).hexdigest() != digest:
        raise ValueError("The Pocket TTS wheel does not match the runner lock")
    with zipfile.ZipFile(io.BytesIO(wheel)) as z:
        return {config: z.read(f"pocket_tts/config/{config}.yaml").decode() for config in POCKET_CONFIGS}


def pocket():
    # Profiles share revisions and some files; each profile lists every file it reads.
    needed = {}
    for config, text in pocket_configs().items():
        profile = f"pocket-tts:{config}"
        voice = POCKET_VOICES[config.split("_")[0]]
        files = [(f"languages/{config}/embeddings/{voice}.safetensors", POCKET_VOICE_REVISION)]
        for key, ref in re.findall(r"^\s*(\w+): hf://(\S+)$", text, re.M):
            if key == "weights_path" and ref.startswith("kyutai/pocket-tts/"):
                continue  # Gated voice-cloning weights. Offline, the SDK loads the public ones.
            if not ref.startswith(f"{POCKET}/"):
                raise ValueError(f"Unexpected Pocket TTS asset in {config}: {key}")
            files.append(tuple(ref.removeprefix(f"{POCKET}/").split("@")))
        for name, revision in files:
            needed.setdefault(revision, {}).setdefault(name, []).append(profile)
    for revision, names in needed.items():
        directory = f"hub/models--{POCKET.replace('/', '--')}/snapshots/{revision}"
        for file in snapshot(POCKET, revision, directory, None, set(names)):
            for profile in names[file["path"].removeprefix(f"{directory}/")]:
                yield {**file, "profile": profile}
    for file in snapshot(POCKET, POCKET_VOICE_REVISION, f"licenses/{POCKET}/{POCKET_VOICE_REVISION}",
                         None, {"README.md"}):
        for config in POCKET_CONFIGS:
            yield {**file, "profile": f"pocket-tts:{config}"}


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
    for repo, revision, only, profile in PHONON:
        directory = f"hub/models--{repo.replace('/', '--')}/snapshots/{revision}"
        files.extend(snapshot(repo, revision, directory, profile, only))
    files.extend(pocket())
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
