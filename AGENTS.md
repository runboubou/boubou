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

- Keep changes small, direct, and specific to this Rust/macOS project. Prefer existing modules and standard tools over new abstractions or workflow machinery.
- Preserve cancellation, cleanup, bounded input, offline behavior, and verified model/backend downloads.
- For changed runtime behavior, add focused ordinary Rust regressions covering relevant failures. Use local fixtures or mocks; CI must not require microphones, real models, or large downloads.
- Validate the final candidate with the formatting, Clippy, test, and build commands in [README](README.md#development). CI runs one read-only macOS job and compiles, but does not run, the native audio harness.
- Record verification against the exact candidate as PASS, FAIL, or NOT RUN. Simulated tests and green CI do not establish real-model, microphone, permission, acoustic, signing, or release acceptance. See the [product acceptance requirements](spec/0001-liquid-macos-product.md).
- Before merging, review the final diff and current checks. No independent approval is required unless the repository itself enforces one.
- Keep secrets, recordings, transcripts, and personal diagnostics out of commits.

## Specifications

- Small fixes and refactors can explain their intent and verification in the PR; they need no separate Spec.
- Record a short Spec for material changes to behavior, commands, interfaces, protocol, audio/model lifecycle, asset integrity, privacy, or persistent data.
- Update an existing Spec when it covers the behavior. Create a new file only for a distinct, lasting contract that does not fit an existing Spec; a new PR alone is not a reason to add one.
- Use [the template](spec/TEMPLATE.md) to cover the problem, observable behavior, relevant failures, scope, and acceptance. Keep detail proportional to the change; there is no length quota.
- Keep debugging narratives, run logs, and commit-specific verification in the relevant PR or Issue, with the source revision. Specs retain the behavior, design reasons, and acceptance requirements that future maintainers need.
