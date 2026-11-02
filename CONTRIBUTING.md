# Contributing to Boubou

Boubou is a development prototype for local English voice conversations on
Apple Silicon Macs. Start with the [README](README.md), the repository's
[contribution rules](AGENTS.md), and the
[product specification](docs/specs/0001-liquid-macos-product.md).

All repository-facing contributions, including Issues and review comments, must
be in English. Never publish recordings, transcripts, credentials, or private
machine details. Report vulnerabilities through [SECURITY.md](SECURITY.md).

## Choose a focused change

Check existing Issues and PRs before starting. A reproducible bug, documentation
correction, or small maintenance change can use a focused PR. Discuss material
changes to public commands, architecture, permissions, asset handling, or
persistent data in a Spec first; use the [Spec template](docs/specs/TEMPLATE.md).
Record a maintainer's approval before implementing that expanded scope.

An explicit current maintainer request can authorize scoped work without a
separate Issue. A draft Spec or feature request does not approve implementation.
The current product work remains tracked in
[Issue #2](https://github.com/entropyai-labs/boubou/issues/2); an engineering
maintenance PR must not close it as if device acceptance were complete.

## Set up and verify

For the supported native build, use an Apple Silicon Mac, Rust 1.88 or newer,
the `rustfmt` and `clippy` components, Xcode Command Line Tools, Git, and Python
3.9 or newer. Python runs developer checks; it is not an application runtime
dependency. Linux checks exercise portable Rust behavior, not a supported Linux
voice product.

Create a branch from an up-to-date `main`. For a fork, use your fork as `origin`
and the official repository as `upstream`. Preserve unrelated local work and
use an isolated worktree when needed. Install dependencies from `Cargo.lock`;
do not regenerate the lockfile merely to make a check pass.

Run the shared verification command from the repository root:

```sh
python3 scripts/verify.py
```

When Cargo dependencies are already cached, the same checks can run without
dependency downloads:

```sh
python3 scripts/verify.py --offline
```

See [Testing](docs/development/testing.md) for individual commands and evidence
limits. Ordinary verification does not download model weights, run inference,
or activate audio devices. Run targeted tests while developing, then run the
complete verification command on the final candidate.

## Submit a pull request

1. Keep one focused branch and review the entire diff against the intended base.
2. Add a regression for each behavior change, including the relevant failure
   path. Update the owning documentation rather than duplicating instructions.
3. Stage exact paths and commit with a descriptive English subject, for example
   `fix(audio): reject truncated capture events`.
4. Push the branch and open a PR using the provided template. Explain the
   problem, resulting behavior, scope, actual test results, and material risks.
5. Address review findings with focused updates. Rerun affected checks and the
   final verification after changes; previous evidence does not cover new code.

Report `PASS`, `FAIL`, and `NOT RUN` separately, with the checked commit and
environment. List ignored model/device checks as unperformed unless they were
explicitly executed. Link an Issue with `Refs #N` for partial work; use a closing
keyword only when its full acceptance is complete and closure is authorized.

Follow the [review standard](docs/development/review.md). Passing CI and opening
a PR do not grant merge or release permission. Do not force-push, rewrite
published history, publish a package, or change repository access as an
incidental part of a contribution.
