"""Write the candidate's models.json using exact upstream Git objects."""

import argparse
import hashlib
import io
import json
import re
import subprocess
import tempfile
import time
import zipfile
from pathlib import Path
from urllib.parse import quote
from urllib.request import urlopen

# Model files are stored as normal Git blobs or Git LFS pointers on the Hub.
LFS = re.compile(
    rb"version https://git-lfs.github.com/spec/v1\noid sha256:([0-9a-f]{64})\nsize ([0-9]+)\n?"
)

# Kokoro-82M's voices. The first letter of a name is its language. Parakeet hears American and British
# English, Spanish, French, Italian and Brazilian Portuguese; Whisper, which only the Mac runner has, also
# hears Hindi, Japanese and Mandarin.
KOKORO_VOICES = [
    "af_heart", "af_alloy", "af_aoede", "af_bella", "af_jessica", "af_kore", "af_nicole", "af_nova",
    "af_river", "af_sarah", "af_sky", "am_adam", "am_echo", "am_eric", "am_fenrir", "am_liam",
    "am_michael", "am_onyx", "am_puck", "am_santa", "bf_alice", "bf_emma", "bf_isabella", "bf_lily",
    "bm_daniel", "bm_fable", "bm_george", "bm_lewis", "ef_dora", "em_alex", "em_santa", "ff_siwis",
    "hf_alpha", "hf_beta", "hm_omega", "hm_psi", "if_sara", "im_nicola", "jf_alpha",
    "jf_gongitsune", "jf_nezumi", "jf_tebukuro", "jm_kumo", "pf_dora", "pm_alex", "pm_santa",
    "zf_xiaobei", "zf_xiaoni", "zf_xiaoxiao", "zf_xiaoyi", "zm_yunjian", "zm_yunxi", "zm_yunxia",
    "zm_yunyang",
]

# The MLX repositories contain one conversion each, including the nested TTS codec.
# Kokoro ships its card, weights and voices, not samples.
MODELS = [
    ("parakeet-tdt-0.6b-v3", "ed2b7e8c15f9aaa0b5772e2efb986255eaef7e15", None, None),
    ("Qwen3.5-4B-4bit", "0e7ffd5c629ef7719d4cbc04069232580bfa9d9c", None, None),
    ("Qwen3.5-2B-4bit", "674aaa7240b91e8012fcad5d791b7dfe5ba90207", None, None),
    ("Qwen3.5-9B-4bit", "8b2b98c00a6b4d291155e4890773ca8f769aee53", None,
     {"README.md", "chat_template.jinja", "config.json", "model-00001-of-00002.safetensors",
      "model-00002-of-00002.safetensors", "model.safetensors.index.json",
      "preprocessor_config.json", "processor_config.json", "tokenizer.json",
      "tokenizer_config.json", "video_preprocessor_config.json", "vocab.json"}),
    ("Qwen3.5-27B-4bit", "45797d2985a12c55e6473686e9ea91b95e959553", None,
     {"README.md", "chat_template.jinja", "config.json", "generation_config.json",
      "model-00001-of-00003.safetensors", "model-00002-of-00003.safetensors",
      "model-00003-of-00003.safetensors", "model.safetensors.index.json",
      "preprocessor_config.json", "processor_config.json", "tokenizer.json",
      "tokenizer_config.json", "video_preprocessor_config.json", "vocab.json"}),
    ("Qwen3-TTS-12Hz-0.6B-CustomVoice-6bit", "7dc92af14613355896fcab13b268c19ede233139", "qwen3-tts:0.6b-customvoice", None),
    ("Qwen3-TTS-12Hz-1.7B-CustomVoice-6bit", "1c6c0ff58c43afa8df571facde2efa077efd85e2", "qwen3-tts:1.7b-customvoice", None),
    ("Kokoro-82M-bf16", "a71e4d38b236d968966a2002c4c895dbd12b1c3c", "kokoro:82m",
     {"README.md", "config.json", "kokoro-v1_0.safetensors", *(f"voices/{v}.safetensors" for v in KOKORO_VOICES)}),
]
# Retain original attribution alongside the MLX model cards.
ORIGINALS = [
    ("nvidia/parakeet-tdt-0.6b-v3", "541d1f99c6b0c3cd0b11a95167540bb8edefd82b", {"README.md"}, None),
    ("Qwen/Qwen3.5-4B", "851bf6e806efd8d0a36b00ddf55e13ccb7b8cd0a", {"LICENSE", "README.md"}, None),
    ("Qwen/Qwen3.5-2B", "15852e8c16360a2fea060d615a32b45270f8a8fc", {"LICENSE", "README.md"}, None),
    ("Qwen/Qwen3.5-9B", "c202236235762e1c871ad0ccb60c8ee5ba337b9a", {"LICENSE", "README.md"}, None),
    ("Qwen/Qwen3.5-27B", "fc05daec18b0a78c049392ed2e771dde82bdf654", {"LICENSE", "README.md"}, None),
    ("Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice", "85e237c12c027371202489a0ec509ded67b5e4b5", {"README.md"}, "qwen3-tts:0.6b-customvoice"),
    ("Qwen/Qwen3-TTS-12Hz-1.7B-CustomVoice", "0c0e3051f131929182e2c023b9537f8b1c68adfe", {"README.md"}, "qwen3-tts:1.7b-customvoice"),
    ("hexgrad/Kokoro-82M", "f3ff3571791e39611d31c381e3a41a3af07b4987", {"README.md"}, "kokoro:82m"),
]
# Qwen3-TTS Base weights clone a voice; CustomVoice weights cannot. A Qwen3-TTS entry reads a cloned
# voice with the Base weights of its own size. `boubou clone` holds Whisper, which writes down what an
# uploaded clip says, and adds the Base weights of the size Boubou chose for the computer. Nobody else
# downloads them.
CLONE = "qwen3-tts:clone"
BASE = [
    ("0.6B", "4e44ed4bcee28a0f89a493e07bde16e6dccd43eb", "5d83992436eae1d760afd27aff78a71d676296fc"),
    ("1.7B", "34ff5318365b59cba9c03ff729f2eee0814caf72", "fd4b254389122332181a7c3db7f27e918eec64e3"),
]
# VoxCPM2 (Apache-2.0) reads the voices cloned in the twenty languages it speaks and Qwen3-TTS does
# not, from the recording and its words. A Qwen3-TTS entry or `boubou clone` fetches it once such a
# voice exists: the MLX 8-bit weights, and the original card for its license.
# OmniVoice reads the voices cloned in the other languages Whisper hears, with the files its own entry
# runs. Its model allows noncommercial use only; Boubou asks the person to accept that first.
OMNIVOICE_CLONE = "omnivoice:clone"
VOXCPM = ("voxcpm2:2b", "VoxCPM2-8bit", "d52725898a0675703f7f9ddc5a4d1a3cdbb99032",
          {"README.md", "config.json", "model.safetensors", "special_tokens_map.json", "tokenizer.json",
           "tokenizer_config.json"},
          "openbmb/VoxCPM2", "7936080b41163874f38610f46f438e2c0757f9fd")
# ptts 0.4.0 defaults: its checkpoint, read locally (q8 weights, tokenizer, default voice), and its
# voice catalog, which the upstream handler lists at startup. The Hub client ptts uses ignores
# HF_HUB_OFFLINE, but returns a file cached at an exact commit without connecting.
# The catalog's English voices, by name and voice ID, the ones the English checkpoint speaks with.
PHONON_VOICES = [
    ("Zoey", "NbpkqMVS3CJeq2j8"), ("Juniper", "NIWIOv5vTi5eRMj8"), ("Everly", "vjFyp2BcCyTlUmYb"),
    ("Harper", "4SZHfMpw-p46Ywgs"), ("Hadley", "bXwrkHhURL1VFJdG"), ("Delia", "nGzMHMPAWGaJFHOo"),
    ("Easton", "LMDIYE0Oa377Qh6Z"), ("Micah", "40QrMVgAxFiUnP9f"), ("Trevor", "0zLApsZfVLLNaEGl"),
    ("Russell", "_6Aslh2DxfmnRLmP"), ("Preston", "qIxlZ7Ei8KvWlyQH"), ("Miles", "d9XCyhjo4JhkHF1d"),
    ("Maeve", "6PWnV0Nq4wu7RVBT"), ("Freya", "GgfEkEJtxZR7gnpy"), ("Pippa", "uem82D50GRv2Dwma"),
    ("Elodie-Rose", "y9bQqIDnyxwqK01t"), ("Imogen", "gDT1nz7Ie36ZhL-C"), ("Lottie", "m9MxpY6IUGXtWp76"),
    ("Alfie", "X6NceqF285Gr04gb"), ("Hugh", "YyKcxPG7iIgS3jsK"), ("Edwin", "zUCxzrOrugTSis03"),
    ("Spencer", "JOW0vchyswkQUMDS"), ("Toby", "dME3IWyZBvmh1n1q"), ("Teddy", "Zn7g6NxLqXlJb7Qi"),
    ("Bryony", "gfP44RlHwr2kEwk7"), ("Captain Silas Blackwater", "xPDpsV3F592srkjA"),
    ("Morwenna of the Deep", "nOykTmcH9ydsGGIc"), ("Old Mortlake", "lX3LavMNdlIbgEHH"),
    ("Azgorath", "dGyGDKo2osj7h3s7"), ("Fennick the Young Elf", "cXoIXJ2llS8S0WOW"),
    ("The Thing Under the Stairs", "GXaSqMEmOg12OIYb"), ("Vyrmathrax the Ancient", "xO8xQo3BrB8FuI6J"),
    ("Grub the Ogre", "cYtsRSxV5q9IMehn"), ("Master Aldric", "gSgG5Tor0GPEnqzC"),
    ("The Snow Queen", "NIWoEurJHMJjyUTe"), ("Sir Bramwell", "y2eiKxtQo2B67deU"),
    ("Fairy Godmother", "QXNr0SvkukLB4wPq"),
]
PHONON = [
    ("gradium/phonon-31m-en", "8c64b55ec301728e2d1695bfea87a08a267d0c2d",
     {"README.md", "config.json", "default-voice.safetensors", "model.q8.gguf", "tokenizer.json"}, "phonon:31m-en"),
    ("gradium/phonon-voices", "da6c9ec0d47be8df5c87c730a3aa97187f3229c3",
     {"voices.jsonl", *(f"voices/{voice_id}.safetensors" for _, voice_id in PHONON_VOICES)}, "phonon:31m-en"),
]
# supertonic 1.3.1 loads the revision it pins from one directory: the ONNX graphs, their configs and
# the voice styles, M1 the upstream default. The stub config.json marks the snapshot for voice.py.
SUPERTONIC_VOICES = ["M1", "M2", "M3", "M4", "M5", "F1", "F2", "F3", "F4", "F5"]
SUPERTONIC = ("Supertone/supertonic-3", "724fb5abbf5502583fb520898d45929e62f02c0b",
              {"LICENSE", "README.md", "config.json", "onnx/tts.json", "onnx/unicode_indexer.json",
               "onnx/duration_predictor.onnx", "onnx/text_encoder.onnx", "onnx/vector_estimator.onnx",
               "onnx/vocoder.onnx", *(f"voice_styles/{v}.json" for v in SUPERTONIC_VOICES)}, "supertonic:3")
# The assets ChatTTS 0.2.5 checks against its own hashes and loads; the card states the CC BY-NC 4.0 terms.
CHATTTS = ("2Noise/ChatTTS", "1a3c04a8b0651689bd9242fbb55b1f4b5a9aef84",
           {"README.md", "asset/Decoder.safetensors", "asset/DVAE.safetensors", "asset/Embed.safetensors",
            "asset/Vocos.safetensors", "asset/gpt/config.json", "asset/gpt/model.safetensors",
            "asset/tokenizer/special_tokens_map.json", "asset/tokenizer/tokenizer_config.json",
            "asset/tokenizer/tokenizer.json"}, "chattts:en")
# English entry points; wider language coverage is tracked separately in Issue #34.
MMS = ("facebook/mms-tts-eng", "c71de0fe7204c83f1c10820a7d696d0b450048ba",
       {"README.md", "config.json", "model.safetensors", "special_tokens_map.json",
        "tokenizer_config.json", "vocab.json"}, "mms-tts:eng")
OMNIVOICE = ("k2-fsa/OmniVoice", "c5fdb5ccb189668d56333f77ba2629f4cd7535f4",
             {"README.md", "chat_template.jinja", "config.json", "model.safetensors",
              "tokenizer.json", "tokenizer_config.json", "audio_tokenizer/LICENSE",
              "audio_tokenizer/README.md", "audio_tokenizer/config.json",
              "audio_tokenizer/model.safetensors", "audio_tokenizer/preprocessor_config.json"},
             "omnivoice:0.6b")
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
# The SDK's preset voices. Every config has a voice state for each, which speaks the config's language.
POCKET_PRESETS = [
    "alba", "marius", "javert", "jean", "fantine", "cosette", "eponine", "azelma", "anna", "vera",
    "charles", "paul", "george", "mary", "jane", "michael", "eve", "bill_boerst", "peter_yearsley",
    "stuart_bell", "caro_davy", "giovanni", "lola", "juergen", "rafael", "daan", "estelle",
]
# KittenTTS ONNX checkpoints: every file but .gitattributes. The config names the weights and voices.
KITTEN = [
    (f"KittenML/kitten-tts-{name}", revision, {"README.md", "config.json", weights, "voices.npz"}, f"kitten-tts:{name}")
    for name, revision, weights in [
        ("mini-0.8", "c02725660cea441db4c383af69f1f26f5cd00947", "kitten_tts_mini_v0_8.onnx"),
        ("micro-0.8", "1ccf72b2c2048fd17efac7de2fab32d10e225084", "kitten_tts_micro_v0_8.onnx"),
        ("nano-0.8-fp32", "e73cfb095bd949c786ccdcf3b8ab04eb36c47442", "kitten_tts_nano_v0_8.onnx"),
        ("nano-0.8-int8", "84781d74e29ee25217551556398b42f80593a813", "kitten_tts_nano_v0_8.onnx"),
        ("mini-0.1", "01d5d9ef9d365444e6d3669f7f3fe8a20bad9439", "kitten_tts_mini_v0_1.onnx"),
        ("nano-0.1", "7d99aae46867223af52d5ea16d076d8839ce1a2e", "kitten_tts_nano_v0_1.onnx"),
        ("nano-0.2", "9c81564aa56c6fb79f83780e87099357b88d6617", "kitten_tts_nano_v0_2.onnx"),
    ]
]


# Upstream's CUDA engines: Parakeet through nano-parakeet, Qwen3.5-4B in 4 bits through llama.cpp
# and Kokoro through its PyTorch package. A file without a profile serves every Linux profile.
CUDA = [
    ("nvidia/parakeet-tdt-0.6b-v3", "541d1f99c6b0c3cd0b11a95167540bb8edefd82b",
     {"README.md", "parakeet-tdt-0.6b-v3.nemo"}, None),
    ("unsloth/Qwen3.5-4B-GGUF", "e87f176479d0855a907a41277aca2f8ee7a09523",
     {"README.md", "Qwen3.5-4B-Q4_K_M.gguf"}, None),
    ("hexgrad/Kokoro-82M", "f3ff3571791e39611d31c381e3a41a3af07b4987",
     {"README.md", "config.json", "kokoro-v1_0.pth", *(f"voices/{v}.pt" for v in KOKORO_VOICES if v[0] not in "hjz")},
     "kokoro:82m"),
]


def retried(action, attempts=4):
    """Run a network call again after a short wait; the Hub sometimes answers 502 or drops a fetch."""
    for attempt in range(1, attempts + 1):
        try:
            return action()
        except (OSError, subprocess.CalledProcessError):
            if attempt == attempts:
                raise
            time.sleep(15 * attempt)


def snapshot(repo, revision, destination, profile, only=None):
    url = f"https://huggingface.co/{repo}"
    with tempfile.TemporaryDirectory() as folder:
        def git(*args):
            return subprocess.check_output(["git", *args], cwd=folder)

        git("init", "--bare", "--quiet")
        retried(lambda: git("-c", "fetch.fsckObjects=true", "fetch", "--quiet", "--depth=1", url, revision))
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
    def download():
        with urlopen(url, timeout=90) as response:
            return response.read()

    wheel = retried(download)
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
        embeddings = f"languages/{config}/embeddings"
        files = [(f"{embeddings}/{voice}.safetensors", POCKET_VOICE_REVISION, profile)]
        # voice.py reads one voice state per profile, so each other preset has a profile of its own,
        # pocket-tts:<config>/<voice>, which Boubou downloads only when --voice names that voice.
        files += [(f"{embeddings}/{other}.safetensors", POCKET_VOICE_REVISION, f"{profile}/{other}")
                  for other in POCKET_PRESETS if other != voice]
        for key, ref in re.findall(r"^\s*(\w+): hf://(\S+)$", text, re.M):
            if key == "weights_path" and ref.startswith("kyutai/pocket-tts/"):
                continue  # Gated voice-cloning weights. Offline, the SDK loads the public ones.
            if not ref.startswith(f"{POCKET}/"):
                raise ValueError(f"Unexpected Pocket TTS asset in {config}: {key}")
            files.append((*ref.removeprefix(f"{POCKET}/").split("@"), profile))
        for name, revision, owner in files:
            needed.setdefault(revision, {}).setdefault(name, []).append(owner)
    for revision, names in needed.items():
        directory = f"hub/models--{POCKET.replace('/', '--')}/snapshots/{revision}"
        for file in snapshot(POCKET, revision, directory, None, set(names)):
            for profile in names[file["path"].removeprefix(f"{directory}/")]:
                yield {**file, "profile": profile}
    for file in snapshot(POCKET, POCKET_VOICE_REVISION, f"licenses/{POCKET}/{POCKET_VOICE_REVISION}",
                         None, {"README.md"}):
        for config in POCKET_CONFIGS:
            yield {**file, "profile": f"pocket-tts:{config}"}


WHISPER_PROFILES = {
    "qwen3-tts:1.7b-customvoice": "qwen3-tts:1.7b-customvoice-whisper",
    "qwen3-tts:0.6b-customvoice": "qwen3-tts:0.6b-customvoice-whisper",
    "chattts:en": "chattts:whisper",
    "kokoro:82m": "kokoro:82m-whisper",
}


def runnable(profile):
    """Whether Boubou runs `profile`. A Pocket TTS voice profile only adds a voice state to its config's."""
    return "/" not in profile


def with_whisper_profiles(files):
    english = sorted({file["profile"] for file in files if "profile" in file and runnable(file["profile"])})
    result = []
    for file in files:
        profile = file.get("profile")
        if profile in WHISPER_PROFILES:
            file = {key: value for key, value in file.items() if key != "profile"}
            file["profiles"] = [profile, WHISPER_PROFILES[profile]]
        elif file["path"].startswith("hub/models--mlx-community--parakeet-tdt-0.6b-v3/"):
            file = {**file, "profiles": english}
        result.append(file)
    for repo, revision, only in [
        ("mlx-community/whisper-large-v3-turbo", "a4aaeec0636e6fef84abdcbe3544cb2bf7e9f6fb",
         {"README.md", "config.json", "weights.safetensors"}),
        ("openai/whisper-large-v3", "06f233fe06e710322aca913c1bc4249a0d71fce1",
         {"README.md", "added_tokens.json", "merges.txt", "normalizer.json", "preprocessor_config.json",
          "special_tokens_map.json", "tokenizer.json", "tokenizer_config.json", "vocab.json"}),
    ]:
        directory = f"hub/models--{repo.replace('/', '--')}/snapshots/{revision}"
        result.extend({**file, "profiles": list(WHISPER_PROFILES.values())}
                      for file in snapshot(repo, revision, directory, None, only))
    return result


def with_runtime_profiles(files):
    presets = json.loads((Path(__file__).parent / "runtimes.json").read_text())
    legacy = {profile for file in files
              for profile in file.get("profiles", [file["profile"]] if "profile" in file else [])
              if runnable(profile)}
    tiny_profiles = [preset["id"] for preset in presets if preset["stt"] == "whisper-tiny"]
    repo, revision = "openai/whisper-tiny", "169d4a4341b33bc18d8881c4b69c2e104e1cc0af"
    names = {"README.md", "added_tokens.json", "config.json", "generation_config.json",
             "merges.txt", "model.safetensors", "normalizer.json", "preprocessor_config.json",
             "special_tokens_map.json", "tokenizer.json", "tokenizer_config.json", "vocab.json"}
    files = [*files, *({**file, "profiles": tiny_profiles} for file in snapshot(
        repo, revision, f"hub/models--openai--whisper-tiny/snapshots/{revision}", None, names))]
    result = []
    for file in files:
        owners = set(file.get("profiles", [file["profile"]] if "profile" in file else legacy))
        owners.update(preset["id"] for preset in presets if preset["tts"] in owners)
        llm = re.match(r"(?:hub/models--mlx-community--|licenses/Qwen/)(Qwen3\.5-(?:2|4|9|27)B)(?:-4bit)?/", file["path"])
        if llm:
            name = llm[1] + "-4bit"
            owners = (legacy if name == "Qwen3.5-4B-4bit" else set()) | {
                preset["id"] for preset in presets if preset["llm"] == name}
        elif file["path"].startswith("hub/models--mlx-community--parakeet-tdt-0.6b-v3/"):
            owners -= {preset["id"] for preset in presets if preset["stt"] != "parakeet-tdt-0.6b-v3"}
        if not owners:
            raise ValueError(f"No runtime owns {file['path']}")
        result.append({**{key: value for key, value in file.items() if key not in ("profile", "profiles")},
                       "profiles": sorted(owners)})
    return result


def with_cloning(files):
    """Add the Base weights and VoxCPM2, which own nothing else, lend Whisper to `boubou clone`, and
    OmniVoice's weights to the voices it reads (OMNIVOICE_CLONE)."""
    whisper = ("hub/models--mlx-community--whisper-large-v3-turbo/", "hub/models--openai--whisper-large-v3/")
    files = [{**file, "profiles": [*file["profiles"], CLONE]} if file["path"].startswith(whisper) else file
             for file in files]
    omnivoice = f"hub/models--{OMNIVOICE[0].replace('/', '--')}/"
    files = [{**file, "profiles": [*file["profiles"], OMNIVOICE_CLONE]} if file["path"].startswith(omnivoice) else file
             for file in files]
    for size, revision, original in BASE:
        profile = f"qwen3-tts:{size.lower()}-base"
        name, repo = f"Qwen3-TTS-12Hz-{size}-Base-6bit", f"Qwen/Qwen3-TTS-12Hz-{size}-Base"
        for file in [*snapshot(f"mlx-community/{name}", revision, f"hub/models--mlx-community--{name}/snapshots/{revision}", profile),
                     *snapshot(repo, original, f"licenses/{repo}/{original}", profile, {"README.md"})]:
            del file["profile"]
            files.append({**file, "profiles": [profile]})
    profile, name, revision, only, repo, original = VOXCPM
    for file in [*snapshot(f"mlx-community/{name}", revision, f"hub/models--mlx-community--{name}/snapshots/{revision}", profile, only),
                 *snapshot(repo, original, f"licenses/{repo}/{original}", profile, {"README.md"})]:
        del file["profile"]
        files.append({**file, "profiles": [profile]})
    return files


def shared():
    """Weights both runners load the same way, the turn detector and license texts."""
    files = []
    for repo, revision, only, profile in [*PHONON, SUPERTONIC, CHATTTS, *KITTEN, MMS, OMNIVOICE]:
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
    files.append({
        "path": "licenses/mms/CC-BY-NC-4.0.txt",
        "url": "https://raw.githubusercontent.com/creativecommons/cc-legal-tools-data/a0dac18d1a773b4cdb7d8dd5d9bf4efab7901030/docs/licenses/by-nc/4.0/legalcode.txt",
        "size": 19347,
        "sha256": "41003d4a74749c0220e33dd415042164b5a1093ed401f36277234f772d22d3d0",
        "profile": "mms-tts:eng",
    })
    return files


def cuda():
    """List the x86-64 Linux runner's files and the profiles each one serves."""
    files = []
    for repo, revision, only, profile in CUDA:
        directory = f"hub/models--{repo.replace('/', '--')}/snapshots/{revision}"
        files.extend(snapshot(repo, revision, directory, profile, only))
    for repo, revision, only, profile in ORIGINALS:
        if repo == "Qwen/Qwen3.5-4B":
            files.extend(snapshot(repo, revision, f"licenses/{repo}/{revision}", profile, only))
    # ChatTTS takes about 9 s for its first sentence on CUDA, so it stays on the Mac.
    files.extend(file for file in shared() if file.get("profile") != CHATTTS[3])
    everyone = sorted({file["profile"] for file in files if "profile" in file and runnable(file["profile"])})
    return [{**{key: value for key, value in file.items() if key != "profile"},
             "profiles": [file["profile"]] if "profile" in file else everyone} for file in files]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="output path for models.json")
    parser.add_argument("--cuda", action="store_true", help="lock the x86-64 Linux CUDA runner's models")
    args = parser.parse_args()
    if args.cuda:
        args.output.write_text(json.dumps(cuda(), indent=2) + "\n")
        return
    files = []
    for name, revision, profile, only in MODELS:
        directory = f"hub/models--mlx-community--{name}/snapshots/{revision}"
        files.extend(snapshot(f"mlx-community/{name}", revision, directory, profile, only))
    for repo, revision, only, profile in ORIGINALS:
        files.extend(snapshot(repo, revision, f"licenses/{repo}/{revision}", profile, only))
    files.extend(shared())
    args.output.write_text(json.dumps(with_cloning(with_runtime_profiles(with_whisper_profiles(files))), indent=2) + "\n")


if __name__ == "__main__":
    main()
