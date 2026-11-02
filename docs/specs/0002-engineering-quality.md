# Engineering quality baseline

## Classification

- Type: repository quality and contribution workflow.
- Authorization: the maintainer's current request to adapt Open Machine's
  PR, review, Spec, and automated-testing standards to Boubou.
- Status: authorized scope; completion depends on the implementing PR's actual
  review and verification evidence.
- Product boundary: this work does not complete or close
  [Issue #2](https://github.com/entropyai-labs/boubou/issues/2) or approve a release.

## Why

Boubou needs a repeatable contribution and verification path before opening the
repository to external contributors. Existing Rust tests and the native helper
must be checked consistently without accidentally starting inference or audio
devices. Written standards and enforced GitHub settings must be distinguished.

## Outcome

Contributors can prepare a focused change, run one documented check command,
provide reproducible evidence, and receive review of the exact candidate.
Ordinary CI verifies software behavior without downloading models or recording
audio. Missing product acceptance remains visible.

## Scope

- Add contribution, review, testing, security-reporting, PR, Issue, and Spec
  guidance that preserves the existing English-only contribution rule.
- Provide `python3 scripts/verify.py` as the shared complete verification entry
  point, with `--offline` for already cached dependencies.
- Check repository hygiene, formatting, Clippy warnings, ordinary Rust tests,
  and compilation using locked dependencies.
- Exercise the supported native build on macOS ARM64, including the declared
  Rust 1.88 baseline, and portable regressions on Linux.
- Add meaningful regressions and focused fixes for defects found while making
  these gates reliable. Describe any behavior change and failure coverage in
  the implementation PR.
- Keep CI read-only for candidate code, pin external actions, disable persisted
  checkout credentials, and bound job runtime.
- Expose one stable aggregate `quality` check that succeeds only after the
  repository check and every runtime matrix job succeed.

Non-goals: a new application architecture, generalized test selection/cache
framework, automatic merging, model downloads, microphone or speaker activation,
installer publication, a website, or claims of completed human-conversation
acceptance. GitHub rulesets require a separately verified settings change.

## Acceptance

- [ ] The documented verification command runs the actual checks and propagates
  any failure to a nonzero exit status.
- [ ] Ordinary checks neither execute ignored tests nor activate a real model
  or audio device.
- [ ] macOS/native and Linux/portable results are reported separately with
  actual toolchains, test counts, and ignored tests.
- [ ] CI runs for PRs and main changes without write permissions for candidate
  code; `quality` cannot pass when any required job fails, is cancelled, or skips.
- [ ] The complete final diff has review evidence tied to its current commit.
- [ ] The PR records actual `PASS`, `FAIL`, and `NOT RUN` outcomes, including
  any toolchain or device validation that could not be executed.
- [ ] Branch-protection observations are reported separately from files added.
- [ ] No source license is selected, release published, or product Issue closed
  as an incidental effect of this work.

## GitHub enforcement status

On 2026-11-02, reading this repository's rulesets returned HTTP 403 with the
message: "Upgrade to GitHub Pro or make this repository public to enable this
feature". Required-ruleset enforcement has not been deployed by this work; the
workflow and documentation do not enable it. Changing the repository's visibility
or subscription is outside this scope.

When the feature is available, configure `main` to require `quality`, at least
one independent approval, dismissal of stale approvals, resolved review
conversations, and protection against force pushes and deletion. Authors cannot
approve their own PRs. Verify the actual settings and blocked-merge behavior
before reporting those gates as enforced. Until then, maintainers must apply
the documented review and verification requirements when deciding whether to merge.

## Evidence and design decisions

The reference is
[Open Machine](https://github.com/OpenMachineLabs/openmachine/tree/c3d94ef627478abd2b08f5651ee13c53426bd90b),
especially its `CONTRIBUTING.md`, `MAINTAINERS.md`, `test/README.md`, quality
workflow, and [test-architecture Spec #86](https://github.com/OpenMachineLabs/openmachine/issues/86).
Adopt full-diff review, honest evidence, isolated tests, and safe CI boundaries.
Use the complete suite while Boubou remains small; do not copy Open Machine's
Python packaging gates, test selector, hard-coded maintainer IDs, or internal
review exemptions.

The [product specification](0001-liquid-macos-product.md) continues to own
conversation, device, distribution, and release acceptance. The implementing PR
owns this change's final commit identities, test results, and remaining gaps;
this Spec does not predeclare a passing result.
