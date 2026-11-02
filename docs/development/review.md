# Review standard

Review establishes whether a specific change satisfies its agreed scope and
evidence requirements. No review or test suite proves that all bugs are absent.
Use [CONTRIBUTING.md](../../CONTRIBUTING.md) for the contributor workflow and
[Testing](testing.md) for the verification commands.

## Match review depth to risk

| Change | Required review evidence |
| --- | --- |
| Documentation or templates | Correct instructions, valid local links, English prose, no private content, and repository checks |
| Rust behavior | Complete diff, relevant success and failure regressions, full verification, and compatibility impact |
| Audio, concurrency, cancellation, protocol, or asset integrity | Ordering and ownership analysis, bounded failure handling, stale-event/corruption regressions, and explicit native/model acceptance status |
| Public commands, permission boundaries, persistent data, or architecture | Approved Spec, migration/compatibility decision where relevant, observable acceptance criteria, and the checks above |
| CI, build, dependencies, or distribution | Locked inputs, workflow permissions, trust boundaries, supported toolchains, and artifact or release acceptance where applicable |

A small documentation fix does not need a separate Spec or invented rollback
plan. A material change does not become safe merely because its diff is short.

## Review the complete candidate

Before approval, record the PR's full head commit SHA and examine the complete
diff against its base, including generated configuration, lockfiles, tests, and
documentation. Check the affected callers and failure paths, not just changed
lines. Confirm that:

- The implementation matches the requested behavior and approved scope.
- Invalid input, cancellation, timeouts, and unexpected responses produce clear
  failures; broad catches or fallback paths do not fabricate success.
- Cleanup targets owned children and resources. Limits and timeouts remain
  bounded; cancellation cannot submit stale input or resume obsolete playback.
- Model/backend downloads retain size and hash verification, offline behavior,
  and safe handling of partial or corrupted data.
- Tests observe behavior and relevant failures, use isolated fixtures, and do
  not reproduce a second implementation merely to agree with the first.
- Native audio and model claims have appropriate evidence. Mocked services,
  portable tests, or a successful build do not establish conversation quality.
- Credentials, recordings, transcripts, private logs, and personal paths are
  absent from the published change.

Report findings with severity, exact location, the failure scenario, and a
concrete requested correction. Distinguish blocking correctness concerns from
optional suggestions. Close each discussion with evidence or an explicit
maintainer decision; do not silently dismiss unresolved findings.

## Approve and merge

Require at least one independent reviewer for code changes. Authors cannot
approve their own PRs. The author's own review `COMMENT`, an automated summary,
and a green CI run are not an independent `APPROVED` review. Approval must cover
the latest head commit. A new push requires review of the new complete candidate
and current checks.

Before an authorized merge, verify that the intended base/head are unchanged,
required jobs succeeded for that candidate, discussions are resolved, and no
change request remains. State material checks still `NOT RUN`; obtain the
required acceptance for the change before claiming completion. Merge, release,
deployment, and completion of product Issue #2 are separate decisions.

## GitHub enforcement is a separate setting

Maintainers should configure `main` to require a PR, at least one independent
approval, dismissal of stale approvals, resolution of review discussions, and
the stable aggregate required check `quality`. That check must pass only after
the repository check and every runtime matrix job succeed. Require review of
the latest push, protect against deletion and force pushes, and avoid
administrator bypasses. Verify these settings through GitHub and test that a
failing candidate is blocked. An author cannot supply the required approval.

This document and a workflow file do not configure branch protection or rulesets.
Record the observed settings and any unavailable enforcement separately. Do not
copy another repository's team IDs, workflow IDs, or maintainer review exemptions.
