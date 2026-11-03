# Boubou

Local English voice conversations on Apple Silicon Macs, powered by Liquid AI LFM3-Audio-2B. This is a development prototype, not a signed installer or a device-quality guarantee.

## Development package

Obtain the precompiled macOS arm64 development archive from the maintainer and extract it. In Terminal, from the extracted directory, run:

```sh
sh ./install.sh --run
```

This verifies the package, installs Boubou into `~/.local/bin` and starts the existing Liquid 2B profile. End users do not compile code or install Rust, Python, Xcode Command Line Tools, or a separate inference backend. First use downloads and verifies approximately 1.42 GB of pinned assets; allow microphone access when macOS asks. Inference stays local.

Later, run `"$HOME/.local/bin/boubou" run liquid` or add that directory to PATH to use `boubou run liquid`. `sh ./uninstall.sh` removes only the CLI and keeps model caches and saved audio. See [package instructions](packaging/README.txt) for offline use, installation directories, upgrades and removal.

These are development candidates. Developer ID signing, notarization, browser-download Gatekeeper/TCC behavior and another-Mac installation have not been qualified. No public download/bootstrap URL is advertised before a distribution entry point exists. Model/voice selection and a website are outside this MVP.

## Commands

```sh
boubou pull liquid                     # Prepare and verify assets
boubou run liquid --offline            # Use cached assets only
boubou doctor                          # Check cached assets
boubou run liquid --offline --manual   # Record/submit turns manually
boubou --cache-dir /path/to/cache run liquid
```

The default cache is `~/Library/Caches/boubou`; `BOUBOU_HOME` also overrides it. Downloads resume and are checked for size and SHA-256.

## Controls

Speak English; a short pause submits a turn. Speaking during a reply interrupts playback. `Esc` pauses input submission, `Enter` resumes it, and `Ctrl+C` exits. Each input and queued playback segment is limited to 30 seconds.

File mode accepts 16 kHz, mono, PCM16 WAV and does not open audio devices:

```sh
boubou run liquid --offline --input question.wav --output reply.wav
```

The active context window is 32,768 tokens. Before it fills, Boubou automatically clears the conversation history and opens a fresh session while keeping the model loaded. A short terminal notice marks the change. Capture, pending turns, and keyboard controls continue during renewal; you may need to restate facts from before the clear. This allows continued conversation with bounded memory, not unlimited recall. See [automatic context renewal](spec/0003-automatic-context-renewal.md). If Liquid gives the same sentence four times in a row, Boubou clears the context the same way, because the model does not recover from such a loop. See [repeated replies](spec/0005-repeated-replies.md).

Interruptions cannot erase already generated model context. Audio is not saved by default. Real microphone, echo, interruption quality, and release readiness require separate device testing.

## Development

Building requires Rust 1.88+, Python 3.9+ for package assembly, and Xcode Command Line Tools. From a clean committed tree:

```sh
python3 packaging/build-macos.py        # Precompiled archive, checksums and notices in target/dist
cargo install --path . --locked         # Optional developer installation

cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo build --locked
```

CI runs these checks on macOS, without downloading models or activating audio devices. Use [the Spec template](spec/TEMPLATE.md) for material behavior or interface changes; small fixes need no process overhead.

## Licenses

The model uses the [LFM Open License v1.0](https://huggingface.co/LiquidAI/LFM3-Audio-2B); the [pinned backend](https://github.com/ggml-org/llama.cpp/releases/tag/b11960) uses MIT. Model redistribution and commercial-use terms still apply.
