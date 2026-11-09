# Boubou

Run voice AI locally with open-source models. Bring it to any device with one command.

Boubou is a command-line tool for talking with an AI out loud. Speech
recognition, the language model and speech synthesis all run on your own
machine, so your voice never leaves it. Talk over a reply and Boubou stops to
listen, the way a person would.

## Quick start

You need an Apple Silicon Mac with macOS 13.3 or later and Node.js 18 or later.

```sh
npx boubou@beta run LFM3-Audio-2B
```

The first run downloads about 1.4 GB and asks for microphone access. After
that, just talk. Pause and Boubou answers. Start speaking and it stops.
Apple's echo cancellation keeps Boubou from hearing itself, so headphones are
optional.

| Key | Action |
| --- | --- |
| Esc | Pause listening |
| Enter | Resume |
| Ctrl+C | Quit |

## Models

| Model | What it does | Download |
| --- | --- | --- |
| `LFM3-Audio-2B` | English voice chat. The default. | 1.4 GB |
| `LFM3-Omni-2B` | English voice chat, plus text questions about images and short videos | 1.6 GB |
| `LFM2.5-Audio-1.5B` | Smaller English voice chat model. Interrupting it takes longer, because Boubou has to restart its server. | 1.3 GB |

All three are [Liquid AI](https://huggingface.co/LiquidAI) models served by
[llama.cpp](https://github.com/ggml-org/llama.cpp).

Boubou also has a modular pipeline: Parakeet or Whisper for speech
recognition, Qwen3.5 for replies, and a choice of voices including Qwen3-TTS,
Kokoro, Pocket TTS and Supertonic. It runs in a prebuilt Python runtime on
macOS 15 or on x86-64 Linux with an NVIDIA GPU, and is not published yet. To
build it yourself, see [packaging/qwen](packaging/qwen/README.md).

## Usage

The published beta only includes `run LFM3-Audio-2B`. Everything below works
from a [source build](#build-from-source) and ships in the next release
([#5](https://github.com/runboubou/boubou/issues/5)).

```sh
boubou                          # check this machine and pick a model from a menu
boubou list                     # list model IDs
boubou pull LFM3-Omni-2B        # download and verify ahead of time
boubou run LFM3-Omni-2B --offline
boubou doctor LFM3-Omni-2B      # verify cached files without the network
```

`run --manual` switches to push-to-talk: press Enter to start and send a turn.
`run --input turn.wav` speaks from 16 kHz mono PCM WAV files instead of the
microphone, and `--output reply.wav` saves the spoken replies.

`LFM3-Omni-2B` can answer a question about a picture or a short clip:

```sh
boubou ask LFM3-Omni-2B photo.png "What is in this picture?"
boubou ask LFM3-Omni-2B clip.mp4 "What happens, and what can you hear?"
```

Images can be PNG or JPEG up to 2048 × 2048. Videos can be MP4 or MOV with a
constant frame rate, up to 10 seconds, 512 × 512 and 60 fps, and need
`ffmpeg` and `ffprobe` installed. Files are limited to 64 MiB.

## Linux

The three Liquid models also run on x86-64 and arm64 Linux with glibc 2.38 or
later (for example Ubuntu 24.04 or Debian 13), OpenSSL 3 and `libgomp`.
[Build from source](#build-from-source), then talk:

```sh
boubou run LFM3-Audio-2B
```

It works as on a Mac: pause and Boubou answers, start speaking and it stops.
Boubou uses the default microphone and speakers, usually through PipeWire or
PulseAudio, and cancels its own echo with WebRTC's AEC3, so headphones are
optional. Interrupting starts to work two seconds into the first reply, once
Boubou has learned its echo. `--manual`, `--input` and `ask` work as on a Mac.

Linux uses llama.cpp's portable CPU builds for the Liquid models. The menu
still needs a Mac.

### Modular pipeline on an NVIDIA GPU

The modular pipeline also runs on x86-64 Linux with an NVIDIA GPU, with every
voice except Qwen3-TTS and the Whisper entries. You need:

- An NVIDIA GPU from the Turing generation (RTX 20 series) or newer, with about
  7.5 GB of free memory, or 8.5 GB for OmniVoice.
- NVIDIA driver 580.65.06 or later. The runtime brings its own CUDA 13
  libraries, so no CUDA toolkit is needed.
- glibc 2.39 or later and OpenSSL 3, for example Ubuntu 24.04.
- About 10 GB of disk for the runtime, plus 5.3 GB of models shared by every
  voice and 0.02 to 3.3 GB for each voice.

Build the runtime as described in [packaging/qwen](packaging/qwen/README.md#x86-64-linux-with-an-nvidia-gpu),
then talk:

```sh
boubou pull kokoro:82m
boubou run kokoro:82m --offline
```

On the test machine, the first `pull kokoro:82m` took 87 s at about 110 MB/s.
Each later start verifies every file in about 2 s and is listening after about
15 s.

## Privacy

- Inference runs on your machine. Boubou does not upload audio, transcripts or
  replies, and does not save them unless you pass `--output`.
- The network is only used to download pinned model and runtime files from
  Hugging Face and GitHub. Each file must match a SHA-256 hash before it is
  used. After that, `--offline` works with no network at all.
- There is no telemetry, no account and no API key.

## Storage

Models and runtimes live in `~/Library/Caches/boubou`, or `~/.cache/boubou` on
Linux. Set `BOUBOU_HOME` or pass `--cache-dir` to use another folder.
Interrupted downloads resume where they stopped. To remove everything, delete
the folder.

## How it works

```
microphone ──► audio helper ──► boubou ──► model server
speakers   ◄── audio helper ◄── boubou ◄── model server
```

`boubou` is a Rust program that starts two child processes. A small native
audio helper uses Apple's voice processing for echo cancellation and speech
detection, and streams 20 ms frames over a pipe. On Linux, Boubou records and
plays through the default ALSA devices itself, with WebRTC echo cancellation and
neural speech detection in-process. A model server, either
llama.cpp or the bundled Python runtime, listens on a loopback port. When you
speak during a reply, Boubou drops the queued audio and cancels the
generation. When the conversation fills the model's context, it starts a
fresh session in the background.

## Build from source

You need Rust 1.91 or later, plus the Xcode Command Line Tools on a Mac. On
Linux, you need a C compiler, `pkg-config` and the ALSA headers (`libasound2-dev`
on Ubuntu).

```sh
git clone https://github.com/runboubou/boubou.git
cd boubou
cargo build --locked --release
./target/release/boubou
```

To build the npm package from a clean checkout, run
`python3 packaging/build-macos.py`. It writes the tarball, checksums and build
provenance to `target/dist/<commit>/`.

## Contributing

Bug reports, device reports and model requests are welcome. Please read
[CONTRIBUTING.md](CONTRIBUTING.md) first. Report security issues privately as
described in [SECURITY.md](SECURITY.md).

Boubou runs on Apple Silicon Macs and Linux. Another platform needs an audio
backend with echo cancellation; open an issue before starting on one.

## Contributors

Thanks to everyone who contributes to Boubou.

<a href="https://github.com/runboubou/boubou/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=runboubou/boubou&amp;max=1000&amp;columns=12" alt="Boubou GitHub contributors" />
</a>

[View GitHub contributors](https://github.com/runboubou/boubou/graphs/contributors) · [Contribute to Boubou](CONTRIBUTING.md)

## License

Boubou's code is released under the [MIT License](LICENSE).

Models and inference backends keep their own licenses. Liquid models use the
[LFM Open License](https://www.liquid.ai/lfm-license), and llama.cpp is MIT.
Some optional voices in the Python runtime, including MMS, OmniVoice and
ChatTTS, are licensed for non-commercial use only. Check the model card
before using a model commercially.
