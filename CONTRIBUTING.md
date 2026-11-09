# Contributing

Thanks for helping. Boubou is small on purpose, so the best contributions are
focused: one bug, one model or one improvement per pull request.

## Before you start

Search the existing issues first. For anything that changes a command, adds a
model or touches the audio path, open an issue and agree on the approach
before writing code. Model requests should link the model card and its
license.

## Development

You need an Apple Silicon Mac, Rust 1.88 or later and the Xcode Command Line
Tools. Run the same checks as CI before you push:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```

Tests cover protocol, download and lifecycle behavior with fakes. They cannot
tell whether a real conversation sounds right, so changes to audio, models or
interruption also need a short test on a real Mac.

## Pull requests

Describe what changed and why, any change to commands, downloads or privacy,
and how you tested it. Say which device checks you ran and which you did not.
Write in English. More conventions are in [AGENTS.md](AGENTS.md).

A maintainer must approve the latest commit of your pull request before it can
merge. A new push needs a new approval.

## Releases

Maintainers bump the version in `Cargo.toml` and `packaging/package.json`,
merge, then push a matching `v<version>` tag. The Release workflow checks every
model profile and publishes the tested tarball to npm.

## Privacy

Do not attach recordings, transcripts, raw logs or personal paths to issues or
pull requests. `boubou doctor` output and your Mac model, memory and macOS
version are usually enough.
