# Boubou

Local English voice conversations on Apple Silicon Macs, powered by Liquid AI LFM3-Audio-2B. This is a development prototype, not a signed installer or a device-quality guarantee.

## Install

Requires Rust 1.88+ and Xcode Command Line Tools.

```sh
cargo install --path . --locked
boubou run liquid
```

First use downloads and verifies approximately 1.42 GB of model weights plus a local inference backend. Allow microphone access when macOS asks. Inference stays local; there is no cloud fallback.

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

Conversation history lasts for the session; interruptions cannot erase already generated model context. Audio is not saved by default. Real microphone, echo, interruption quality, and release readiness require separate device testing.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo build --locked
```

CI runs these checks on macOS, without downloading models or activating audio devices. Use [the Spec template](spec/TEMPLATE.md) for material behavior or interface changes; small fixes need no process overhead.

## Licenses

The model uses the [LFM Open License v1.0](https://huggingface.co/LiquidAI/LFM3-Audio-2B); the [pinned backend](https://github.com/ggml-org/llama.cpp/releases/tag/b11960) uses MIT. Model redistribution and commercial-use terms still apply.
