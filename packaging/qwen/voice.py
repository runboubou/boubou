"""Run the pinned HF conversation using only assets prepared by Boubou."""

import json
import os
import sys
from pathlib import Path


def main():
    model_id, cache, manifest = sys.argv[1:]
    cache = Path(cache).resolve()
    bundle = Path(__file__).resolve().parent
    files = json.loads(Path(manifest).read_text())
    snapshots = {}
    for entry in files:
        parts = Path(entry["path"]).parts
        if entry.get("profile", model_id) == model_id and len(parts) == 5 and parts[0] == "hub" and parts[2] == "snapshots" and parts[4] == "config.json":
            snapshots[parts[1]] = str(cache.joinpath(*parts[:4]))
    tts = next(path for name, path in snapshots.items() if "Qwen3-TTS-" in name)
    os.environ.update({
        "HF_HOME": str(cache / "hf"),
        "HF_HUB_CACHE": str(cache / "hub"),
        "HF_HUB_OFFLINE": "1",
        "HF_HUB_DISABLE_TELEMETRY": "1",
        "NLTK_DATA": str(bundle / "nltk_data"),
        "TORCH_HOME": str(bundle / "torch"),
    })
    # local runs the upstream microphone client and server together over loopback.
    sys.argv = [
        "speech-to-speech", "local", "--mac-optimal-settings",
        "--vad", "silero", "--smart_turn_model_path", str(cache / "smart-turn/smart-turn-v3.2-cpu.onnx"),
        "--stt", "parakeet-tdt", "--parakeet_tdt_model_name", snapshots["models--mlx-community--parakeet-tdt-0.6b-v3"],
        "--llm_backend", "mlx-lm", "--model_name", snapshots["models--mlx-community--Qwen3.5-4B-4bit"],
        "--tts", "qwen3", "--qwen3_tts_model_name", tts, "--qwen3_tts_speaker", "Aiden",
        "--qwen3_tts_language", "english",
    ]
    from speech_to_speech.cli import main as conversation
    conversation()


if __name__ == "__main__":
    main()
