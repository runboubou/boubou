# Boubou Liquid-first macOS Product Specification

## Classification

- Type: productization / local speech runtime.
- Spec name: **Boubou Liquid-first macOS Product Specification**.
- Status: **Draft — implementation planning, not completed functionality or release approval.**
- Priority: P1. Unvalidated hands-free conversations and barge-in block the initial release; this is not a production incident.
- Owner: to be assigned by repository maintainers. No contributor is assigned by this document.
- Target: Apple Silicon Mac, Liquid LFM3-Audio-2B, English voice conversations.
- Original evidence date: 2026-11-02.
- Change scope: planning documentation and issues only. Do not modify or publish the uncommitted local implementation, merge code, or publish an installer.

> **Extend the existing Rust + native macOS audio + llama.cpp implementation. First make real conversations reliable, then add model selection and no-toolchain installation, and finally build the website. Do not rebuild the project in Go/Python or force Liquid into an STT -> LLM -> TTS cascade.**

### Current tracking

Keep **one roadmap and one active implementation task**: [roadmap #1](https://github.com/entropyai-labs/boubou/issues/1) and [Step 1 in #2](https://github.com/entropyai-labs/boubou/issues/2). The former lifecycle issue #3 is consolidated into #2. Issues #4-#6 are deferred references for model selection, distribution, and the website; closing them does not mean their functionality is complete.

The sequence is **Step 1: reliable conversations and lifecycle -> Step 2: model selection -> Step 3: installer -> Step 4: Library website**. Sections 6 and 7 are two parts of Step 1, not separate active issues. Repository-facing planning documentation, issues, and PR descriptions are written in English.

## Why

### 1. This is not an empty project

At the original review, remote `main` was `a9120242d55e954349791897f66c39e03524363a` and contained only the initial README. The local worktree already contained approximately 2,950 lines of Rust, Objective-C, and documentation, but that implementation was uncommitted. File locations below refer to the **local snapshot identified by the appendix fingerprints**, not files claimed to exist in that Git commit.

| Component | Evidence in the reviewed snapshot | Planning conclusion |
| --- | --- | --- |
| CLI | `src/main.rs:25-129`; `run liquid`, `pull liquid`, `doctor`, offline/manual modes, file input | A prototype already exists; do not plan it from scratch |
| Downloads/cache | `src/assets.rs:13-103,160-315`; fixed revision, size/SHA-256, resumable downloads, installation lock, verified publication | Reuse this implementation rather than building another download platform |
| Inference process | `src/backend.rs:19-88`; owned llama.cpp process, loopback, health checks, cleanup | Preserve the boundary; improve startup cancellation and failure handling |
| Model protocol | `src/realtime.rs:45-301`; 16 kHz input, 24 kHz output, streaming, cancellation, response-ID filtering | Implemented internally; not a complete public Realtime API product |
| Audio devices | `src/audio.rs`, `native/audio.m`; VoiceProcessingIO, resampling, playback epochs, bounded queues | Retain the path; device compatibility still needs real tests |
| Automatic turns | `src/voice.rs:55-112`; 20 ms frames, near-end speech indication, 200 ms preroll, 600 ms silence endpoint | A policy exists; integrated hands-free and human-interruption acceptance remains open |
| Distribution | `build.rs:20-65`; native-helper build and ad-hoc signing | Not a signed, notarized public installer |
| Catalog/website | Only `liquid` is accepted; assets/descriptions are hard-coded; no Library | Necessary later work, not a reason to build the website first |

The original planning task ran `cargo test --locked --offline`: **19 passed, 0 failed, 4 ignored**. The ignored tests require a real model or audio devices and were not run. No microphone/speaker was opened and no models were downloaded. Source fingerprints matched immediately before and after this test run.

**Evidence boundary:** later concurrent edits affected Cargo files, README, and audio/main/realtime code, including Esc pause/Enter resume and development-only VAD comparisons. The earlier test result does not validate those later changes. Reconcile the latest source before implementation; the language correction does not constitute a new code review or test run.

### 2. Three facts that determine the implementation direction

1. **Liquid is an audio-native model, not a four-stage application pipeline.** The official description identifies an LFM3 backbone, causal audio encoder, and LiquidCodec. Select the model and matching audio components together; the projector is not an interchangeable standalone TTS backend. [S1][S2]
2. **Ship the initial chat capability as English.** The model card distinguishes English voice chat from multilingual ASR/TTS. Chinese recognition/synthesis support does not establish supported Chinese voice conversations in this product. [S1]
3. **Stopping playback is not rolling back model history.** The reviewed implementation flushes playback, cancels generation, filters late events, and inserts an interruption notice before the next turn. It does not truncate model history to the audio the user actually heard. Preserve this limitation; do not claim model-native full duplex or precise history rollback. See `src/realtime.rs:108-117,146-239` in the reviewed snapshot.

## Outcome

### 3. User outcome

After installing a verified Mac distribution, the user does not configure Python, Rust, a compiler, or llama.cpp. They run the existing command:

```sh
boubou run liquid
```

Boubou checks compatibility/resources, downloads missing pinned assets, requests microphone access, loads the local model, and starts an English conversation. The user can stop a reply, interrupt, continue the next turn, and exit. With all assets cached:

```sh
boubou run liquid --offline
```

All inference remains local. Initial downloads and microphone authorization are visible steps, not hidden behind a claim that neither is required.

The initial product requires **working real conversations, installation on another clean Mac, and catalog information matching runnable capabilities**. A successful developer build or generated WAV alone does not satisfy that outcome.

### 4. Initial scope

| Area | Contract |
| --- | --- |
| Platform | Apple Silicon macOS; derive the minimum OS/device envelope from actual binaries, HAL capabilities, and tests |
| Model | Start with one pinned Liquid Q4_K_M model plus Q8_0 audio projector profile |
| Language | English voice chat; documentation/interface language is separate from model language support |
| Interaction | Foreground CLI, one session, default audio devices; retain manual mode as a diagnostic and explicitly selected alternative |
| Local operation | No cloud-inference fallback, default telemetry upload, or account requirement |
| Settings | Expose understandable profile selection and supported settings, not every backend flag |
| Website | A static model Library for selection and command copying, not in-browser model execution |
| Non-goals | Linux/Windows, full native app, persistent daemon, multiple users, arbitrary HF repository loading, tool execution, training/fine-tuning, generic plugins, complete public Realtime API |

## Scope

### 5. Preserve the existing technical boundaries

```text
Boubou CLI (Rust)
  |-- Profile resolution, asset verification, errors, process lifecycle
  |-- Native audio helper (Objective-C / AVAudioEngine)
  |     Microphone -> echo processing / near-end speech -> PCM16 16 kHz
  |     PCM16 24 kHz -> playback queue -> speakers
  `-- Owned llama.cpp child process (loopback WebSocket)
        Pinned Liquid GGUF + matching audio projector -> text/audio streams
```

Boubou owns turn boundaries, playback cancellation, and device handling. The pinned backend owns model architecture, tokenizer/codec, and inference. Do not add separate application-level ASR/TTS to adapt Liquid, or make a future Hugging Face/LiveKit cascade a prerequisite.

The internal `/v1/realtime` endpoint connects Boubou to its owned backend. Do not present its temporary port as a stable public API or add public-interface binding in this stage.

### 6. Step 1a: validate hands-free conversations and barge-in

**Real entry point:** `boubou run liquid --offline`. Manual and file modes are diagnostic controls, not substitutes for default-path acceptance.

Preserve or demonstrate the following:

- Silence does not submit empty turns; assistant playback and its acoustic tail do not become false user speech.
- Submit each completed utterance once. Preserve its onset rather than dropping words during buffering or cancellation waits.
- Human barge-in stops current and queued playback, then cancels the corresponding generation. Late audio from the old response cannot resume playback.
- While cancellation is pending, retain distinct user-turn boundaries. Do not merge two completed questions or answer an earlier question while the user is still speaking.
- Interruption flushes buffered playback even if model generation has already finished. Test this separately from cancellation during generation.
- Filter native playback ACKs by epoch. Preserve the fix preventing obsolete ACKs from overflowing the capture-event queue.
- Preserve the latest documented Esc pause/Enter resume behavior when reconciling code; paused audio is not submitted. `Ctrl+C` exits and reclaims owned processes/audio resources. The older reviewed snapshot only stopped a reply on Esc; it is not authority to undo later pause/resume work.
- Reaching existing input/queue limits produces an explicit error, not silent truncation presented as a successful conversation.

Initially support only **tested device combinations**. Automatic detection currently depends on HAL near-end speech support. Explain unsupported devices and optionally suggest explicit manual mode. Do not silently disable echo processing or substitute volume thresholds while claiming verified automatic interruption.

An explicit exit with restart guidance after a device change is acceptable initially; universal headset hot switching is not required. Before changing automatic detection, compare the same permitted test input through file, manual, and automatic paths to isolate model, endpointing, echo, or playback problems.

### 7. Step 1b: complete startup, shutdown, and diagnostics

These are **static findings from the original snapshot**, not individually reproduced production failures. Recheck them against later code before implementing changes.

| Gap | Historical location | Required behavior |
| --- | --- | --- |
| Health waiting lacks the stop flag | `src/backend.rs:19-79` | Stop waiting and clean up the owned child when loading is cancelled |
| Protocol initialization lacks global cancellation | `src/realtime.rs:45-77,262-277` | Allow cancellation during handshake/initialization |
| Blocking audio-ready read; permission may stay pending | `src/audio.rs:98-107`, `native/audio.m:422-433` | Permit cancellation without requiring the user to first answer the permission prompt |
| Doctor returns success with missing files | `src/assets.rs:105-129` | Return accurate readiness and actionable exit status |
| OS/architecture checks do not establish the support envelope | `src/assets.rs:55-59`, `build.rs` | Reject known unsupported platforms before downloads; explain the basis for disk/memory guidance |
| Raw backend output is persisted | `src/backend.rs:23-49` | Check for conversation content; default diagnostics must not persist audio, transcripts, or full replies |

Extend existing `run`, `Backend`, `Realtime`, and `Audio` lifecycles rather than introducing a daemon or scheduler. Preserve bounded queues, timeouts, and owned-child semantics. Never clean up by killing every `llama-server` process.

Proposed observable contracts:

- Default doctor is offline and performs no recording, inference, or repair downloads.
- Report platform support, asset integrity, memory/disk facts, and unperformed device checks separately. Complete assets do not certify human-conversation quality.
- Suggested doctor codes: `0` for all checks within its responsibility passed; `1` for unsupported or unmet asset/runtime conditions. Preserve argument-parser errors.
- Suggested user-cancellation code: `130`; normal completion `0`; runtime failure `1`. Finalize, document, and test this new CLI contract in implementation.
- Pending permission is not permission denial. Do not authorize access for the user; cancellation must leave no helper/model process running.
- Cancelled downloads retain resumable `.part` files. Unverified assets remain unusable.
- Show preparation, download, verification, loading, permission waiting, ready, cancellation, and failure from real state. Do not build a separate state framework just to display those messages.

### 8. Step 2: turn the pinned configuration into a model catalog

#### 8.1 Users select a complete runtime profile

A Liquid profile includes model, projector, backend version, task/language coverage, and defaults. Do not offer independently interchangeable STT/LLM/TTS selectors. Introduce a cascade-specific profile only when a real cascade engine is implemented.

Use **one repository-owned JSON catalog shipped with the CLI** as input to the CLI and website. Do not build an online registry or execute shell commands from remote profile data.

Proposed commands:

```sh
boubou list                      # Known profiles and local cache state
boubou show liquid               # Components, size, languages, limits, evidence
boubou pull liquid               # Preserve existing behavior
boubou run liquid                # Preserve the default entry point
boubou run liquid:q4km-v1         # Concrete reproducible profile, once implemented
```

`liquid` is the default alias in a given release's catalog. `liquid:q4km-v1` identifies a pinned model/backend combination. Do not resolve mutable main/latest references to silently replace it. Changes to bytes, defaults, or backend require a new profile version and compatibility notes.

#### 8.2 Minimum catalog fields

| Group | Required information |
| --- | --- |
| Identity | Schema version, profile ID, display name, default aliases, model family, minimum Boubou version |
| Model assets | Repository/revision; required file roles, names, URLs, bytes, SHA-256; projector and license included |
| Backend | Engine, pinned version, OS/architecture, archive/size/hash, executable entry point; no arbitrary startup script |
| Settings | Context size, output cap, and sampling settings actually consumed by the current backend |
| Product capabilities | Implemented tasks, task-specific languages, interruption status, history-truncation limits, offline behavior |
| Support envelope | Confirmed OS/device constraints; unknowns remain null/unverified, not estimates presented as measurements |
| Hardware evidence | Reference machine, total RAM, whole-runtime peak usage, tested versions/configuration, date, result; no recommendation badge without evidence |
| Provenance/rights | Upstream card, license name/link, pre-distribution review status, known restrictions |
| Maturity | Experimental versus verified, tied to acceptance records; download completion never implies verified |

`list` need not hash every large file on every call. It may report downloaded/pending verification. Doctor and run retain integrity-check responsibility. Directory presence alone must not imply runnable status.

First migrate the existing Liquid profile without behavior changes, preserving cache paths and download tests. Validate a second quantization's files, parameters, and conversations before listing it as runnable. A Library with one genuine profile is preferable to unverified integrations added for appearance.

#### 8.3 Language and parameter display

- List English for this profile's voice-chat task. Do not expose an unsupported `--language zh` flag.
- Multilingual ASR/TTS may be labeled upstream capabilities not yet exposed by Boubou, without generating run commands.
- Bind sample rates, projector, and backend compatibility to the profile; do not allow arbitrary mixing.
- Additional voices, prompts, and context settings require actual backend support and tests before exposure.
- Keep 2B parameter count, download size, installed disk usage, and peak runtime memory distinct. Quantized file size is not a minimum-RAM requirement.

The reviewed source pins model size 1,099,626,016 bytes, projector 303,811,840 bytes, license 10,574 bytes, and runtime archive 16,465,074 bytes: **1,419,913,504 bytes total, approximately 1.42 GB in decimal units**. This is a sum of source constants, not an installed-size or RAM measurement. Derive the website's total from the same catalog.

### 9. Step 3: distribute a Mac installer without development-tool requirements

This is already a native Rust runtime; **there is no Python environment to package**.

Produce a reproducible macOS arm64 artifact, then validate signing, notarization, and clean-machine installation. Developers may use Rust/clang; end users should not run cargo, install Xcode Command Line Tools, or compile llama.cpp.

Validate the complete executable chain: Boubou, its embedded/extracted audio helper, the downloaded backend, and dynamic libraries. Signing only the outer CLI does not establish the entire chain's behavior. Test the exact bytes users download, install, and execute.

If re-signing the backend changes bytes, provide the corresponding new archive, hash, and profile version. Do not claim modified files match the pinned upstream hash. Missing signing/notarization credentials block public distribution; do not disable Gatekeeper, require quarantine removal, or call ad-hoc signing a production release. [S3]

Deliver an installation entry point, version, integrity/provenance information, licenses/notices, and upgrade/uninstall instructions. Reinstalling an explicit earlier version is sufficient for initial rollback; an automatic updater daemon is unnecessary. Distinguish application removal from cache removal and require the user's choice before deleting data.

The prototype uses `~/Library/Caches/boubou`. Document that cached data may be removed by the system and offline startup must fail clearly when assets are missing. Do not introduce an incidental path migration or duplicate caches. A persistent-model directory requires a separate migration contract.

### 10. Step 4: build the static Library last

The website has four jobs: filter supported platforms/tasks/languages, show profile details, explain device requirements, and copy runnable commands.

- Consume the same JSON catalog from Step 2; do not maintain separate handwritten size/language tables.
- Show model/quantization, total download, measured memory or unmeasured status, platform, chat languages, maturity, license, and minimum CLI version.
- Let visitors select a reference machine; accurate local hardware checks belong in the CLI.
- Experimental, incompatible, or unimplemented tasks must not look one-command ready. Explain limitations without generating misleading commands.
- Validate profile IDs, flags, and installation links against the actual CLI/distribution. Copy text only; do not execute commands on visitors' machines.
- Before a distribution exists, label the site as a development preview. Do not invent a `curl | sh` installation URL.
- Do not add accounts, billing, model uploads, a database, telemetry, or automatic browser-to-localhost control.

Choose a simple static-site stack. A full app or API service is not a prerequisite.

### 11. Where implementation belongs

| Work | Existing owner to extend | Add a boundary only when needed |
| --- | --- | --- |
| Conversation/barge-in | `main.rs`, `voice.rs`, `realtime.rs`, `audio.rs`, `native/audio.m` | Focused regressions, not a conversation-framework rewrite |
| Startup/diagnostics | `main.rs`, `backend.rs`, `realtime.rs`, `audio.rs`, `assets.rs` | Propagate cancellation through current ownership, not global process scanning |
| Catalog | Asset constants, CLI arguments/descriptions, actual backend settings | One catalog; a parser/resolver module only to remove demonstrated duplication |
| Distribution | `build.rs`, `native/Info.plist`, Cargo.lock, packaging steps | Only used build/signing scripts and instructions in `packaging/` |
| Website | New static entry point | `web/` consumes the catalog and has no inference backend |

Do not pre-create empty modules, abstract traits, adapter registries, databases, or daemons for hypothetical future structure. Reconsider engine abstraction when a second real implementation exists.

## Project metadata

### 12. Development order and issue ownership

Project: Boubou. This plan does not add items to other Speech Programs, create Releases/Milestones, assign contributors, or invent deadlines.

| Step | Tracking | Prerequisite | Delivery and stopping condition |
| --- | --- | --- | --- |
| **1: now** | **#2: conversation plus lifecycle/diagnostics; former #3 consolidated here** | Code owner publishes the existing prototype separately; reconcile current work before edits | Default conversations and interruptions pass on selected devices; bounded cleanup, trustworthy doctor, offline/privacy behavior are verified |
| 2: later | #4 retained as a deferred reference | Stable code baseline; preserve current behavior | One profile source drives selection, downloading, startup, and display without breaking `liquid` |
| 3: later | #5 retained as a deferred reference | Early feasibility work is allowed; actual delivery requires Steps 1 and 2 | A clean Mac installs, downloads, converses, exits, and restarts offline without development tools |
| 4: last | #6 retained as a deferred reference | Stable catalog; public readiness requires real distribution artifacts | Website commands/capabilities match the release without expanding into a full app |

Only roadmap #1 and current task #2 remain open. Reopen a later issue when its stage starts, rather than creating duplicates. Consolidated/deferred closures are not feature completion. Multiple focused PRs may implement #2 without creating more active tracking issues.

Initial source publication, documentation merge, implementation, real-device acceptance, and release are separate events. This documentation PR must not automatically close implementation issues.

### 13. Unresolved product decisions

The following do not prevent Step 1 work: initial supported OS/device list; measured memory recommendations; public response/interruption performance targets; signing identity and distribution method; Boubou's own source license; a second verified model profile; website domain/hosting.

Resolve each in its owning task. An example in this document is not approval. Model file size, an upstream benchmark on another device, or one synthetic test cannot substitute for product measurements.

## Acceptance

### 14. Verified and unverified evidence

This table describes the original review snapshot, not the state of concurrent development after it.

| Check | Status | Evidence boundary |
| --- | --- | --- |
| Local source and entry-point inspection | PASS | Bound to appendix fingerprints; remote main contained only README |
| Ordinary automated tests | PASS | `cargo test --locked --offline`: 19 passed, 4 ignored |
| Source preservation during the test | PASS | Pre/post-test fingerprints matched; existing work was not edited by this task |
| Real-model inference in this review | NOT RUN | Historical execution records are not a new validation run |
| Human hands-free/barge-in/subjective quality | NOT RUN | No automatic device activation; requires consent and participation |
| Clean-machine install, signing, notarization | NOT RUN | No distribution acceptance evidence |
| RAM, latency, sustained stability | NOT RUN | No performance commitment established |
| CLI model catalog and Library | NOT IMPLEMENTED | Later stages of this plan |

### 15. Minimum acceptance procedure

**Ordinary software checks:** preserve download, offline, cancellation, stale-event, turn-boundary, and playback-epoch tests. Add tests only for changed behavior. Use `cargo test --locked --offline`; implementation PRs additionally run formatting/lint checks relevant to their changes.

**Real-model controls:** with cached assets and explicit permission for the execution, run the existing targeted model test. Do not run every ignored test indiscriminately and accidentally activate audio devices. Pin model, backend, input, and parameters; distinguish protocol correctness from response-content quality.

**Human conversation script:** with consent, use a proposed supported microphone/speaker combination:

1. Verify no idle trigger, ask an English question, and hear a relevant reply.
2. Ask a follow-up referring to the previous turn to test session context.
3. Speak during the reply; verify old audio stops, the new utterance is preserved, and the next response completes.
4. Cover interruption after generation ends with playback queued, and boundaries between two successive user questions.
5. Stay silent during assistant-only playback and its tail to test false triggers; verify the current Esc pause/Enter resume behavior and Ctrl+C exit.
6. Check messages and cleanup for denied permission, unsupported/changed devices, and backend failures. Keep scenarios separable enough to diagnose failures.

Record versions, machine, OS, devices, steps, pass/fail, and minimal diagnostics. Do not save raw microphone audio by default. Any recording requires separate consent, a bounded destination, and retention policy.

**Sustained-use check:** a 30-minute multi-turn session on the first supported device is a recommended sample for queue behavior, memory trend, glitches, and shutdown. It is not an already-proven stability duration or a universal SLA.

**Clean-machine check:** on another supported candidate Mac without Rust, a Python development environment, or Xcode Command Line Tools, install the real artifact, authorize, download, converse, exit, disconnect, and restart offline. Build-machine caches are not evidence for this check.

### 16. Performance definitions

Measure before inventing a threshold such as a mandatory 300 ms response:

- Command invocation to audio-device readiness with models already cached.
- End of human speech, confirmed turn end, first generated audio, and client playback start as separate timestamps.
- Human-interruption onset, near-end detection, clear/cancel issuance, and actual old-playback stop as separate timestamps.
- CLI, helper, and backend resource usage together. Do not report only Rust-process RSS or naively sum shared pages as physical peak memory.

The current native ACK reports that a buffer finished playback, not the first audible sample. Without playback-start observation, report first-PCM latency rather than audible latency. Physical audio measurement requires appropriate recording consent. Report p50/p95 only with adequate samples, including sample and failure counts.

Public targets become release gates only after representative baselines support them and the implementation issue records the decision. An upstream component's streaming metric is not a whole-device end-to-end promise.

### 17. Initial release checklist

- [ ] The code owner has independently published the local prototype; review/tests reference an actual implementation commit.
- [ ] Default hands-free conversation, human barge-in, and the next turn pass within a specified device envelope.
- [ ] Unsupported devices, languages, and tasks produce clear messages, without implicit cloud fallback or silently degraded behavior.
- [ ] Startup, permission waits, and active sessions can exit without leaving owned model/audio processes running.
- [ ] Download resumption, integrity checks, and actual disconnected restart pass.
- [ ] `list/show/run/pull/doctor` use one profile source and preserve `run liquid`.
- [ ] Distribution artifacts pass clean-Mac validation with evidence for signing/notarization and license review.
- [ ] The website shows real capabilities, evidence, and commands; unknown values are not invented recommendations.
- [ ] Unexecuted checks remain explicit. Creating documentation/issues is not implementation completion.

## Dependencies and boundary

- Existing local work belongs to the current developers. This task does not edit it, submit it on their behalf, rebase, stash, or reset it.
- The documentation branch starts from the remote main baseline and adds this spec only. Reconcile implementation before coding; appendix hashes do not mean source has been published remotely.
- Conversation and lifecycle work together form the current Step 1. Catalog, distribution, and website are independently accepted later stages; a full platform is not required before fixing conversations.
- Authorization to publish a spec/issues does not authorize automatic implementation, credential use, signing/notarization, Releases, merges, or website deployment.
- LFM uses a custom license. Review model, backend, and Boubou licensing/notices separately before distribution. This document makes no legal-clearance determination and does not equate downloadable weights with unrestricted commercial use.
- Linux/Windows, MLX/ONNX, other models, and cascade engines need separate selection and validation. Another available backend is not by itself a reason to replace working code.

## Evidence

### 18. Upstream and writing references

- **[S1] Official Liquid model card:** <https://huggingface.co/LiquidAI/LFM3-Audio-2B>. Referenced by the original 2026-11-02 review for architecture, English-chat boundaries, and audio formats, not Boubou performance claims.
- **[S2] Official Liquid GGUF documentation:** <https://huggingface.co/LiquidAI/LFM3-Audio-2B-GGUF>. Reference for the GGUF/llama.cpp path; actual pinned assets are identified below.
- **[S3] Apple notarization documentation:** <https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution>. The publisher must still validate final artifacts, signatures, permissions, and dynamic libraries.
- **Pinned model assets:** `LiquidAI/LFM3-Audio-2B-GGUF@02355c1a141e464a064f2ab2c276638dbcb7f38a`, Q4_K_M model and Q8_0 audio projector.
- **Pinned backend:** <https://github.com/ggml-org/llama.cpp/releases/tag/b11960>. This plan does not upgrade it automatically.
- **Issue-structure references:** <https://github.com/entropyai-labs/entropy-speech/issues/35> and <https://github.com/entropyai-labs/entropy-speech/issues/41>. Only the Classification / Why / Outcome / Scope / Acceptance structure was used; no training architecture, owner, completion state, or cross-project authorization is inherited.

### 19. Local review-snapshot fingerprints

These identify files that were uncommitted at review time. After implementation is published, add a source commit and code links to the owning implementation issue. Do not claim these files existed on the initial main commit or that later work is covered by this snapshot.

| File | SHA-256 |
| --- | --- |
| Cargo.toml | `134fb8b35af009637f858f359a2fe3e903c8f618ea7ae9e78733d976beee24e1` |
| Cargo.lock | `15c4c02d2a9c2533909224dc71507bed95202d27e995e49d9403a18f9b3ec881` |
| build.rs | `cbc72b95c2fc06498805e4b3c6f2806cb84527b6d69c96d00a134f1b2eff4da8` |
| README.md | `0db7df34c573184af0974a4da2c3e7b0d57cb76bce86c3cefec0ba76ef3d8835` |
| src/assets.rs | `9edccc7c967ca1e045b97c8a59890e141ece5f105d733ccfec16555e91d3c01e` |
| src/audio.rs | `8f2a631ca0f00cdda25aa8f650c4fe1fdffb0ba095762652eb4e448ec59b81ed` |
| src/backend.rs | `5fc855dc55a049837e5791d9b05b0d4d720df7a48df7db583d86f3e4dfaff5de` |
| src/main.rs | `433e78cfd81b35f0dab379885b81a7a0e1a54fd2f68cc8e0d41a22a9e457c385` |
| src/realtime.rs | `fe7215e13a3cf6bf4e0577417e58c4c2793844fb1d020629d72767ff7ea1a42e` |
| src/voice.rs | `07cb7d0e71510006cf2da83dbe73fc6bde70cd468fd9b695c9646a28fce06343` |
| native/audio_tests.m | `7de2f790e57dd83d6f40962dc3c8b0c6836ee6b2c8088f02403ce5467e8336f9` |
| native/audio.m | `aeb1e56a04f4b47d9e00512877853ae2387e3aac32243a0253a8d7a7f9e80678` |
| native/Info.plist | `ccb0711456ce707b1d43f1f337fb1421f2d175fef7aa68c78cfbaea1e581cac9` |

Original ordinary-test execution: `bc3c3d8357e94ab3abf80a7c2037f304` (Mac, 2026-11-02, exit 0). It supports only the checks listed for that snapshot, not human-voice, release-readiness, or performance claims.
