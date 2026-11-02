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

## Engineering changes and verification

Read [CONTRIBUTING.md](CONTRIBUTING.md), the [review standard](docs/development/review.md),
the [testing guide](docs/development/testing.md), and the affected Spec before
changing code. Define the outcome, allowed scope, non-goals, and acceptance;
preserve unrelated work and prefer a focused branch or worktree. A draft product
plan is not approval to implement every future stage.

Keep existing owners and readable, direct implementations. Add abstractions or
dependencies only for a present need. Test observable behavior and important
failure paths; do not remove necessary coverage or weaken checks to get a pass.
Use `python3 scripts/verify.py` for the final candidate. Ordinary verification
must not download models, start inference, activate audio devices, or run every
ignored test. Real-model and physical-device acceptance need specific permission
and evidence as described in the testing guide.

Review the complete diff and report the tested commit, environment, and actual
`PASS`, `FAIL`, and `NOT RUN` results. Self-review comments do not replace
independent approval. New code requires current review and check evidence.
Opening a PR or passing CI does not authorize merge, release, deployment, source
license selection, or closure of product work whose acceptance remains open.
