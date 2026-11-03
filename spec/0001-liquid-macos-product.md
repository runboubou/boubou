# Liquid 2B MVP

**Outcome:** A user installs the native npm package and runs
`bobo run LFM3-Audio-2B` for local English voice chat without a compiler.

## Contract

- Apple Silicon macOS, one fixed Liquid 2B configuration and default voice. Reuse the Rust CLI, native audio helper and owned llama.cpp backend on loopback.
- Download pinned assets with size/SHA-256 checks, resumable transfers and one preparation lock. Reject unsupported hosts before downloads. Never execute unverified bytes.
- Submit each utterance once; ignore silence and playback echo. Interrupt current or queued playback, cancel generation and reject stale response audio/ACKs. Preserve separate utterances while cancellation is pending.
- Keep input/playback bounded to 30 seconds and queues bounded; report limits rather than silently losing speech. `Esc` pauses submission, `Enter` resumes, `Ctrl+C` exits.
- Keep startup and permission waits cancellable. Reap only owned processes and release audio resources. Device changes may require a clear error and restart.
- `doctor` checks cached readiness offline without recording, inference or downloads. Offline startup fails clearly for missing or corrupt assets.
- Inference stays local. Save no audio, transcript or model replies by default. Interruption stops playback without rolling model history back.
- Long sessions clear history at safe input boundaries: [context capacity](0003-automatic-context-renewal.md) and [repeated replies](0005-repeated-replies.md).

## Verification

Use [repository checks](../.github/workflows/quality.yml) for software regressions.
Real-model and microphone checks separately cover multi-turn chat, interruption,
playback-only silence, pause/resume and owned-process cleanup. Record tested
versions/devices and distinguish measured results from unexecuted checks.

Runtime acceptance is recorded in [#2](https://github.com/entropyai-labs/boubou/issues/2).
The [development package contract](0006-liquid-development-package.md) covers delivery.
Signing, notarization and clean-Mac release qualification remain deferred in
[#5](https://github.com/entropyai-labs/boubou/issues/5).

Model/configuration selection, additional voices and all website work are outside this MVP.
