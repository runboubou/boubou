# Boubou Liquid-first macOS Product Specification

**Status:** Product requirements and planned stages, not evidence that functionality or release acceptance is complete. Track progress in [roadmap #1](https://github.com/entropyai-labs/boubou/issues/1) and conversation/lifecycle work in [issue #2](https://github.com/entropyai-labs/boubou/issues/2). The original planning review and snapshot evidence remain in [PR #7](https://github.com/entropyai-labs/boubou/pull/7) and Git history.

## Product goal and boundaries

The release goal is a verified Mac distribution that starts local English voice conversations with `boubou run liquid`, without requiring users to install Rust, Python, Xcode Command Line Tools, or llama.cpp. Initial asset downloads and microphone permission are visible steps. With verified assets cached, `boubou run liquid --offline` must work without network access or cloud inference.

| Area | Contract |
| --- | --- |
| Platform | Apple Silicon macOS; supported OS and audio-device combinations must be established by binary requirements and real tests. |
| Model | One pinned Liquid LFM3-Audio-2B Q4_K_M model and matching Q8_0 audio projector profile initially. |
| Language | English voice chat. Upstream multilingual ASR/TTS does not establish supported multilingual conversations. |
| Interaction | Foreground CLI, one session, default audio devices. Manual and file modes remain explicit alternatives and diagnostic controls. |
| Local operation | No account requirement, cloud-inference fallback, or default telemetry upload. Audio, transcripts, and complete replies are not persisted by default. |

Linux/Windows, a full native app, a daemon, multiple users, arbitrary Hugging Face loading, tool execution, training, generic plugins, and a complete public Realtime API are outside this scope.

## Architecture and conversation boundaries

Extend the existing implementation:

- The Rust CLI owns profile selection, verified assets, turn boundaries, cancellation, diagnostics, and child-process lifecycle.
- The native Objective-C audio helper owns capture, echo processing, near-end speech detection, resampling, and bounded playback queues. Input is PCM16 at 16 kHz; output is PCM16 at 24 kHz.
- The owned llama.cpp process loads the pinned model and projector and serves text/audio through a loopback WebSocket. Its temporary `/v1/realtime` endpoint is internal, not a stable public interface.

Keep Liquid's audio-native model and matching components together. Do not introduce a separate STT/LLM/TTS cascade to adapt it or add abstractions for hypothetical backends.

Stopping playback and cancelling generation do not roll model history back to what the user heard. Do not claim precise history rollback or model-native full duplex. [Automatic context renewal](0003-automatic-context-renewal.md) and [repeated-reply recovery](0005-repeated-replies.md) clear previous context while keeping the model loaded. These mechanisms bound memory and permit continued use; they do not preserve recall or introduce persistent conversation history.

## Delivery stages

Complete reliable conversations before expanding the product. Keep one roadmap and one active implementation task; deferred issue closures do not mean features are complete.

| Stage | Delivery and acceptance |
| --- | --- |
| 1. Conversations and lifecycle ([#2](https://github.com/entropyai-labs/boubou/issues/2)) | Default conversation, human interruption, cancellation, cleanup, diagnostics, offline behavior, and privacy work within a tested device envelope. |
| 2. Model selection ([#4](https://github.com/entropyai-labs/boubou/issues/4)) | One repository-owned JSON catalog drives CLI selection and later website information, preserving `run liquid` and existing cache behavior. |
| 3. Distribution ([#5](https://github.com/entropyai-labs/boubou/issues/5)) | Verified, signed/notarized artifacts install and run on a clean supported Mac without development tools. Delivery follows stages 1 and 2; feasibility work may start earlier. |
| 4. Static Library website ([#6](https://github.com/entropyai-labs/boubou/issues/6)) | Supported profiles, requirements, evidence, and copied commands match the catalog and actual distribution. |

### Stage 1: conversation and lifecycle contract

- Silence, assistant playback, and its acoustic tail must not create user turns. Submit each completed utterance once and preserve its beginning.
- Human interruption stops current and queued playback, cancels the corresponding generation, and allows the next turn to finish. Cover interruption after generation has already completed with playback still queued.
- Pending cancellation must preserve separate utterances and never start an answer while the user is still speaking. Filter stale response events and playback acknowledgments by their response/epoch identities.
- Preserve Esc pause/Enter resume; paused audio is not submitted. Enforce bounded input and queues with explicit errors rather than silent truncation.
- Loading, protocol initialization, audio readiness, and pending microphone permission must be cancellable. Ctrl+C and failures clean up only this invocation's processes and audio resources within a documented deadline.
- Distinguish denied or pending permission, unsupported or changed devices, corrupt assets, and backend failures. Reject known unsupported platforms before downloads. Explicit restart guidance after a device change is acceptable; universal hot switching is not required.
- Automatic detection depends on supported HAL near-end speech capabilities. Do not silently disable echo processing or replace detection with volume thresholds while claiming verified hands-free support. Manual mode is not a substitute for automatic-mode acceptance.
- Preserve pinned revisions, size/SHA-256 verification, resumable `.part` downloads, installation locking, and verified publication. Missing or invalid offline assets fail explicitly; unverified assets remain unusable.
- Default `doctor` is offline and does not run inference, open audio devices, or download repairs. Report asset integrity, platform facts, and untested device capabilities separately. Complete assets alone do not certify conversation quality.
- Document and test exit codes and cleanup deadlines. The proposed codes are `0` for success, `1` for diagnostic/runtime failure, and `130` for cancellation; finalize them in the implementation task while preserving argument-parser errors.

### Later stages: catalog, distribution, and website

A profile is a complete compatible model/projector/backend configuration. The catalog must identify its schema and profile versions, aliases, minimum Boubou version, pinned files and hashes, backend platform/entry point, supported settings, tasks/languages, licenses, maturity, and measured device evidence. Unknown requirements remain unverified. Do not execute remote startup scripts or offer arbitrary component mixing.

Proposed `list` and `show` commands expose this information while `run`, `pull`, and `doctor` retain their responsibilities. Preserve `liquid` as a release-defined alias; changes to asset bytes, backend, or defaults require a new profile version and compatibility notes. Downloaded and verified are distinct states. Validate a second profile's files, settings, and conversations before advertising it as runnable.

Distribution must validate the exact executable chain: CLI, audio helper, backend, and dynamic libraries. Changed or re-signed bytes require matching artifacts and hashes. Ad-hoc signing, disabling Gatekeeper, or removing quarantine does not establish release readiness. Provide provenance, licenses, installation, upgrade, rollback, and uninstall instructions; distinguish application removal from cache deletion, which requires the user's choice. Preserve the current cache path unless a separate migration is designed, and explain that system cache removal can prevent offline startup.

The static website consumes the same catalog, filters supported platforms/tasks/languages, presents profiles and measured requirements, and copies valid commands. Keep parameter count, download size, installed size, and runtime memory distinct. Unsupported capabilities must not produce runnable commands; label previews and unmeasured values honestly. In-browser inference, accounts, billing, uploads, databases, telemetry, and browser-to-localhost control are outside this scope.

## Acceptance and evidence

Record the implementation commit, model/backend revisions, machine, OS, audio devices, exact steps, and results in the owning PR or issue. Distinguish ordinary automated tests, real-backend tests, human-device tests, and distribution acceptance. Mark unexecuted checks **NOT RUN**. A merged change, green CI, or synthesized WAV does not establish real microphone or release acceptance.

- **Software:** follow the [engineering checks](../AGENTS.md#engineering). Cover changed behavior and relevant failures, including offline assets, cancellation, stale events, turn boundaries, and playback epochs.
- **Real backend:** run explicitly authorized targeted tests with pinned inputs and parameters. Do not run all ignored tests indiscriminately; some activate audio devices. Protocol correctness and response quality are separate observations.
- **Human conversation:** with consent, test default automatic mode for a normal question, contextual follow-up, interruption during generation and queued playback, distinct consecutive questions, assistant-only playback, pause/resume, and exit. Verify actionable errors and cleanup for permission, device, and backend failures. Compare file/manual/automatic paths when isolating failures.
- **Sustained use:** a 15-20 minute multi-turn session on a supported device is a recommended sample for queues, memory trends, glitches, and shutdown, not a universal stability guarantee.
- **Clean Mac:** install the actual distribution on another supported Mac without development tools; authorize, download, converse, exit, disconnect, and restart offline. Build-machine caches do not satisfy this check.

Do not save recordings by default. Recording requires separate consent, a bounded destination, and a retention policy. Inspect default diagnostics for unintended conversation content.

Measure startup readiness, end of speech, turn submission, first generated audio, and playback separately; likewise distinguish interruption detection, cancellation, and actual playback stop. Native playback completion acknowledgments are not first-audible-sample measurements. Account for CLI, helper, and backend memory without double-counting shared pages. Publish latency percentiles or performance targets only with representative samples, failure counts, and an explicit product decision.

## Release decisions and references

Resolve the supported OS/device list, measured memory guidance, performance targets, signing/distribution method, Boubou source license, additional profiles, and website hosting in their owning stages. Review Boubou, backend, and model licenses separately before distribution; downloadable weights do not imply unrestricted commercial use. These decisions do not block focused stage 1 reliability work.

Upstream references: [Liquid model card](https://huggingface.co/LiquidAI/LFM3-Audio-2B), [GGUF documentation](https://huggingface.co/LiquidAI/LFM3-Audio-2B-GGUF), and [Apple notarization](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution). Actual pinned assets and backend versions are maintained in [the asset implementation](../src/assets.rs); this specification does not upgrade them or establish distribution clearance.
