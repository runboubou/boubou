"""Run the pinned HF conversation using only assets prepared by Boubou."""

import json
import os
import re
import sys
from pathlib import Path
from tempfile import TemporaryDirectory

# The upstream v1.1.0 default system prompt.
PROMPT = ("You are a helpful and friendly AI assistant. You are polite, respectful, "
          "and aim to provide concise responses of less than 20 words.")
# Voices chosen by listening, so a model without preset speakers keeps one voice across
# sentences and restarts. Upstream would sample or design a new voice instead.
VOICES = Path(__file__).resolve().parent / "voices"


def pocket(voice):
    # The voice state sits at languages/<config>/embeddings/<voice>.safetensors.
    config = Path(voice).parent.parent.name
    # Each stock config speaks one language, named first: french_24l is French.
    language = config.split("_")[0].capitalize()
    return ["--tts", "pocket", "--pocket_tts_language", config, "--pocket_tts_voice", voice,
            "--init_chat_prompt", f"{PROMPT} Always reply in {language}."]


def supertonic(path):
    # The upstream handler loads the SDK's default model from this directory, so nothing is downloaded.
    os.environ["SUPERTONIC_CACHE_DIR"] = path
    return ["--tts", "supertonic", "--supertonic_tts_voice", "M1", "--supertonic_tts_lang", "en"]


def spell_numbers(text):
    from num2words import num2words

    def words(match):
        number = match[0].replace(",", "")
        return num2words(float(number) if "." in number else int(number)).replace("-", " ")
    return re.sub(r"\d+(?:,\d{3})*(?:\.\d+)?", words, text).replace("%", " percent")


def chattts(work):
    import ChatTTS

    speaker = (VOICES / "chattts.txt").read_text().strip()

    class Chat(ChatTTS.Chat):
        # ChatTTS drops digits unless a text normalizer is registered, and the upstream handler registers none.
        def __init__(self, *args, **kwargs):
            super().__init__(*args, **kwargs)
            self.normalizer.register("en", spell_numbers)

        # The upstream handler samples a new speaker at every start. Keep the chosen one,
        # sampled with seed 314 from the pinned weights.
        def sample_random_speaker(self):
            return speaker

    ChatTTS.Chat = Chat
    # The upstream handler loads ChatTTS from asset/ in the working directory, linked here by arguments().
    os.chdir(work)
    # ChatTTS splits a multi-sentence input into a batch, and the handler streams only its first sentence.
    return ["--tts", "chatTTS", "--chat_tts_device", "cpu", "--stream_batch_sentences", "1"]


# Speech options per profile family, given the speech model that profile owns.
TTS = {
    "qwen3-tts": lambda path: ["--tts", "qwen3", "--qwen3_tts_model_name", path, "--qwen3_tts_speaker", "Aiden",
                               "--qwen3_tts_language", "english"],
    # A voice file path keeps MLX Audio from resolving the voice on the Hub.
    "kokoro": lambda path: ["--tts", "kokoro", "--kokoro_model_name", path, "--kokoro_lang_code", "a",
                            "--kokoro_voice", str(Path(path, "voices/af_heart.safetensors"))],
    # 0.1 and 0.2 checkpoints name voices only by ID. On 0.8 this ID is Bruno, the upstream default.
    "kitten-tts": lambda path: ["--tts", "kitten", "--kitten_model_name", path, "--kitten_voice", "expr-voice-3-m"],
    "phonon": lambda path: ["--tts", "phonon", "--phonon_tts_model_name", path, "--phonon_tts_language", "en"],
    "pocket-tts": pocket,
    "supertonic": supertonic,
    "chattts": chattts,
    "mms-tts": lambda path: ["--tts", "facebookMMS", "--facebook_mms_model_name", path,
                              "--facebook_mms_device", "cpu", "--tts_language", "en"],
    # Without a prompt OmniVoice picks a new voice for every sentence. This prompt was designed as
    # "male, young adult, moderate pitch, american accent".
    "omnivoice": lambda path: ["--tts", "omnivoice", "--omnivoice_model_name", path,
                               "--omnivoice_device", "mps", "--omnivoice_language", "en",
                               "--omnivoice_voice_clone_prompt", str(VOICES / "omnivoice.prompt")],
}


def arguments(model_id, cache, manifest, work):
    cache = Path(cache).resolve()
    bundle = Path(__file__).resolve().parent
    files = json.loads(Path(manifest).read_text())
    presets = json.loads((bundle / "runtimes.json").read_text())
    preset = next((preset for preset in presets if preset["id"] == model_id), None)
    tts_id = preset["tts"] if preset else model_id
    multilingual = model_id.endswith("-whisper") or model_id == "chattts:whisper"
    tiny = preset is not None and preset["stt"] == "whisper-tiny"
    recognizer = ("models--openai--whisper-tiny" if tiny else
                  "models--mlx-community--whisper-large-v3-turbo" if multilingual else
                  "models--mlx-community--parakeet-tdt-0.6b-v3")
    llm = "models--mlx-community--" + (preset["llm"] if preset else "Qwen3.5-4B-4bit")
    snapshots, owned = {}, []
    for entry in files:
        if entry.get("profile", model_id) != model_id or model_id not in entry.get("profiles", [model_id]):
            continue
        parts = Path(entry["path"]).parts
        if (len(parts) == 5
                and parts[0] == "hub" and parts[2] == "snapshots" and parts[4] == "config.json"):
            # MLX Audio infers Parakeet and Kokoro architectures from the final path component.
            alias = work / parts[1].removeprefix("models--mlx-community--")
            alias.symlink_to(cache.joinpath(*parts[:4]), target_is_directory=True)
            snapshots[parts[1]] = str(alias)
            if ("profile" in entry or "profiles" in entry) and parts[1] not in (recognizer, llm):
                owned.append(str(alias))
        elif ("profile" in entry or "profiles" in entry) and parts[-2] == "embeddings":
            # Pocket TTS loads its weights by Hub path; its preset voice state is the owned file.
            owned.append(str(cache.joinpath(*parts)))
        elif parts[4:] == ("asset", "gpt", "config.json"):
            # Keep ChatTTS's working directory temporary, so nothing is written into the verified snapshot.
            (work / "asset").symlink_to(cache.joinpath(*parts[:5]), target_is_directory=True)
            owned.append(str(work))
    (tts,) = owned
    os.environ.update({
        "HF_HOME": str(cache / "hf"),
        "HF_HUB_CACHE": str(cache / "hub"),
        "HF_HUB_OFFLINE": "1",
        "HF_HUB_DISABLE_TELEMETRY": "1",
        "NLTK_DATA": str(bundle / "nltk_data"),
    })
    if tiny:
        stt = ["--stt", "whisper", "--stt_model_name", snapshots[recognizer],
               "--stt_device", "mps", "--stt_torch_dtype", "float16",
               "--language", "en", "--stt_gen_task", "transcribe"]
    elif multilingual:
        # MLX's Whisper conversion omits its processor. Combine the two pinned snapshots
        # in temporary storage, keeping the verified cache and installed runtime unchanged.
        whisper = work / "whisper"
        whisper.mkdir()
        for entry in files:
            parts = Path(entry["path"]).parts
            if (len(parts) == 5 and parts[1] in (recognizer, "models--openai--whisper-large-v3")
                    and parts[-1] != "README.md"):
                (whisper / parts[-1]).symlink_to(cache / entry["path"])
        stt = ["--stt", "mlx-audio-whisper", "--mlx_audio_whisper_model_name", str(whisper), "--language", "auto"]
    else:
        stt = ["--stt", "parakeet-tdt", "--parakeet_tdt_model_name", snapshots[recognizer]]
    speech = TTS[tts_id.split(":")[0]](tts)
    if multilingual:
        if model_id.startswith("qwen3-tts:"):
            speech[speech.index("--qwen3_tts_language") + 1] = "auto"
            speech[speech.index("--qwen3_tts_speaker") + 1] = "Vivian"
        speech += ["--init_chat_prompt", f"{PROMPT} Reply in the language of the user's latest message."]
    return [
        "--mac-optimal-settings",
        "--vad", "silero", "--smart_turn_model_path", str(cache / "smart-turn/smart-turn-v3.2-cpu.onnx"),
        *stt,
        "--llm_backend", "mlx-lm", "--model_name", snapshots[llm],
        *speech,
    ]


def link_espeak_data(work):
    # espeak-ng exits when its data path exceeds 159 bytes, as the installed runtime's does.
    # Phonemizer resolves the configured directory, so configure the short parent of a link.
    import espeakng_loader
    from misaki import espeak
    (work / "espeak-ng-data").symlink_to(espeakng_loader.get_data_path(), target_is_directory=True)
    espeak.EspeakWrapper.set_data_path(str(work))


def preserve_qwen_user_turns():
    # SDK 1.1.0 merges unanswered user turns for strict alternating-role templates.
    # Our fixed Qwen LLM accepts separate turns and needs that boundary after barge-in.
    from speech_to_speech.LLM import chat

    def append_user_message(messages, content):
        messages.append(chat.TransformersUserMessage(content=content))

    chat._append_user_message = append_user_message


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
        if model.startswith(("kokoro:", "kitten-tts:")):
            link_espeak_data(work)
        from speech_to_speech.s2s_pipeline import run_pipeline_command
        preserve_qwen_user_turns()
        # Upstream v1.1.0 sets this to its package directory during import.
        os.environ["TORCHINDUCTOR_CACHE_DIR"] = str(work / "inductor")
        run_pipeline_command(command, [*options, *extra])


if __name__ == "__main__":
    # Boubou owns the microphone and speakers; the pipeline serves the Realtime API on loopback.
    run("serve", *sys.argv[1:])
