# Native npm package

**Status:** [Accepted for MVP 1](0001-liquid-macos-product.md).

**Behavior:** Publish `boubou` with the `bobo` command. Require the explicit model
ID `LFM3-Audio-2B` for run/pull. Bundle the Mac arm64 Rust executable and a small
Node launcher; inherit terminal I/O, cancel Rust on shutdown, wait for its cleanup
and preserve exit status/signals.
The documented npx entrypoint selects Boubou's per-user npm cache, without global
installation, npm login or administrator access. No install-time downloader or
npm dependencies; existing model caches and inference behavior stay unchanged.

Build from a clean locked commit. Versions in Cargo/npm must match. Use an
explicit package-file allowlist; include provenance, hashes and verified full
upstream/dependency licenses. Replace the manual shell installation channel.

**Failures:** Reject unknown/missing model IDs before downloads. Unsupported
platforms fail npm installation. Missing executables and failed license checks
produce errors; do not publish partial artifacts.

**Verification:** Run repository checks and CLI/launcher regressions. Test the exact
documented command outside the checkout, including a damaged global npm cache,
anonymous installation, real offline inference, microphone startup and cleanup.
Publish only the reviewed clean-main artifact and read back registry integrity.
