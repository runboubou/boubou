# Boubou

Local voice conversations, one command away.

```sh
boubou run liquid
```

Boubou downloads pinned assets, listens through the microphone, and streams the
model's spoken replies locally. The current development prototype supports
**Apple Silicon Macs and English voice chat**, using Liquid AI LFM3-Audio-2B.
Natural interruption, acoustic quality, and long sessions still need target-device
acceptance. A signed/notarized installer has not been released.

## Install from source

Install Rust 1.88+ and the Xcode Command Line Tools, then run in this checkout:

```sh
cargo install --path . --locked
boubou run liquid
```

The installed executable needs no Python, Rust compiler, or separate llama.cpp
installation. First use downloads about 1.42 GB of model/projector weights plus
the inference backend. Allow microphone access for Boubou or its terminal when
macOS requests it. Startup can be cancelled with Ctrl+C.

## Commands

```sh
# Download and verify the fixed Liquid profile in advance.
boubou pull liquid

# Use verified cached assets only, without downloads or update checks.
boubou run liquid --offline

# Verify cached assets locally; missing or invalid assets return a failure.
boubou doctor

# Override the cache directory, also configurable with BOUBOU_HOME.
boubou --cache-dir /path/to/cache run liquid
```

The default macOS cache is `~/Library/Caches/boubou`. Interrupted downloads
resume on the next attempt. Exact size and SHA-256 verification are required
before assets become usable. Concurrent preparations lock the installation
cache. There is no automatic cloud fallback or profile update.

## Conversation controls

- Speak English and pause for about 600 ms to submit a turn.
- Speak during a reply to clear local playback, cancel generation, and supply
  the next utterance. Separate turns remain separate while cancellation drains.
- Esc clears queued input and playback, cancels the reply, and pauses input
  submission. Capture remains open, but its frames are discarded until Enter.
- Enter resumes. Ctrl+C exits with status 130 and cleans up owned children.
- Spoken “stop” or “goodbye” is ordinary model input, not an explicit pause control.
- Individual inputs and the pending playback queue are limited to 30 seconds.
  Device failure, queue overflow, or a stalled backend produces an error.

The model's voice-chat mode currently supports English. Multilingual ASR/TTS
capability does not establish multilingual conversational support. Speaker echo
handling uses macOS VoiceProcessingIO and HAL near-end voice detection; hardware
behavior still needs coordinated verification.

For a manual comparison with record-then-submit examples:

```sh
boubou run liquid --offline --manual
```

Enter starts or submits recording. During a reply it interrupts and starts a new
recording. Idle and playback audio are not automatically submitted in this mode.
Manual/file-mode success does not establish hands-free interruption quality.

## Context and interrupted history

The profile uses the model's 32,768-token context. The pinned realtime backend
does not roll conversation history and can abort when it is full. Boubou
identifies this known failure from the backend log and explains that a new
conversation is needed. Increasing the former 8,192 limit does not provide
unlimited history.

The backend can cancel generation but cannot truncate history at the timestamp
actually heard. Boubou clears playback, filters obsolete response output and
acknowledgments, and adds an interruption notice before the next input.
Generated content remains in model context. Precise played-history rollback and
model-native full duplex are not implemented.

Conversation state lasts only for the running inference session. Microphone
audio is not saved by default. Inference connects only to the owned loopback
backend. Backend diagnostics are kept under the cache's `logs` directory.

## File mode

Input must be PCM16 mono WAV at 16 kHz, at most 30 seconds per file. Repeat
`--input` for multiple turns in one conversation. File mode opens no microphone
or speakers and can save generated replies as PCM16 mono 24 kHz audio:

```sh
boubou run liquid --offline \
  --input question.wav --input follow-up.wav --output replies.wav
```

## Diagnostics

```sh
BOUBOU_TRACE=1 boubou run liquid --offline
```

Opt-in terminal traces contain relative times, input turn numbers, response IDs,
playback epochs, first-audio events, per-response sample totals, pause/resume,
and cancellation decisions. Trace records contain no PCM, transcripts, or
generated text and create no trace file. Normal conversation text stays visible.
`playback.clear.applied` acknowledges the native player's clear operation, not
the remaining hardware/acoustic tail.

## Implementation and development

| Component | Implementation |
| --- | --- |
| CLI, downloads, session and process ownership | Rust |
| Audio input/output and echo processing | Embedded Objective-C AVAudioEngine / VoiceProcessingIO helper |
| Voice detection | Echo-aware macOS HAL indication, 20 ms frames and onset preroll |
| Inference | llama.cpp `b11960`, streaming `/v1/realtime` |
| Model | `LiquidAI/LFM3-Audio-2B-GGUF`, revision `02355c1a141e464a064f2ab2c276638dbcb7f38a` |
| Weights | `Q4_K_M` model and `Q8_0` audio projector |

The helper uses local ad-hoc signing. Rebuilding it may cause macOS to request
microphone access again. Other operating systems and chips are not supported
by this profile.

## Contributing and code quality

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a PR. Repository-facing
code, documentation, Issues, and reviews are written in English. Material changes
use the [Spec template](docs/specs/TEMPLATE.md) and the
[review standard](docs/development/review.md).

Run the same verification entry point used by CI:

```sh
python3 scripts/verify.py
# When Cargo dependencies are already cached:
python3 scripts/verify.py --offline
```

The checks cover repository hygiene, formatting, Clippy, the complete ordinary
Rust test suite, compilation, and a compile-only native test harness on macOS.
CI checks macOS ARM64 with stable Rust and the declared Rust 1.88 minimum, plus
portable Rust regressions on Linux. A Linux check does not establish Linux voice
support. See [Testing](docs/development/testing.md) for commands and evidence limits.

Ordinary tests do not download models or open audio devices. Cached-backend and
physical audio checks are explicitly ignored and run separately when needed.
CI success does not establish real conversation quality or release readiness.
Report vulnerabilities through [SECURITY.md](SECURITY.md).

## Upstream and licenses

The model uses the custom LFM Open License v1.0 with commercial-use and
redistribution conditions. Its license is verified alongside the weights.
llama.cpp uses MIT; the verified backend archive retains its upstream license.

- [Liquid model card and license](https://huggingface.co/LiquidAI/LFM3-Audio-2B)
- [Pinned model assets](https://huggingface.co/LiquidAI/LFM3-Audio-2B-GGUF/tree/02355c1a141e464a064f2ab2c276638dbcb7f38a)
- [Pinned inference backend](https://github.com/ggml-org/llama.cpp/releases/tag/b11960)
- [Apple voice processing and voice detection](https://developer.apple.com/videos/play/wwdc2023/10235/)

Additional models and Hugging Face/LiveKit cascades remain separate future work.
