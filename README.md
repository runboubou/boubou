# Boubou

Local English voice chat on Apple Silicon Macs with Liquid AI LFM3-Audio-2B.

## Run

Requires macOS 13.3+, Apple Silicon, Node.js 18+ and npm:

```sh
npx --yes --cache "$HOME/Library/Caches/boubou/npm" boubou@beta run LFM3-Audio-2B
```

The command keeps npm downloads in Boubou's user cache; no global installation,
npm login or administrator access is needed.

Selecting `LFM3-Audio-2B` downloads about 1.42 GB of verified assets on first
use and asks for microphone access. Inference stays local. Speak to chat or interrupt; Esc pauses, Enter
resumes and Ctrl+C exits. Use `--offline` for cached assets only.

Source builds also offer a guided start: run `boubou` without a subcommand to
choose a recommended model or paste a model name/run command. The English menu
uses arrow keys and Enter; Esc goes back and Ctrl+C exits. Recommendations
estimate memory and disk fit; speed varies by chip and other apps. Set
`NO_COLOR=1` for uncolored menus. Redirected I/O and `TERM=dumb` print help.
This menu and the unified `boubou` command will ship in the next npm release;
the published beta still uses the explicit-model command above.

Models and runtime downloads are kept in `~/Library/Caches/boubou`.
Use `list` to see supported model IDs and `doctor [MODEL]` to check cached readiness.
Without a model ID, `doctor` checks `LFM3-Audio-2B`.

The [first MVP](spec/0001-liquid-macos-product.md) is accepted.
Signing and notarization remain deferred.

## Develop

Requires Rust 1.88+, Xcode Command Line Tools, Python 3.9+ and Node.js/npm.
Build from a clean commit:

```sh
cargo build --locked
python3 packaging/build-macos.py
```

The npm tarball, checksum and provenance are under `target/dist/<commit>/`.
See [checks](.github/workflows/quality.yml) and [contribution rules](AGENTS.md).
Model: [LFM Open License](https://www.liquid.ai/lfm-license).
Backend: [llama.cpp MIT](https://github.com/ggml-org/llama.cpp/blob/b11960/LICENSE).
Full upstream license texts are verified during packaging and included in the package.
