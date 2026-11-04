# Apple Silicon Qwen candidate

From a clean checkout on macOS 15 or newer, use Python 3.11 or newer to build the shared interpreter and resolve the pinned model manifests. Keep the output outside the checkout so the npm build can verify its source commit.

```sh
candidate="$(mktemp -d)"
python3 -B packaging/qwen/lock_models.py "$candidate/models.json"
python3 -B packaging/qwen/build_runner.py "$candidate"
cargo fetch --locked
python3 -B packaging/build-macos.py --qwen "$candidate"
```

The resulting npm tarball includes the interpreter, native wheels, Silero and NLTK data. The first `pull` fetches verified speech and language model weights. Both CustomVoice sizes reuse the same runner, speech recognizer and language model. To distribute a smaller npm package, pass `--url <public immutable HTTPS URL>` to the runner builder and host its exact `runner.tar.gz` at that URL.

Test the tarball from outside the checkout on an idle Mac. Use the same new cache for both commands. Grant microphone access and test multi-turn English conversation, audible interruption, a new turn, offline restart, and Ctrl+C for each model. Record the machine and measurements in [#29](https://github.com/entropyai-labs/boubou/issues/29).

```sh
export BOUBOU_HOME="$(mktemp -d)"
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run qwen3-tts:1.7b-customvoice
npx --yes --package /absolute/path/to/boubou-0.1.0-beta.3.tgz -- boubou run qwen3-tts:0.6b-customvoice
```
