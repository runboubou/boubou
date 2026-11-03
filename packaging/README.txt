Boubou Liquid 2B — macOS arm64 development candidate

Extract this archive, open Terminal in the extracted directory, then run:

    sh ./install.sh --run

This verifies and installs the precompiled CLI into ~/.local/bin, then starts
an English conversation with the current Liquid 2B configuration. End users do
not need Rust, Python, Xcode Command Line Tools or a separate inference backend.
First use downloads about 1.42 GB of pinned assets. Allow microphone access when
macOS asks. Inference stays local; there is no cloud fallback.

Later runs, from any directory:

    "$HOME/.local/bin/boubou" run liquid
    "$HOME/.local/bin/boubou" run liquid --offline

Pause with Esc, resume with Enter, and exit with Ctrl+C. Speaking interrupts a
reply. The current profile uses English and the model's default voice. Small-
model recognition and answer errors remain possible. Model/voice selection and
website features are outside this MVP.

An explicit absolute BOUBOU_INSTALL_DIR overrides the binary directory for both
install.sh and uninstall.sh. No shell startup files are edited. If ~/.local/bin
is already on PATH, the short command is: boubou run liquid

Upgrade or roll back by extracting the chosen candidate and running install.sh.
Only the binary is replaced; model caches and saved WAV files are preserved.
To remove the installed CLI, run this package's uninstall.sh:

    sh ./uninstall.sh

The existing ~/Library/Caches/boubou cache remains. Delete it manually only if
you intentionally want to remove downloaded assets; it is needed for offline use.

This is a development candidate for authorized testing. Developer ID signing,
notarization, browser-download Gatekeeper/TCC behavior and installation on another
clean Mac have not been qualified. Do not disable Gatekeeper, remove quarantine
attributes, or re-sign the package manually to work around a failed launch.
Record the actual error for release validation.

SHA256SUMS verifies package file integrity; the adjacent archive .sha256 file
checks the download. These checksums are not a trusted publisher signature.
BUILD.json records source, dependency and tool provenance. Third-party license
texts and dependency notices are included under licenses/. Boubou's own public
distribution/source license has not been established; distribution review remains
pending for a formal release. No model weights or user data are included.
