Boubou Liquid 2B — Apple Silicon Mac development package

Extract, open Terminal in this directory, then run:
    sh ./install.sh --run

No developer tools are needed. First use downloads about 1.42 GB of verified
assets and asks for microphone access. Inference stays local.

Later runs:
    "$HOME/.local/bin/boubou" run liquid
    "$HOME/.local/bin/boubou" run liquid --offline

Speak English to chat or interrupt. Esc pauses, Enter resumes, Ctrl+C exits.
For other commands: "$HOME/.local/bin/boubou" --help

Installation defaults to ~/.local/bin. Set an absolute BOUBOU_INSTALL_DIR for
both scripts to change it. Reinstall to upgrade or roll back. Uninstall with:
    sh ./uninstall.sh
Models in ~/Library/Caches/boubou and saved audio are kept.

SHA256SUMS lists file hashes; BUILD.json records provenance. Full third-party
licenses are under licenses/. No model weights or user data are bundled.

Development candidate: Developer ID signing, notarization, browser-download
permissions and installation on another Mac are unverified. Public distribution
review, including Boubou's own license, remains pending.
