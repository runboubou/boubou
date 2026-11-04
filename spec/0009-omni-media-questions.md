# Local image and short-video questions

**Problem:** Omni voice chat cannot submit pictures or video through the pinned
Realtime channel.

**Behavior:** `boubou ask LFM3-Omni-2B FILE "QUESTION"` prints one local text
answer without opening the microphone. Reuse the Omni registry entry, verified
assets and bundled llama.cpp b11960 multimodal CLI. Keep voice startup unchanged.

Accept one PNG/JPEG image up to 2048 × 2048 pixels, or a constant-frame-rate
MP4/MOV video up to 10 seconds and 512 × 512 pixels, with 4–600 source frames
at up to 60 fps. Files are at most 64 MiB; questions contain 1–4096 UTF-8 bytes.
Video needs `ffmpeg` and `ffprobe` in PATH; image inspection uses macOS `sips`.
Use the pinned model's 2 fps sampling, frame-group timestamps and interleaved
soundtrack handling. A video without an audio track is accepted as visual input.

Reject unsupported models and invalid paths, types, file sizes or questions
before asset preparation. Inspect dimensions/duration and verify video decoding
before inference, under cancellable owned processes. Reserve a conservative
media/question/output allowance inside the profile's context. Text output uses
greedy decoding and at most the profile's output-token limit. Inspection and
decode waits are bounded; inference times out after three minutes.

Keep media, prompts and output in memory; create no processing files or
conversation logs. Ctrl+C kills and reaps the owned runner/decoder process group.
`--offline` uses verified cached assets only. No camera, screen capture or spoken
visual replies.

**Failures:** Clearly report unreadable/unsupported media, missing decoding tools,
invalid metadata, decode failures, exceeded limits, backend failures and empty
answers. Never fall back to a voice model or silently discard a soundtrack that
fails the decoding check.

**Verification:** Rust tests cover model capability, local input validation,
image/video/context limits and process cancellation. Run all repository Quality
checks. On Apple Silicon demonstrate offline image QA and video QA requiring
both picture and sound; verify process cleanup and absence of processing files.

Reference: [pinned multimodal input handling](https://github.com/ggml-org/llama.cpp/blob/8642541b255bfb5dc39cd351a178be09013a55d6/tools/mtmd/mtmd-helper.cpp).
