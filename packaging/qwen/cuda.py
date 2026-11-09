"""Run the pinned HF conversation on an NVIDIA GPU, using only assets prepared by Boubou."""

import ctypes
import importlib.util
import json
import os
import signal
import socket
import subprocess
import sys
import sysconfig
import threading
import time
from pathlib import Path
from urllib.error import URLError
from urllib.request import ProxyHandler, build_opener

HERE = Path(__file__).resolve().parent
# The macOS entry owns process setup, the chosen voices and the SDK fixes. Only engines and weights differ.
_spec = importlib.util.spec_from_file_location("voice", HERE / "voice.py")
voice = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(voice)

PARAKEET = "models--nvidia--parakeet-tdt-0.6b-v3"
LLM = "models--unsloth--Qwen3.5-4B-GGUF"

TTS = {
    **voice.TTS,
    # The PyTorch package loads the model by repository name and the voice from a file.
    "kokoro": lambda path: ["--tts", "kokoro", "--kokoro_device", "cuda", "--kokoro_lang_code", "a",
                            "--kokoro_voice", str(Path(path, "voices/af_heart.pt"))],
    "omnivoice": lambda path: ["--tts", "omnivoice", "--omnivoice_model_name", path,
                               "--omnivoice_device", "cuda", "--omnivoice_language", "en",
                               "--omnivoice_voice_clone_prompt", str(voice.VOICES / "omnivoice.prompt")],
}


def llama_server(model, work):
    """Serve the language model with llama.cpp, upstream's lighter CUDA setup, and return the pipeline's LLM options."""
    with socket.socket() as reserved:
        reserved.bind(("127.0.0.1", 0))
        port = reserved.getsockname()[1]
    libc = ctypes.CDLL(None)
    with (work / "llama-server.log").open("wb") as log:
        server = subprocess.Popen(
            [HERE / "llama-b11960/llama-server", "-m", model, "--alias", "qwen", "--host", "127.0.0.1",
             "--port", str(port), "-ngl", "999", "-c", "32768", "-np", "1", "-fa", "on", "--reasoning", "off",
             "--no-mmproj", "--no-webui", "--offline"],
            # llama.cpp uses the CUDA libraries installed with PyTorch, and stops when this process does.
            env=dict(os.environ, LD_LIBRARY_PATH=str(Path(sysconfig.get_paths()["purelib"], "nvidia/cu13/lib"))),
            stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
            preexec_fn=lambda: libc.prctl(1, signal.SIGKILL),  # PR_SET_PDEATHSIG
        )
    health = build_opener(ProxyHandler({}))
    deadline = time.monotonic() + 120
    while True:
        try:
            with health.open(f"http://127.0.0.1:{port}/health", timeout=1):
                break
        except (URLError, ConnectionError):
            if server.poll() is not None or time.monotonic() > deadline:
                server.kill()
                tail = (work / "llama-server.log").read_text(errors="replace")[-2000:]
                raise RuntimeError(f"llama-server did not start (exit {server.poll()}):\n{tail}") from None
            time.sleep(0.1)
    return ["--llm_backend", "responses-api", "--model_name", "qwen",
            "--responses_api_base_url", f"http://127.0.0.1:{port}/v1", "--responses_api_api_key", ""]


def warm_librosa():
    # ChatTTS and MMS call librosa.resample for every reply. Its first call compiles for about
    # nine seconds, so compile it while the models load rather than during the first reply.
    import librosa
    import numpy

    librosa.resample(numpy.zeros(2400, numpy.float32), orig_sr=24000, target_sr=16000)


def arguments(model_id, cache, manifest, work):
    cache = Path(cache).resolve()
    # Upstream's CUDA loaders find Parakeet and Kokoro by repository name. A private Hub cache names
    # each pinned snapshot as the current revision, so they load offline and the verified cache stays as is.
    hub = work / "hub"
    owned = []
    for entry in json.loads(Path(manifest).read_text()):
        parts = Path(entry["path"]).parts
        if model_id not in entry["profiles"] or parts[0] != "hub":
            continue
        snapshot = cache.joinpath(*parts[:4])
        link = hub.joinpath(*parts[1:4])
        if not link.exists():
            link.parent.mkdir(parents=True, exist_ok=True)
            link.symlink_to(snapshot, target_is_directory=True)
            (hub / parts[1] / "refs").mkdir(exist_ok=True)
            (hub / parts[1] / "refs/main").write_text(parts[3])
        if parts[1] == LLM and parts[-1].endswith(".gguf"):
            model = cache / entry["path"]
        if parts[1] in (PARAKEET, LLM):
            continue
        if parts[4:] == ("config.json",):
            owned.append(str(snapshot))
        elif parts[4:] == ("asset", "gpt", "config.json"):
            # Keep ChatTTS's working directory temporary, so nothing is written into the verified snapshot.
            (work / "asset").symlink_to(cache.joinpath(*parts[:5]), target_is_directory=True)
            owned.append(str(work))
        elif parts[-2] == "embeddings":
            owned.append(str(cache / entry["path"]))
    (tts,) = owned
    family = model_id.split(":")[0]
    if family in ("chattts", "mms-tts"):
        threading.Thread(target=warm_librosa, daemon=True).start()
    os.environ.update({
        "HF_HOME": str(work / "hf"),
        "HF_HUB_CACHE": str(hub),
        "HF_HUB_OFFLINE": "1",
        "HF_HUB_DISABLE_TELEMETRY": "1",
        "NLTK_DATA": str(HERE / "nltk_data"),
    })
    return [
        "--vad", "silero", "--smart_turn_model_path", str(cache / "smart-turn/smart-turn-v3.2-cpu.onnx"),
        "--stt", "parakeet-tdt", "--parakeet_tdt_device", "cuda",
        *llama_server(model, work),
        *TTS[family](tts),
    ]


# run() builds the pipeline from voice.py's module-level mapping.
voice.arguments = arguments
run = voice.run

if __name__ == "__main__":
    # Boubou owns the microphone and speakers; the pipeline serves the Realtime API on loopback.
    run("serve", *sys.argv[1:])
