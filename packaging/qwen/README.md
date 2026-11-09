# Apple Silicon Qwen candidate

From a clean checkout on macOS 15 or newer, use Python 3.11 or newer, CMake 3.24+ and Xcode Command Line Tools to build the shared interpreter and resolve the pinned model manifests. Keep the output outside the checkout so the npm build can verify its source commit.

```sh
candidate="$(mktemp -d)"
python3 -B packaging/qwen/lock_models.py "$candidate/models.json"
python3 -B packaging/qwen/build_runner.py "$candidate"
cargo fetch --locked
python3 -B packaging/build-macos.py --qwen "$candidate"
```

The resulting npm tarball includes the interpreter, native wheels, Silero, NLTK
and phonemizer data. The first `pull` fetches only the selected profile's verified
weights. Profiles reuse the same runner. The [runtime catalog](runtimes.json) is
shared by the client, asset locking and runner arguments. Its `guided` entries
are Qwen 0.6B and 1.7B CustomVoice with fixed Aiden, Pocket TTS English
(`english_2026-10`, Alba) and Phonon 31M English. All use Parakeet and
Qwen3.5-4B. The client also offers Liquid Audio and Liquid Omni integrated
voices, preferring Audio within that family.

Guided startup uses English. Other voices, Whisper language modes and the 2B,
9B and 27B LLM combinations require explicit developer selection. Memory groups
and budgets are estimates for device testing. More parameters do not imply
better replies or voice quality.

`kokoro:82m` and its larger-LLM variants speak with the American English `af_heart` voice; Qwen CustomVoice English entries use `Aiden`. The 2B entry uses Whisper tiny; other curated English entries use Parakeet. `phonon:31m-en` speaks on the CPU, `supertonic:3` uses Supertonic 3's `M1` voice on ONNX Runtime's CPU provider, and `chattts:en` uses a CPU speaker sampled at startup. Each `pocket-tts:<config>` runs one Pocket TTS 4.0.1 stock config with its default voice on the CPU. Each `kitten-tts:<checkpoint>` runs its ONNX checkpoint with `expr-voice-3-m` on the CPU. `boubou list` shows supported IDs. To distribute a smaller npm package, pass `--url <public immutable HTTPS URL>` to the runner builder and host its exact `runner.tar.gz` at that URL.

Test the tarball from outside the checkout on an idle Mac. Use the same new cache for every command. Grant microphone access and test multi-turn conversation in the profile's language, audible interruption, a new turn, offline restart, and Ctrl+C for each model. Report the machine and measurements in [#5](https://github.com/runboubou/boubou/issues/5).

```sh
export BOUBOU_HOME="$(mktemp -d)"
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
developer selection. MMS is excluded from guided recommendations. The current
OmniVoice and Drama-3 profiles do not fix a speaker identity.

| ID | Local TTS engine | Weight terms |
| --- | --- | --- |
| `mms-tts:eng` | Transformers VITS on CPU | CC-BY-NC 4.0 |
| `omnivoice:0.6b` | OmniVoice on MPS; its audio codec runs on CPU | Noncommercial, per the pinned model card |
| `drama3:4bit` | Native MLX Drama-3 service on the same Mac | Fish Audio Community License |

Use `boubou pull ID`, `boubou doctor ID`, then `boubou run ID --offline`.
The package includes the Drama-3 executable and its libraries; BOUBOU starts it
on an allocated loopback port and stops it with the conversation. No separate
server installation or hosted API is needed.

Drama-3 downloads about 29.4 GB including the shared STT and 4B LLM.
Its upstream native implementation recommends at least 48 GB of unified memory
for the TTS checkpoint alone. That is not an established minimum for the complete
BOUBOU conversation. Smaller-Mac validation and acceptance remain
separate work. MMS currently exposes only the English checkpoint;
this does not establish support for the full MMS language catalog in Issue #34.
Model cards, license texts supplied upstream and the Drama-3 conversion notice
are retained with the pinned weights. Review their terms before redistribution
or commercial use.

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
its default entries keep English with `Aiden`. ChatTTS samples a speaker at
startup. The light 2B English runtime uses Whisper tiny; other profiles retain Parakeet.

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
`chat-completions` LLM backends. Boubou's packaged profiles use `mlx-lm` and the
pinned checkpoints selected by each complete runtime. Other component combinations
and inference backends require a separately configured developer runtime.
