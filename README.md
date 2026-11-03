# Boubou

Local English voice chat on Apple Silicon Macs with Liquid LFM3-Audio-2B.

## Run

Get the development archive from the maintainer, extract it, then run in its directory:

```sh
sh ./install.sh --run
```

First use downloads about 1.42 GB of verified assets and asks for microphone access. Inference stays local; no developer tools are needed.

Speak to chat or interrupt a reply. `Esc` pauses, `Enter` resumes, `Ctrl+C` exits.

Later: `"$HOME/.local/bin/boubou" run liquid`. Uninstall: `sh ./uninstall.sh`; models are kept. See [package instructions](packaging/README.txt) for offline use and installation paths.

Development candidate: signing, notarization and installation on another Mac are unverified.

## Develop

Requires Rust 1.88+ and Xcode Command Line Tools; packaging also uses Python 3.9+.

```sh
cargo build --locked
python3 packaging/build-macos.py
```

Build packages from a clean commit; packaging fetches verified upstream licenses. See [checks](.github/workflows/quality.yml), [contribution rules](AGENTS.md) and the [MVP contract](spec/0001-liquid-macos-product.md).

Model: [LFM Open License](https://www.liquid.ai/lfm-license). Backend: [llama.cpp MIT](https://github.com/ggml-org/llama.cpp/blob/b11960/LICENSE). Full license texts are included in the package.
