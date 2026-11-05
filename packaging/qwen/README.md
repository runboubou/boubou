# Apple Silicon Qwen candidate

From a clean checkout on macOS 15 or newer, use Python 3.11 or newer to build the shared interpreter and resolve the pinned model manifests. Keep the output outside the checkout so the npm build can verify its source commit.

```sh
candidate="$(mktemp -d)"
python3 -B packaging/qwen/lock_models.py "$candidate/models.json"
python3 -B packaging/qwen/build_runner.py "$candidate"
cargo fetch --locked
python3 -B packaging/build-macos.py --qwen "$candidate"
```

The resulting npm tarball includes the interpreter, native wheels, Silero, NLTK and Kokoro phonemizer data. The first `pull` fetches verified speech and language model weights. Every profile reuses the same runner and Qwen3.5-4B-4bit language model; `kokoro:82m` speaks with the American English `af_heart` voice, `phonon:31m-en` speaks on the CPU, `supertonic:3` speaks English with Supertonic 3's `M1` voice on ONNX Runtime's CPU provider, and `chattts:default` speaks Chinese and English on the CPU with a speaker that ChatTTS samples at random on each start. Each `pocket-tts:<config>` profile runs one Pocket TTS 4.0.1 stock config on the CPU with the SDK's default voice for its language, and the language model replies in that language. `boubou list` shows the 26 configs; the `english` alias is `pocket-tts:english_2026-10`. Each of the seven `kitten-tts:<checkpoint>` profiles runs one KittenTTS ONNX checkpoint on the CPU with the `expr-voice-3-m` voice. To distribute a smaller npm package, pass `--url <public immutable HTTPS URL>` to the runner builder and host its exact `runner.tar.gz` at that URL.

Test the tarball from outside the checkout on an idle Mac. Use the same new cache for every command. Grant microphone access and test multi-turn conversation in the profile's language, audible interruption, a new turn, offline restart, and Ctrl+C for each model. Record the machine and measurements in [#29](https://github.com/entropyai-labs/boubou/issues/29) for CustomVoice and [#34](https://github.com/entropyai-labs/boubou/issues/34) for the other families.

```sh
export BOUBOU_HOME="$(mktemp -d)"
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run qwen3-tts:1.7b-customvoice
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run qwen3-tts:0.6b-customvoice
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run kokoro:82m
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run kitten-tts:mini-0.8
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run phonon:31m-en
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run supertonic:3
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run chattts:default
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run pocket-tts:french_24l
```

## Multilingual conversations

Qwen3-TTS and ChatTTS use MLX Audio Whisper large-v3-turbo to recognize Chinese
and English in the same session. Each model has one entry:

```sh
boubou run qwen3-tts:0.6b-customvoice
boubou run qwen3-tts:1.7b-customvoice
boubou run chattts:default
```

Whisper detects the input language, and the language model replies in that
language. Qwen3-TTS uses automatic synthesis language and the `Vivian` voice;
ChatTTS samples a speaker at startup. Other profiles retain Parakeet TDT 0.6B v3.
A profile downloads only its selected recognizer and shares the same 4-bit
Qwen3.5-4B language model.

Whisper's MLX conversion and OpenAI processor files are pinned separately. The
runner combines them in temporary storage, so `--offline` needs no Hub request
and does not modify the verified model snapshots. This packaged implementation
requires Apple Silicon and macOS 15 or newer. Other Whisper implementations use
their own model formats and dependencies; this package does not switch STT
engines when a developer changes another component's backend.

The upstream framework supports `mlx-lm`, `transformers`, `responses-api` and
`chat-completions` LLM backends. Boubou's packaged profiles use `mlx-lm` and the
pinned 4B checkpoint. Smaller and larger checkpoints, and other inference
backends, require a separately configured developer runtime; they are not
additional downloads in the default package.
