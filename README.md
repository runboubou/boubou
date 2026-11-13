<p align="center">
  <img src="assets/boubou-logo.png" width="104" height="104" alt="Boubou toucan logo" />
</p>

<h1 align="center">Boubou</h1>

<p align="center">
  <strong>Talk with AI out loud, on your own machine.</strong><br />
  Open voice models. One command. No account.
</p>

<p align="center">
  <a href="https://runboubou.com/"><img src="https://img.shields.io/badge/Website-runboubou.com-141414?style=flat-square" alt="Website" /></a>
  <a href="https://discord.gg/SPz5v4Qf86"><img src="https://img.shields.io/badge/Discord-Join-5865F2?style=flat-square&amp;logo=discord&amp;logoColor=white" alt="Discord" /></a>
  <a href="https://x.com/RunBoubou"><img src="https://img.shields.io/badge/X-Follow-181717?style=flat-square&amp;logo=x&amp;logoColor=white" alt="X" /></a>
</p>

<p align="center">
  <a href="https://runboubou.com/"><strong>runboubou.com</strong></a> · <a href="https://runboubou.com/models/">Models</a> · <a href="https://runboubou.com/connect/">Phones and robots</a>
</p>

<p align="center">
  <img src="assets/boubou-banner.svg" width="1040" alt="Boubou — Talk to AI. Keep it on your machine. Open models. One command. A natural conversation." />
</p>

## Quick start

```sh
npx boubou@beta
```

Apple Silicon Mac with macOS 13.3+ and Node.js 18+. Allow microphone access, then speak.

1. **One command.** `npx boubou@beta` opens Boubou in your terminal.
2. **Voices that fit your Mac.** Boubou checks the chip, memory and free disk, and lists only the voices this Mac can run.
3. **Pick a language and a voice.** On [runboubou.com/models](https://runboubou.com/models/), open a model, choose a language and a voice, and copy the command the page writes. Boubou sets up the rest: what hears, what thinks and what speaks. Each page says which Mac it needs.

   ```sh
   npx boubou@beta run qwen3-tts:0.6b-customvoice --voice ono_anna
   ```

4. **Talk from a phone or a robot.** `serve` shares the voice: a phone scans a QR code, a robot or app takes an address and a port. [Guides with pictures](https://runboubou.com/connect/)

   ```sh
   npx boubou@beta serve qwen3-tts:0.6b-customvoice
   ```

<p align="center">
  <img src="assets/terminal-demo.svg" width="1040" alt="Run boubou, choose a local model, speak, hear a reply, and interrupt by speaking again. An illustrative terminal workflow." />
</p>

<p align="center">
  <sub>Illustrative conversation; setup time condensed.</sub><br />
  <kbd>Esc</kbd> pause · <kbd>Enter</kbd> resume · <kbd>Ctrl+C</kbd> quit
</p>

<details>
<summary>Linux and building from source</summary>

The npm command also works on x86-64 Linux with the Liquid models; name one, as the menu needs macOS:

```sh
npx boubou@beta run LFM3-Audio-2B
```

To build from source, you need Rust 1.91+ and Xcode Command Line Tools on macOS. On Linux, install a C compiler, `pkg-config` and ALSA development headers.

```sh
git clone https://github.com/runboubou/boubou.git
cd boubou
cargo build --locked --release
./target/release/boubou run LFM3-Audio-2B
```

On a Mac, `./target/release/boubou` alone opens the menu. Liquid models also run on arm64 Linux built from source. Linux needs glibc 2.38+, OpenSSL 3 and `libgomp`.

</details>

<details>
<summary>More commands</summary>

```sh
npx boubou@beta list
npx boubou@beta pull LFM3-Omni-2B
npx --offline boubou@beta run LFM3-Omni-2B --offline
npx boubou@beta doctor LFM3-Omni-2B
npx boubou@beta ask LFM3-Omni-2B photo.png "What is in this picture?"
```

`serve` prints a browser link and a QR code for a phone on the same Wi-Fi; one client can use the conversation at a time. Keep the printed link private: it contains the access key. `--local` listens only on this computer. By default, `serve` requests a trusted certificate from `lan.runboubou.com`, whose name includes the host's local IP address; its private key stays on the host. `--local`, `--self-signed` or `--offline` avoids that service.

Modular models also expose OpenAI-compatible Realtime and speech endpoints through `serve`. Prefer the printed HTTPS/WSS address; the HTTP/WS fallback is unencrypted. A robot or app that takes only an address and a port connects without the key: `serve` asks in its terminal before letting that address in and remembers the answer.

Each model page on the website also lists its versions and how to [swap a component](https://runboubou.com/models/qwen3-tts/#swap-title) of a modular model. Use `npx boubou@beta --help` or `npx boubou@beta <command> --help` for every option.

</details>

<details>
<summary>Updates and the cache</summary>

The npm launcher checks for updates before a command, at most once an hour. In a terminal it asks before updating to the newest version on your release channel; in scripts it updates without asking. If the check or installation fails, it warns and uses the last available version. `--offline`, `doctor` and `BOUBOU_NO_UPDATE_CHECK=1` skip the check.

Models and runtimes stay in `~/Library/Caches/boubou` on macOS or `~/.cache/boubou` on Linux across updates; set `BOUBOU_HOME` to choose another folder. A runtime update downloads only the files that changed.

</details>

## How it works

Rust connects your microphone and speakers to a local model server, with echo cancellation and speech detection.

- **One audio model:** Liquid models hear, think and speak through llama.cpp.
- **A modular pipeline:** speech recognition → Qwen → your choice of voice. See [models and runtimes](packaging/qwen/README.md).

**Inference runs on your machine.** No account, cloud model API key or telemetry. With `run`, audio stays on this computer. After downloading the models, use `--offline` to run without a network; with npx, pass `--offline` before the package name too. Update checks go through npm to your configured registry and never include audio, transcripts or model files.

## Contributors

Built in the open. Thanks to everyone helping Boubou find its voice.

<a href="https://github.com/runboubou/boubou/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=runboubou/boubou&amp;max=1000&amp;columns=12" alt="Boubou GitHub contributors" />
</a>

[All contributors](https://github.com/runboubou/boubou/graphs/contributors) · [Contribute to Boubou](CONTRIBUTING.md) · [Report a security issue](SECURITY.md)

---

[MIT](LICENSE).
