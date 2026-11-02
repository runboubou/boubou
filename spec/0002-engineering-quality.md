# Engineering quality

Boubou is a private, single-maintainer Rust application for English voice chat on Apple Silicon macOS. Keep the process proportional to the change; prefer existing Rust modules and standard tools over new abstractions, scanners, or approval workflows.

## Development contract

- Write a short Spec for material changes to commands, protocol, audio/model lifecycle, asset integrity, privacy, or persistent data. A small fix can explain its intent directly in the PR.
- Keep changes focused. Preserve cancellation, cleanup, bounded input, offline behavior, and verified model/backend downloads.
- Add an ordinary Rust regression for changed behavior, including the important failure case. Use local fixtures or mocks; never require microphones, real models, or large downloads in CI.
- Validate the final candidate with `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked --all-targets --all-features`, and `cargo build --locked`.
- CI runs one read-only macOS job and compiles (but does not run) the native audio harness. Green CI is not evidence of acoustic quality, device permissions, signing, or release readiness.
- Review the full diff and current check results before merging. Self-review is sufficient for this private workflow when GitHub permits it; no separate approval gate is introduced.

## Acceptance

The commands above pass on the final commit; changed behavior has relevant tests; no product runtime files are changed by this standards PR. Real-device and distribution acceptance remain owned by [the product Spec](0001-liquid-macos-product.md), not this document.
