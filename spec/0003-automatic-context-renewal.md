# Spec: Automatic context renewal

**Problem:** The pinned Liquid realtime backend has a 32,768-token context window. Accumulated audio and replies eventually exhaust it, ending an otherwise valid conversation. Keep long sessions usable by clearing history before the next turn would exceed capacity.

**Behavior:**

- Before admitting a new input segment, conservatively reserve space for a full 30-second input, a 1,024-token response, and protocol framing. For this pinned profile, 16 kHz audio uses at most one context position per 1,280 samples, rounded up per committed segment. Output tokens include audio generation steps.
- Count partial audio committed by pause, interrupted responses, and interruption notices. Discount an output reservation only from valid final usage belonging to the active response; absent or invalid usage keeps the full reservation. Stale response events cannot change accounting.
- When there is insufficient space, renew at an input boundary after generation or cancellation has finished. Close the old WebSocket, then establish and configure a fresh session in the background. Admit input only after `session.created` and `session.updated` acknowledge readiness. Keep the backend process and model loaded.
- Continue draining capture into the existing bounded pending-turn queue while renewal runs. Preserve separate utterances, playback controls, pause/resume, and exit handling. A short terminal notice identifies the context clear.
- Renewal discards **all** previous model context. It provides continued runtime with bounded memory; it does not preserve recent turns, summarize history, or provide unlimited recall. Users may need to repeat earlier facts. Renewal introduces no recording, transcript, or history persistence.

**Failure cases:** Empty input does not consume an input reservation. Never rotate halfway through an utterance or before cancellation completes. Connection failure or initialization timeout produces an actionable error, rather than accepting input into an unready session. Ctrl+C remains cancellable during renewal. Existing queue and turn limits remain enforced.

**Scope:** Update the existing realtime client, conversation-loop integration, help, and focused tests. Retain the pinned backend, model assets, offline behavior, and native audio architecture. Add no dependency or persistent history store. Precise interruption rollback and selective history deletion remain unsupported.

Backend evidence is pinned to llama.cpp commit `8642541b255bfb5dc39cd351a178be09013a55d6` (`b11960`): [realtime dispatch and usage](https://github.com/ggml-org/llama.cpp/blob/8642541b255bfb5dc39cd351a178be09013a55d6/tools/server/server-realtime.cpp), [slot cleanup and context-shift exclusion](https://github.com/ggml-org/llama.cpp/blob/8642541b255bfb5dc39cd351a178be09013a55d6/tools/server/server-context.cpp), [streaming audio rows](https://github.com/ggml-org/llama.cpp/blob/8642541b255bfb5dc39cd351a178be09013a55d6/tools/mtmd/mtmd.cpp), and [text/audio generation steps](https://github.com/ggml-org/llama.cpp/blob/8642541b255bfb5dc39cd351a178be09013a55d6/tools/mtmd/mtmd-helper-gen.cpp). This protocol has no public history-delete/reset event; `input_audio_buffer.clear` does not remove encoded audio.

**Acceptance:**

- Ordinary Rust tests cover repeated renewal, maximum input/output budgets, paused partial input, missing/invalid usage, cancelled and stale responses, and bounded accounting. Run the repository's formatting, Clippy, test, and build checks; attach exact-commit results to the PR.
- Local WebSocket mocks cover old-session closure, delayed replacement readiness, queued utterances, initialization failure, and cancellation. They must not require a model or audio devices.
- A separate opt-in real-backend test crosses a renewal boundary using text input without opening audio devices. Record the implementation commit, model revision, backend, machine, and result. **NOT RUN**; mocks do not establish model acceptance.
- Real microphone capture through renewal, interruption/pause controls, and sustained conversation remain separate human/device checks. **NOT RUN**; CI cannot establish these results.
