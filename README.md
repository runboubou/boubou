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

Boubou checks this Mac and opens an English voice menu. With the shared runner
packaged, it prefers Qwen's fixed Aiden voice with Qwen3.5-4B when resources
permit, offers Meta MMS English as another voice, and keeps Liquid's integrated
voice for lighter devices. Other models and LLM sizes remain available through
`list`, direct commands and the developer entry. See [runtime packaging](packaging/qwen/README.md).

The menu checks architecture, macOS, total and available memory, and cache disk
space before downloading. Review the estimated download and memory needs before
starting. These estimates do not establish hardware minima; smaller Macs still
need testing. Use arrow keys and Enter, Esc to go back or exit, and Ctrl+C to
quit. Set `NO_COLOR=1` for uncolored menus. Redirected I/O and `TERM=dumb` print
help. A failed download can be resumed by starting again.

Source builds also support `boubou run LFM3-Omni-2B` for English voice chat
(about 1.62 GB on first use). Ask a local image or short video a text question:

```sh
boubou ask LFM3-Omni-2B photo.png "What is in this picture?"
boubou ask LFM3-Omni-2B clip.mp4 "What happens, and what can you hear?" --offline
```

Images: PNG/JPEG, up to 2048 × 2048 pixels. Videos: MP4/MOV with constant frame
rate, up to 10 seconds, 512 × 512 pixels and 60 fps; install `ffmpeg` and `ffprobe`
for video. Files are limited to 64 MiB. This mode returns text and does not open
the microphone. Invalid or oversized media is rejected before inference.
These source and candidate-package capabilities do not imply an npm release.

Models and runtime downloads are kept in `~/Library/Caches/boubou`.
Use `list` to see supported model IDs and `doctor [MODEL]` to check cached readiness.
Without a model ID, `doctor` checks `LFM3-Audio-2B`.

## Develop

Requires Rust 1.88+, Xcode Command Line Tools, Python 3.9+ and Node.js/npm.
Build from a clean commit:

```sh
cargo build --locked
python3 packaging/build-macos.py
```

The npm tarball, checksum and provenance are under `target/dist/<commit>/`.
See [checks](.github/workflows/quality.yml) and [contribution rules](AGENTS.md).
Boubou code: [MIT](LICENSE).
Liquid models: [LFM Open License](https://www.liquid.ai/lfm-license).
Backend: [llama.cpp MIT](https://github.com/ggml-org/llama.cpp/blob/b11960/LICENSE).
Full upstream license texts are verified during packaging and included in the package.
Meta MMS weights use [CC BY-NC 4.0](https://huggingface.co/facebook/mms-tts-eng)
and are restricted to non-commercial use. Each model retains its own terms.
Signing, notarization and public distribution qualification remain tracked in
[#5](https://github.com/entropyai-labs/boubou/issues/5).
