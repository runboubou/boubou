# Boubou

Local voice conversations on Apple Silicon Macs.

## Run

Requires macOS 13.3+, Apple Silicon, Node.js 18+ and npm:

```sh
npx --yes --cache "$HOME/Library/Caches/boubou/npm" boubou@beta run LFM3-Audio-2B
```

The command keeps npm downloads in Boubou's user cache; no global installation,
npm login or administrator access is needed.

Selecting `LFM3-Audio-2B` downloads about 1.42 GB of verified assets on first
use and asks for microphone access. Inference stays local. Speak to chat or interrupt; Esc pauses, Enter
resumes and Ctrl+C exits. Use `--offline` for cached assets only.

For the guided start, use a source build or a package built from this checkout:

```sh
boubou
```

Choose English or Chinese, review the detected Mac and choose a complete runtime,
then confirm its components, estimated resource needs and download. English
choices grow across the 16, 32, 64 and 128 GiB memory groups, including Liquid
and [fixed STT/LLM/TTS combinations](packaging/qwen/runtimes.json). Higher groups
retain lighter choices. Liquid is the baseline; modular combinations are labeled
as trials pending listening and interaction acceptance. Memory capacity controls
eligibility, not quality ranking. The list filters by current available memory,
macOS and cache disk space.
Chinese uses Vivian, also understands English, and requires macOS 15+
and a [package with the shared voice runner](packaging/qwen/README.md).
Builds without that runner offer English and explain why Chinese is unavailable.
The menu uses arrow keys and Enter; Esc goes back and Ctrl+C exits.
It checks architecture, macOS, total and available memory, and cache disk space
before downloading and rechecks resources when starting the selected runtime.
Memory groups and runtime budgets are estimates, not measured hardware minima.
Smaller Macs have not been hardware-validated, and larger models can take longer
to answer. If startup fails,
follow the error's recovery steps and reopen `boubou` to check again and resume
downloads. Set `NO_COLOR=1` for uncolored menus. Redirected I/O and `TERM=dumb`
print help. The developer entry accepts a model name or run command.
Source builds also support `boubou run LFM3-Omni-2B` for English voice chat
(about 1.62 GB on first use). Ask a local image or short video a text question:

```sh
boubou ask LFM3-Omni-2B photo.png "What is in this picture?"
boubou ask LFM3-Omni-2B clip.mp4 "What happens, and what can you hear?" --offline
```

Images: PNG/JPEG, up to 2048 × 2048 pixels. Videos: MP4/MOV with constant frame
rate, up to 10 seconds, 512 × 512 pixels and 60 fps; install `ffmpeg` and `ffprobe`
for video. Files are limited to 64 MiB. This mode returns text and does not open
the microphone. See [media limits and errors](spec/0009-omni-media-questions.md).
These source and candidate-package capabilities do not imply an npm release.

Models and runtime downloads are kept in `~/Library/Caches/boubou`.
Use `list` to see supported model IDs and `doctor [MODEL]` to check cached readiness.
Without a model ID, `doctor` checks `LFM3-Audio-2B`.

The [first MVP](spec/0001-liquid-macos-product.md) is accepted.
Signing and notarization remain deferred.

## Develop

Requires Rust 1.88+, Xcode Command Line Tools, Python 3.9+ and Node.js/npm.
Build from a clean commit:

```sh
cargo build --locked
python3 packaging/build-macos.py
```

The npm tarball, checksum and provenance are under `target/dist/<commit>/`.
See [checks](.github/workflows/quality.yml) and [contribution rules](AGENTS.md).
Model: [LFM Open License](https://www.liquid.ai/lfm-license).
Backend: [llama.cpp MIT](https://github.com/ggml-org/llama.cpp/blob/b11960/LICENSE).
Full upstream license texts are verified during packaging and included in the package.
