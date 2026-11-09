# Qwen runner

On an Apple Silicon Mac, from a clean checkout on macOS 15 or newer, use Python 3.11 or newer and Xcode Command Line Tools to build the shared interpreter and resolve the pinned model manifests. Keep the output outside the checkout so the npm build can verify its source commit.

```sh
candidate="$(mktemp -d)"
version=$(python3 -c 'import json; print(json.load(open("packaging/package.json"))["version"])')
python3 -B packaging/qwen/lock_models.py "$candidate/models.json"
python3 -B packaging/qwen/build_runner.py "$candidate" \
  --url "https://github.com/runboubou/boubou/releases/download/v$version/boubou-qwen-runner-$version-darwin-arm64.tar.gz"
cargo fetch --locked
python3 -B packaging/build-macos.py --qwen "$candidate"
```

The runner archive holds the interpreter, native wheels, Silero, NLTK and
phonemizer data. It is too large for npm, so the Release workflow publishes it
at that URL and the npm package carries only its size and SHA-256. The first
`pull` downloads and verifies the runner, then fetches only the selected
profile's verified weights. Profiles reuse the same runner. `runtime.json` also
records a digest of the unpacked runner, so each start hashes the installed
files on several threads instead of re-reading the archive, which is removed
once installed. The [runtime catalog](runtimes.json) is
shared by the client, asset locking and runner arguments. Its `guided` entries
are Qwen 0.6B and 1.7B CustomVoice with fixed Aiden, Pocket TTS English
(`english_2026-10`, Alba) and Phonon 31M English. All use Parakeet and
Qwen3.5-4B. The client also offers Liquid Audio and Liquid Omni integrated
voices, preferring Audio within that family.

Guided startup uses English. Other voices, Whisper language modes and the 2B,
9B and 27B LLM combinations require explicit developer selection. Memory groups
and budgets are estimates for device testing. More parameters do not imply
better replies or voice quality.

`kokoro:82m` and its larger-LLM variants speak with the American English `af_heart` voice; Qwen CustomVoice English entries use `Aiden`. The 2B entry uses Whisper tiny; other curated English entries use Parakeet. `phonon:31m-en` speaks on the CPU, `supertonic:3` uses Supertonic 3's `M1` voice on ONNX Runtime's CPU provider, and `chattts:en` uses one fixed ChatTTS speaker on the CPU. Each `pocket-tts:<config>` runs one Pocket TTS 4.0.1 stock config with its default voice on the CPU. Each `kitten-tts:<checkpoint>` runs its ONNX checkpoint with `expr-voice-3-m` on the CPU. `boubou list` shows supported IDs.

Test the tarball from outside the checkout on an idle Mac. Use the same new cache for every command. Grant microphone access and test multi-turn conversation in the profile's language, audible interruption, a new turn, offline restart, and Ctrl+C for each model. Report the machine and measurements in [#5](https://github.com/runboubou/boubou/issues/5).

Before the release exists, place the built runner where `pull` would download it:

```sh
export BOUBOU_HOME="$(mktemp -d)"
sha=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["sha256"])' "$candidate/runtime.json")
mkdir -p "$BOUBOU_HOME/qwen/runtimes/$sha"
cp "$candidate/runner.tar.gz" "$BOUBOU_HOME/qwen/runtimes/$sha/"
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run qwen3-tts:1.7b-customvoice
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run qwen3-tts:0.6b-customvoice
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run kokoro:82m
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run kitten-tts:mini-0.8
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run phonon:31m-en
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run supertonic:3
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run chattts:en
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run pocket-tts:french_24l
```

## Other English TTS families

These entries use Parakeet TDT v3 and Qwen3.5-4B-4bit and require explicit
developer selection. MMS is excluded from guided recommendations. OmniVoice
speaks with one fixed voice, designed as a young American man.

| ID | Local TTS engine | Weight terms |
| --- | --- | --- |
| `mms-tts:eng` | Transformers VITS on CPU | CC-BY-NC 4.0 |
| `omnivoice:0.6b` | OmniVoice on MPS; its audio codec runs on CPU | Noncommercial, per the pinned model card |

Use `boubou pull ID`, `boubou doctor ID`, then `boubou run ID --offline`.
MMS currently exposes only the English checkpoint; this does not establish
support for the full MMS language catalog in Issue #34. Model cards and license
texts supplied upstream are retained with the pinned weights. Review their terms
before redistribution or commercial use.

## Choose a speech recognizer

The default entries keep Parakeet TDT 0.6B v3 for fast English conversations.
Choose a Whisper entry explicitly for Chinese or Chinese/English conversations:

| TTS | Default: Parakeet | Optional: Whisper large-v3-turbo |
| --- | --- | --- |
| Qwen3-TTS 0.6B | `qwen3-tts:0.6b-customvoice` | `qwen3-tts:0.6b-customvoice-whisper` |
| Qwen3-TTS 1.7B | `qwen3-tts:1.7b-customvoice` | `qwen3-tts:1.7b-customvoice-whisper` |
| ChatTTS | `chattts:en` | `chattts:whisper` |

Whisper entries detect the input language and reply in that language. Qwen3-TTS
uses automatic synthesis language and the `Vivian` voice in these entries;
its default entries keep English with `Aiden`. Both ChatTTS entries use the same
fixed speaker. The light 2B English runtime uses Whisper tiny; other profiles retain Parakeet.

A profile downloads only its selected recognizer. Paired entries share TTS
weights and the same 4-bit Qwen3.5-4B language model. Whisper adds recognition
latency: on four English clips on an idle M5 Max, warmed STT medians were
35.3 ms for Parakeet and 393.4 ms for Whisper. This excludes endpointing, LLM,
TTS and startup; it does not predict performance on other Macs.

Whisper's MLX conversion and OpenAI processor files are pinned separately. The
runner combines them in temporary storage, so `--offline` needs no Hub request
and does not modify the verified model snapshots. This implementation requires
Apple Silicon and macOS 15 or newer. Other Whisper implementations need their
own model formats and dependencies; changing another component's backend does
not switch this package's STT engine.

The upstream framework supports `mlx-lm`, `transformers`, `responses-api` and
`chat-completions` LLM backends. Boubou's packaged profiles use `mlx-lm` on a Mac,
`transformers` on Linux, and the pinned checkpoints selected by each complete runtime. Other component combinations
and inference backends require a separately configured developer runtime.

## x86-64 Linux with an NVIDIA GPU

The Linux runner follows upstream's llama.cpp setup on CUDA: Parakeet TDT 0.6B
v3 through nano-parakeet, Qwen3.5-4B as Unsloth's Q4_K_M GGUF through
llama.cpp b11960's CUDA 13 build, and Kokoro through its PyTorch package.
Phonon, Supertonic, MMS, OmniVoice, Pocket TTS and KittenTTS load the same
weights as on a Mac, and OmniVoice runs on CUDA.
llama.cpp uses the CUDA libraries installed with PyTorch, so the runner carries
one CUDA 13 line and no CUDA toolkit is needed. `pull` checks for glibc 2.39
and NVIDIA driver 580.65.06 before downloading anything.

Build on Ubuntu 24.04 or newer with Python 3.11 or newer and Git. Put the
runner next to a release build, where `boubou` looks for it:

```sh
cargo build --locked --release
out=target/release/qwen
mkdir -p "$out"
python3 -B packaging/qwen/lock_models.py --cuda "$out/models.json"
python3 -B packaging/qwen/build_runner.py "$out"
target/release/boubou pull kokoro:82m
target/release/boubou run kokoro:82m --offline
```

The runner is one 3.6 GB archive, 6.2 GB unpacked, shared by every profile.
PyTorch and its CUDA libraries are 2.7 GB of its 3.3 GB of wheels, and all
voice packages together are 0.17 GB, so voices are not separate downloads.
Every profile shares 5.3 GB of models (Parakeet and Qwen3.5-4B); a voice adds
0.02 to 3.3 GB.

Measured on Ubuntu 24.04 with an RTX 5090 (driver 595.84), a Ryzen 9 9950X and
about 110 MB/s from Hugging Face:

| Step | Result |
| --- | --- |
| First `pull kokoro:82m` from a source build | 87 s |
| Verify the runner and models on each start | 1.7 s |
| `run` until it is listening | 14–16 s |
| Peak GPU memory | 6.3–7.3 GiB; OmniVoice 8.3 GiB |
| First audio after a text turn (`smoke.py`) | 0.5–0.8 s for most voices; up to 1.2 s |

Not on Linux: ChatTTS, which takes about 9 s to voice its first sentence on
CUDA too. Not yet: Qwen3-TTS, whose CUDA 13 qwentts.cpp wheel lacks RTX 50
kernels and whose PyTorch path fails with Transformers 5.18; the Whisper
entries; the presets with other Qwen3.5 sizes or Whisper tiny; and guided
startup.
