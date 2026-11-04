"""Run the pinned HF conversation using only assets prepared by Boubou."""

import json
import os
import sys
from pathlib import Path
from tempfile import TemporaryDirectory


def arguments(model_id, cache, manifest, work):
    cache = Path(cache).resolve()
    bundle = Path(__file__).resolve().parent
    files = json.loads(Path(manifest).read_text())
    snapshots = {}
    for entry in files:
        parts = Path(entry["path"]).parts
        if (entry.get("profile", model_id) == model_id and len(parts) == 5
                and parts[0] == "hub" and parts[2] == "snapshots" and parts[4] == "config.json"):
            # MLX Audio infers the Parakeet architecture from the final path component.
            alias = work / parts[1].removeprefix("models--mlx-community--")
            alias.symlink_to(cache.joinpath(*parts[:4]), target_is_directory=True)
            snapshots[parts[1]] = str(alias)
    (tts,) = (path for name, path in snapshots.items() if name.startswith("models--mlx-community--Qwen3-TTS-"))
    os.environ.update({
        "HF_HOME": str(cache / "hf"),
        "HF_HUB_CACHE": str(cache / "hub"),
        "HF_HUB_OFFLINE": "1",
        "HF_HUB_DISABLE_TELEMETRY": "1",
        "NLTK_DATA": str(bundle / "nltk_data"),
    })
    return [
        "--mac-optimal-settings",
        "--vad", "silero", "--smart_turn_model_path", str(cache / "smart-turn/smart-turn-v3.2-cpu.onnx"),
        "--stt", "parakeet-tdt", "--parakeet_tdt_model_name", snapshots["models--mlx-community--parakeet-tdt-0.6b-v3"],
        "--llm_backend", "mlx-lm", "--model_name", snapshots["models--mlx-community--Qwen3.5-4B-4bit"],
        "--tts", "qwen3", "--qwen3_tts_model_name", tts, "--qwen3_tts_speaker", "Aiden",
        "--qwen3_tts_language", "english",
    ]


def run(command, model, cache, manifest, *extra):
    # Keep model-name aliases and compiled caches outside the verified package.
    with TemporaryDirectory(prefix="boubou-qwen-") as work:
        work = Path(work)
        hub = work / "torch/hub"
        hub.mkdir(parents=True)
        silero = "snakers4_silero-vad_master"
        (hub / silero).symlink_to(Path(__file__).resolve().parent / "silero", target_is_directory=True)
        (hub / "trusted_list").write_text("snakers4_silero-vad\n")
        os.environ.update(
            TORCH_HOME=str(hub.parent), XDG_CACHE_HOME=str(work / "cache"),
            NUMBA_CACHE_DIR=str(work / "numba"), PYTHONDONTWRITEBYTECODE="1",
            PYTHONPYCACHEPREFIX=str(work / "bytecode"),
        )
        sys.pycache_prefix = os.environ["PYTHONPYCACHEPREFIX"]
        # Loopback connections must bypass macOS system proxies.
        for key in ("NO_PROXY", "no_proxy"):
            os.environ[key] = ",".join(
                filter(None, (os.environ.get(key), "127.0.0.1,localhost,::1"))
            )
        options = arguments(model, cache, manifest, work)
        from speech_to_speech.s2s_pipeline import run_pipeline_command
        # Upstream v1.1.0 sets this to its package directory during import.
        os.environ["TORCHINDUCTOR_CACHE_DIR"] = str(work / "inductor")
        run_pipeline_command(command, [*options, *extra])


if __name__ == "__main__":
    # Boubou owns the microphone and speakers; the pipeline serves the Realtime API on loopback.
    run("serve", *sys.argv[1:])
