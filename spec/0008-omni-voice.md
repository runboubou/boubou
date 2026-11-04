# LFM3-Omni voice chat

**Problem:** The shared registry exposes only LFM3-Audio-2B. Users should also
be able to start Omni with `boubou run LFM3-Omni-2B`.

**Behavior:** Register English voice chat with Omni's Q4_K_M backbone and Q8_0
projector at revision `d508b88f08a5b4286f2d2805a6a8d69a0ff44c72`. Verify their
sizes and SHA-256 hashes and the matching license before inference. Share the
existing pinned llama.cpp b11960 runtime, Realtime protocol, audio devices and
owned-process cleanup. Disable reasoning and use Omni's text sampling defaults;
its GGUF supplies audio sampling. Reuse the LFM3 interleaved voice-chat prompt,
32K context budget and automatic context renewal.

Expose both profiles through `list`, `run MODEL`, `pull MODEL`, `doctor MODEL`,
CLI help and package provenance. Plain `doctor` still checks Audio. Preserve
Audio's defaults and cache paths. Image/video input and LFM2.5 are separate work.

**Failures:** Reject unknown models before downloads or audio access. Reuse
existing unsupported-platform, offline, integrity and backend errors. No fallback
to another model when Omni assets are unavailable.

**Verification:** Rust tests cover both selections and selected asset paths,
backend arguments and session settings; repository Quality checks and packaged
launcher checks pass. Verify cached multi-turn audio input, cancellation followed
by another turn, pause/resume and process cleanup for both models on Apple
Silicon. Identify microphone or other acceptance that has not been run.

Reference: [pinned official Omni model card](https://huggingface.co/LiquidAI/LFM3-Omni-2B-GGUF/blob/d508b88f08a5b4286f2d2805a6a8d69a0ff44c72/README.md).
