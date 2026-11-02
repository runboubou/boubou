# Testing and evidence

Run the complete ordinary suite for each final candidate. The project is small
enough that an impact selector, test ownership registry, or cached approval
system would add unnecessary complexity. Local verification and CI share
`scripts/verify.py`.

## Ordinary verification

From the repository root, run:

```sh
python3 scripts/verify.py
# Use only when dependencies are already cached:
python3 scripts/verify.py --offline
```

Verification checks repository hygiene, then runs:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo build --locked
```

The offline option also passes `--offline` to the Cargo commands that resolve
dependencies. A missing cached dependency is a failure to prepare the checks,
not a passing test. Dependency preparation may access public package sources in
normal mode; no ordinary check should download model weights or run Boubou's
interactive entry point.

Python 3.9 or newer is required for repository checks. Native verification
requires an Apple Silicon Mac, Rust 1.88 or newer, `rustfmt`, `clippy`, and Xcode
Command Line Tools. CI covers macOS ARM64 on stable Rust and Rust 1.88, plus
portable regression checks on Linux stable. The workflow is the authority for
the exact runner and toolchain versions; inspect its current jobs when reporting
results. Linux success does not establish support for a Linux audio runtime.

The stable aggregate check is `quality`. It succeeds only when the repository
check and every runtime matrix job succeed; a failure, cancellation, or skipped
required job must not produce a passing aggregate. Use `quality` as the required
status check when GitHub repository settings permit enforcement, rather than
maintaining a separate required-check list for changing matrix job names.

## Own the risk in the affected module

Keep Rust tests beside the behavior they exercise; create shared fixtures only
for actual reuse. Use temporary directories, local mock servers, and owned test
processes. Do not use production caches, public services, or another project's
machines as fixtures.

| Area | Cases to retain or add when behavior changes |
| --- | --- |
| CLI and file input | Invalid option combinations, malformed WAV, input format/size limits, accurate exit status |
| Assets | Integrity and size failures, interrupted/resumed downloads, invalid range responses, offline operation, incomplete caches |
| Backend lifecycle | Startup failure, cancellation during readiness, bounded cleanup of the owned child, useful diagnostics |
| Realtime protocol | Malformed events/audio, startup cancellation, late responses, interruption ordering, next-turn behavior |
| Voice policy | Silence, frame boundaries, bounded preroll, separate turns during cancellation, pause/resume input handling |
| Native audio boundary | Packet validation, playback epochs, obsolete acknowledgments, bounded queues and helper cleanup |
| Verification infrastructure | Invalid repository inputs and failure propagation; a failing check must make verification fail |

Test observable outcomes and failure paths. Do not delete unique coverage or
change expected outcomes solely to get a green result. Use explicit
synchronization and bounded waits for process tests; fixed sleeps should not be
the only evidence that work completed. Any deliberate removal of coverage needs
its retained replacement or retirement rationale in the PR.

## Model and physical-device acceptance

Ordinary verification never passes `--ignored`. Ignored tests remain explicitly
unperformed. Select a specific ignored test only after reading its prerequisites
and obtaining permission for the actual model or device use; never run all
ignored tests as a shortcut.

Tests such as `liquid_backend_cancels_and_accepts_the_next_turn` use real cached
model assets even though they open no microphone or speakers.
`cancelled_playback_does_not_overflow_capture_events` builds
`native/audio_tests.m` and uses a real output device with silence.
`speaker_echo_probe` uses real speakers and microphone capture. These have
different effects and must not be treated as interchangeable CI fixtures.

For native/audio/model changes, use the acceptance scenarios in the
[product specification](../specs/0001-liquid-macos-product.md), including real
hands-free turns, human interruption, queued playback, permission handling,
pause/resume, and cleanup. Record the commit, model/backend versions, OS, hardware,
devices, scenario, result, and limitations. Recording human audio needs separate
consent and a bounded retention decision; do not publish raw captures or
conversation logs.

## Report evidence accurately

Record the tested commit or clearly identify an uncommitted candidate, command,
OS/toolchain, actual result, and ignored/skipped tests. Use `PASS`, `FAIL`, and
`NOT RUN` separately. After code changes, rerun verification for the new candidate.
Before merging, inspect hosted CI for the PR's current head and merge candidate.

A local pass is not hosted CI evidence. Compiling the native helper does not
prove microphone permission, echo handling, human barge-in, or audible quality.
Neither unit tests nor model tests prove signing/notarization, clean-machine
installation, memory recommendations, or release readiness. Those results remain
`NOT RUN` until their specific acceptance has been completed.
