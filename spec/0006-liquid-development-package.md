# Liquid 2B development package

**Problem:** End users should run the existing runtime without a compiler.

**Behavior:** Build one versioned macOS arm64 archive from a clean, locked commit.
Include the CLI, install/uninstall scripts, provenance and checksums. Fetch full
model/backend licenses from pinned upstream URLs, verify SHA-256 and include them
with dependency notices; keep no maintained license copies in source.

`sh ./install.sh --run` installs into ~/.local/bin and starts Liquid 2B.
An absolute BOUBOU_INSTALL_DIR overrides the destination. Upgrades atomically
replace only the CLI; removal keeps model caches and saved audio.

**Failures:** Reject unsupported hosts, invalid arguments, symlink destinations,
failed downloads and checksum mismatches without publishing partial artifacts or
replacing installed files.

**Verification:** Run repository checks and installer regressions. Inspect the
actual archive, license hashes and executable; verify install, cached offline
inference and owned-process cleanup. Reuse unchanged runtime acceptance.

Development candidate only: signing, notarization, browser-download permissions
and another-Mac installation are unverified. Runtime behavior, model/voice
selection and websites are outside this packaging change.
