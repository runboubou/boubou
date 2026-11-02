# Automation contribution rules

Read the root `AGENTS.md` and `docs/development/review.md` first.

- Ordinary PR verification runs candidate code on GitHub-hosted runners with
  `contents: read`. Do not use private machines or self-hosted runners for forks.
- Do not add `pull_request_target`, repository secrets, write permissions,
  environment credentials, automatic merging, publication, or deployment to
  ordinary verification. A separate design and explicit authorization are needed
  for privileged automation.
- Pin external actions to complete commit SHAs and record their upstream version
  in a comment. Review action updates; a SHA pin alone does not establish trust.
- Keep checkout credentials unpersisted, job timeouts finite, and dependency
  installation locked. Ordinary tests must not download models or open devices.
- Local development and CI use `python3 scripts/verify.py`. The `quality` job must
  fail when a required dependency fails, is cancelled, or is skipped.
- Keep PR code verification independent of metadata-only edits. Do not let a PR
  title/body edit replace or cancel the code-check run for the same candidate.
- `scripts/check_repository.py` provides limited static guardrails for the simple
  workflow style used here. It does not parse all YAML semantics, audit secrets,
  review shell scripts, or prove that a workflow is safe. Review complete workflow
  changes and their token/data boundaries independently.
- Adding a workflow does not configure required checks or review enforcement.
  Verify the actual GitHub ruleset/settings separately before reporting them as
  enabled. Keep the aggregate check name `quality` stable when configuring it.
