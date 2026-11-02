# Repository Instructions

## English-only contributions

All repository-facing contributions must be written in English. Private conversation with the maintainer may use Chinese; do not copy that language into repository content.

This requirement covers:

- Source comments, docstrings, identifiers, filenames, user-facing messages, configuration descriptions, and committed test/example text.
- README files, specifications, architecture notes, contribution guidance, and other documentation.
- Issue and pull-request titles, descriptions, comments, reviews, labels, and milestone text.
- Branch names, commit messages, release titles, release notes, and other published project metadata.

Before each commit, push, issue/PR write, comment, or release, check the exact authored content for Chinese characters and Chinese prose. Translate it to English before publication. Do not publish Chinese temporarily and translate it afterward: GitHub may retain the original text in edit or rename history.

This is a contribution rule, not a restriction on the languages the speech model may process. Do not publish user recordings, transcripts, or other private data as a language-check workaround.

## Existing content and history

Preserve unrelated work and do not silently change technical meaning while translating. If existing content violates the language rule, identify the affected surface and correct editable content within the authorized task.

Distinguish current content from issue-title events, edit history, earlier commits, and platform caches. Editing the current text does not prove historical copies were removed. Do not claim the repository is free of historical Chinese content without verifying that scope.

Deleting or recreating issues, deleting discussions, force-pushing, rewriting Git history, or replacing the repository requires explicit authorization for the specific destructive action. Do not perform those operations merely to satisfy a language scan.

## Engineering

- Keep changes small, direct, and specific to this private Rust/macOS project.
- Use `spec/TEMPLATE.md` for material behavior, interface, asset, or privacy changes; small fixes need no separate Spec.
- Test behavior and relevant failures with ordinary Rust tests. Never treat simulated tests as real microphone/model acceptance.
- Before merging, review the final diff and current checks. No independent approval is required unless the repository itself enforces one.
- Keep secrets, recordings, transcripts, and personal diagnostics out of commits.
