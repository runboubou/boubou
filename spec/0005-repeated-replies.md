# Repeated replies

**Problem:** Liquid can repeat the same stock answer throughout a long session.

**Behavior:**

- Keep the model-card instruction first, then ask for brief answers to the latest utterance, varied replies and continued conversation after acknowledgements.
- Use text temperature 0.3, min-p 0.15 and repetition penalty 1.05; disable top-k/top-p. Leave audio sampling unchanged.
- Renew context at the next safe input boundary after four consecutive completed replies have identical text, ignoring case/punctuation, with at least four words. Interrupted replies neither count nor reset the sequence.

**Failures:** Short answers and up to three repeats do not trigger renewal.
Near-matches are not detected. Renewal discards prior history under the
[context-renewal contract](0003-automatic-context-renewal.md).

**Verification:** Ordinary tests cover sampling, instructions, normalization and
renewal. The opt-in sustained test uses 120 synthesized turns, interruptions and
renewal without audio devices; real microphone acceptance remains separate.
