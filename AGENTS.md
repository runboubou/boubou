# Repository rules

- Write repository content and GitHub metadata in English; check authored text before publishing. Model input languages are unrestricted.
- Keep secrets, recordings, transcripts and personal diagnostics out of commits.
- Preserve unrelated work. Editing current text does not erase history; deleting GitHub content/repositories or rewriting Git history requires specific authorization.

## Writing

- Write for humans: lead with the result, command or decision. Keep only what the reader needs to act.
- Give each fact one home and link to details. Omit repeated explanations, task logs, historical snapshots and speculative plans.
- Keep README focused on getting started. Specs describe behavior, relevant failures and verification; omit unused template fields.
- Preserve upstream license texts unchanged in distributed artifacts; obtain pinned, verified copies during packaging instead of maintaining source copies.

## Engineering

- Keep code small, direct and specific to this Rust/macOS runtime. Add no speculative infrastructure.
- Use [the Spec template](spec/TEMPLATE.md) for material behavior, interface, asset or privacy changes; small fixes need no Spec.
- Test changed behavior and important failures with ordinary Rust tests. CI does not establish real microphone/model acceptance.
- Run the checks in [.github/workflows/quality.yml](.github/workflows/quality.yml). Review the final diff and current checks before merging; self-review is sufficient unless GitHub requires approval.
