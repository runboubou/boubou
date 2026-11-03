# Automatic context renewal

**Problem:** The pinned backend has a 32,768-token context. Keep long sessions
running by clearing history before capacity is exhausted.

**Behavior:**

- Reserve a full 30-second input, 1,024 output tokens and protocol framing before admitting input. Budget audio at one context position per 1,280 samples (16 kHz), rounded up per committed segment; output includes audio generation steps.
- Count paused partial input, interrupted replies and interruption notices. Reduce output reservations only for valid final usage of the active response; missing, invalid or stale usage cannot release budget.
- At an input boundary after generation/cancellation finishes, close the old WebSocket and configure a fresh session. Wait for both `session.created` and `session.updated` before admitting input; keep the backend/model loaded.
- Continue bounded capture and preserve utterance boundaries, playback and keyboard controls during renewal. Show a short notice: all earlier context is lost. Add no history storage or summary.

**Failures:** Empty input reserves nothing. Never renew mid-utterance or before
cancellation completes. Initialization failure/timeout returns an actionable
error. Ctrl+C and existing queue limits remain effective.

**Verification:** Ordinary tests and local WebSocket fixtures cover accounting,
stale/cancelled responses, replacement readiness, queued utterances and failure.
Opt-in real-backend renewal and microphone renewal are separate checks;
fixtures alone do not establish either.
