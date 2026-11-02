# Security policy

Boubou runs local inference, downloads pinned model/backend assets, and can use
the microphone and speakers when the user starts a conversation. It is a
development prototype, not a sandbox for untrusted models or executables.

## Report vulnerabilities privately

Do not disclose exploit details, proofs of concept, credentials, personal paths,
recordings, transcripts, or raw logs in a public Issue, PR, or discussion.

If GitHub private vulnerability reporting is enabled for this repository, use
**Security -> Advisories -> Report a vulnerability**. This file does not enable
that setting or establish that it is currently available. Otherwise, use an
already verified private channel to a repository maintainer. If no such channel
is available, wait until a private reporting route is established; do not post
technical vulnerability details publicly or send them to an unverified address.

Provide the affected commit/version, OS and architecture, relevant model/backend
versions, impact, minimal steps using disposable resources, and redacted
diagnostics. Obtain explicit consent before recording audio or testing someone
else's machine. A report does not authorize changes to another person's system.

Maintainers assess reports against current `main` and any explicitly supported
release. No response deadline, older-version backport, or bounty is promised.
Coordinate disclosure after scope and remediation have been reviewed.

## Maintainer responsibilities before public release

- Verify a functioning private vulnerability-reporting route and document it.
- Check the final source/artifacts for secrets and private recordings or logs.
- Verify actual branch protections and required checks; documentation alone is
  not enforcement.
- Choose Boubou's source license explicitly and review model/backend licenses
  and redistribution notices separately. This policy grants no license.
- Verify the exact distribution's integrity, signing/notarization, installation,
  permissions, offline behavior, and cleanup on supported hardware.

Ordinary CI cannot certify those release conditions. For nonsensitive defects,
use the Bug report template.
