# Spec: Repeated replies in long conversations

**Problem:** Long sessions with the pinned Liquid profile lock into one stock reply, such as "You're very welcome! Have a great weekend!", and give it to every new utterance, including real questions. Controlled runs reproduce this without a microphone: 80 synthesized English turns with a barge-in on every third reply looped in 2 of 2 greedy runs and in 3 of 6 runs at temperature 0.3 when only the model card's one-line prompt was used. The b11960 realtime sampler is rebuilt for each reply and penalizes only that reply's tokens, so no sampling setting discourages copying earlier turns.

**Behavior:**

- Keep the model card's voice-chat instruction first and add short conversational guidance: answer the user's latest words in one to three short sentences, do not repeat an earlier reply, and keep the conversation going after a bare acknowledgement.
- Sample reply text with Liquid's LFM chat defaults (temperature 0.3, min-p 0.15, repetition penalty 1.05; top-k and top-p disabled) instead of greedy decoding. Audio frames keep the sampler stored in the model.
- When four consecutive completed replies have identical text (case and punctuation ignored, at least four words), renew the context at the next input boundary exactly as [automatic context renewal](0003-automatic-context-renewal.md) does. Interrupted replies neither count nor reset the sequence.

**Failure cases:** Short answers, such as a name or "Goodbye!", and a reply the user asks to hear again up to three times do not trigger renewal. A loop-triggered renewal still discards all earlier context, so the user may need to restate facts. The interruption notice is unchanged. Near-identical repetition is reduced by the prompt and sampling, not detected.

**Scope:** Backend sampling flags, realtime session instructions, and realtime renewal admission. No CLI, protocol, dependency, asset, persistent-data, or transcript-storage change.

**Acceptance:**

- Ordinary tests cover the backend sampling arguments, the session instructions, loop counting (normalization, short, interrupted, and different replies), and loop-triggered renewal at the next input boundary.
- The opt-in `liquid_sustained_voice_conversation_keeps_replies_varied` test sends 120 turns synthesized by macOS `say` (roughly 15-20 minutes of spoken conversation) through the real client and backend, with barge-ins and at least one renewal, and fails on consecutive repeated replies. It opens no audio devices.
- Real microphone conversations and sustained human sessions remain separate device checks.
