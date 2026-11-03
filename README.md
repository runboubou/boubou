# Boubou

Local English voice chat on Apple Silicon Macs with Liquid AI LFM3-Audio-2B.

## Run

Requires macOS 13.3+, Apple Silicon, Node.js 18+ and npm:

```sh
npx --yes --cache "$HOME/Library/Caches/boubou/npm" boubou@beta run LFM3-Audio-2B
```

The command keeps npm downloads in Boubou's user cache; no global installation,
npm login or administrator access is needed.

First use downloads about 1.42 GB of verified assets and asks for microphone
access. Inference stays local. Speak to chat or interrupt; Esc pauses, Enter
resumes and Ctrl+C exits. Use `--offline` for cached assets only.

Models and runtime downloads are kept in `~/Library/Caches/boubou`.
Append `--help` for options.

Development candidate: signing, notarization and installation on another Mac
remain unverified.

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
