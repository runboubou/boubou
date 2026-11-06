# Apple Silicon Qwen candidate

From a clean checkout on macOS 15 or newer, use Python 3.11 or newer to build the shared interpreter and resolve the pinned model manifests. Keep the output outside the checkout so the npm build can verify its source commit.

```sh
candidate="$(mktemp -d)"
python3 -B packaging/qwen/lock_models.py "$candidate/models.json"
python3 -B packaging/qwen/build_runner.py "$candidate"
cargo fetch --locked
python3 -B packaging/build-macos.py --qwen "$candidate"
```

The resulting npm tarball includes the interpreter, native wheels, Silero, NLTK and Kokoro phonemizer data. The first `pull` fetches only the selected profile's verified speech and language model weights. Profiles reuse the same runner; original profiles retain Qwen3.5-4B-4bit. The [curated English runtimes](runtimes.json) select fixed 2B, 4B, 9B or 27B LLMs with their STT and TTS. That catalog is shared by the client, asset locking and runner argument construction. Its recommended memory groups and reserved-memory budgets are estimates, not validated hardware minima or quality rankings.

`kokoro:82m` and its larger-LLM variants speak with the American English `af_heart` voice; Qwen CustomVoice English entries use `Aiden`. The 2B entry uses Whisper tiny; other curated English entries use Parakeet. `phonon:31m-en` speaks on the CPU, `supertonic:3` uses Supertonic 3's `M1` voice on ONNX Runtime's CPU provider, and `chattts:en` uses a CPU speaker sampled at startup. Each `pocket-tts:<config>` runs one Pocket TTS 4.0.1 stock config with its default voice on the CPU. Each `kitten-tts:<checkpoint>` runs its ONNX checkpoint with `expr-voice-3-m` on the CPU. `boubou list` shows supported IDs. To distribute a smaller npm package, pass `--url <public immutable HTTPS URL>` to the runner builder and host its exact `runner.tar.gz` at that URL.

Test the tarball from outside the checkout on an idle Mac. Use the same new cache for every command. Grant microphone access and test multi-turn conversation in the profile's language, audible interruption, a new turn, offline restart, and Ctrl+C for each model. Record the machine and measurements in [#29](https://github.com/entropyai-labs/boubou/issues/29) for CustomVoice and [#34](https://github.com/entropyai-labs/boubou/issues/34) for the other families.

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
