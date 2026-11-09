# Contributing

Thanks for helping. Boubou is small on purpose, so the best contributions are
focused: one bug, one model or one improvement per pull request.

## Before you start

Search the existing issues first. For anything that changes a command, adds a
model or touches the audio path, open an issue and agree on the approach
before writing code. Model requests should link the model card and its
license.

## Development

You need an Apple Silicon Mac, Rust 1.91 or later and the Xcode Command Line
Tools. Run the same checks as CI before you push:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```

Tests cover protocol, download and lifecycle behavior with fakes. CI also runs
the real models on GitHub-hosted Macs: the Liquid tests marked `#[ignore]` in
[liquid-macos.yml](.github/workflows/liquid-macos.yml) and the Qwen voices in
[qwen-macos.yml](.github/workflows/qwen-macos.yml). Three ignored tests stay on
a real Mac: `speaker_echo_probe` needs speakers and a microphone, and the long
Liquid conversation and context renewal tests are too slow on hosted runners.
Changes to echo handling, audio, models or interruption still need a short test
on a real Mac.

## Pull requests

Describe what changed and why, any change to commands, downloads or privacy,
and how you tested it. Say which device checks you ran and which you did not.
Write in English. More conventions are in [AGENTS.md](AGENTS.md).

A core maintainer must approve the latest commit of your pull request before it
can merge. A new push needs a new approval. Pull requests that
[@runboubou/core-maintainers](https://github.com/orgs/runboubou/teams/core-maintainers)
open from this repository need only passing checks.

## Releases

Maintainers bump the version in `Cargo.toml` and `packaging/package.json`,
merge, then push a matching `v<version>` tag. The Release workflow checks every
model profile, attaches the Qwen runner to the GitHub release and publishes the
tested tarball to npm. If publishing fails, re-run only the failed job;
re-running everything rebuilds a different runner, which the release refuses.

## Privacy

Do not attach recordings, transcripts, raw logs or personal paths to issues or
pull requests. `boubou doctor` output and your Mac model, memory and macOS
version are usually enough.
