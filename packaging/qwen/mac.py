"""Run the pinned HF conversation on a Mac, with the models any pipeline options name, and the page
that clones a voice."""

import importlib.util
import json
import os
import re
import sys
import types
from pathlib import Path

HERE = Path(__file__).resolve().parent
# voice.py stays the conversation both hosts share, so the published Linux runner stays current.
_spec = importlib.util.spec_from_file_location("voice", HERE / "voice.py")
voice = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(voice)

# Boubou opens the Hub only for pipeline options, which may name any model. The Hub client keeps
# those in the person's own cache, found before voice.run() moves XDG_CACHE_HOME into temporary storage.
HOME = os.environ.get("HF_HOME") or os.path.join(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache", "huggingface")
HUB = {"HF_HOME": HOME, "HF_HUB_CACHE": os.environ.get("HF_HUB_CACHE") or os.path.join(HOME, "hub"),
       "HF_HUB_OFFLINE": "0"} if os.environ.get("HF_HUB_OFFLINE") == "0" else {}
pinned = voice.arguments
one_voice = voice.read_aloud

# Voices cloned on this Mac: <name>.wav, a recording, and <name>.json, what it says. Boubou names the
# folder once it has verified the Base weights of the profile BOUBOU_CLONER, which read them, and
# BOUBOU_VOICE when a cloned voice speaks first. The clone page may choose another folder.
CLONES, CLONER, OWN = (os.environ.get(name) for name in ("BOUBOU_VOICES", "BOUBOU_CLONER", "BOUBOU_VOICE"))
NAME = re.compile(r"[a-z0-9][a-z0-9_-]{0,31}")
# Qwen3-TTS CustomVoice's own speakers keep their names.
SPEAKERS = {"aiden", "ryan", "ono_anna", "sohee", "vivian", "serena", "uncle_fu", "dylan", "eric"}
LANGUAGES = {"en", "ja", "zh", "ko", "es", "fr", "de", "pt", "it", "ru"}
# Qwen3-TTS weights by kind, "base" or "custom_voice": a path until a reading first needs them.
weights = {}


def cloned(name):
    """The recording and words of the voice cloned as `name`, if there is one."""
    name = str(name or "").lower()
    folder = Path(CLONES or "")
    if not (CLONES and NAME.fullmatch(name) and (folder / f"{name}.json").is_file()
            and (folder / f"{name}.wav").is_file()):
        return None
    return str((folder / f"{name}.wav").resolve()), json.loads((folder / f"{name}.json").read_text())["text"]


def snapshots(manifest, profile):
    """The pinned Hub snapshots that `profile` lists, by repository."""
    found = {}
    for entry in json.loads(Path(manifest).read_text()):
        parts = Path(entry["path"]).parts
        if profile in entry.get("profiles", ()) and parts[0] == "hub" and len(parts) > 4:
            found[parts[1].split("--")[-1]] = Path(*parts[:4])
    return found


def local_kokoro_voices(model):
    """Find each Kokoro voice upstream names among the verified files in `model`, not on the Hub.

    Upstream names the voice --voice chooses, and the voice it switches to for a reply in another
    language. MLX Audio would look for either in a Hub repository that Boubou does not download.
    """
    from mlx_audio.tts.models.kokoro.pipeline import KokoroPipeline

    load = KokoroPipeline.load_single_voice

    def local(self, voice):
        path = model / "voices" / f"{voice}.safetensors"
        return load(self, str(path) if path.is_file() else voice)

    KokoroPipeline.load_single_voice = local


def read_japanese():
    """Let misaki 0.9.4 read Japanese for Kokoro without pyopenjtalk.

    misaki.ja imports pyopenjtalk for a reading Kokoro never asks for: Kokoro's reads with fugashi
    and unidic-lite. pyopenjtalk publishes no wheel, and the fork that does needs a 77 MB dictionary.
    """
    sys.modules.setdefault("pyopenjtalk", types.ModuleType("pyopenjtalk"))


def keep_marks():
    """Keep the vowel signs of Hindi, and every other combining mark, in the replies a voice speaks.

    Upstream drops what its pattern calls unspeakable, one character at a time, keeping `\\w`. Python's
    `\\w` has no combining marks, so "फ्रांस की राजधानी" reached Kokoro as "फरस क रजधन".
    """
    import unicodedata

    from speech_to_speech.LLM import utils

    unspeakable = utils.SPEECHABLE_PATTERN
    if not isinstance(unspeakable, re.Pattern):
        return

    class Speakable:
        def sub(self, replacement, text):
            return unspeakable.sub(
                lambda match: match[0] if unicodedata.category(match[0]).startswith("M") else replacement, text)

    utils.SPEECHABLE_PATTERN = Speakable()


def arguments(model_id, cache, manifest, work):
    keep_marks()
    options = pinned(model_id, cache, manifest, work)
    if "--kokoro_model_name" in options:
        local_kokoro_voices(Path(options[options.index("--kokoro_model_name") + 1]))
        read_japanese()
    os.environ.update(HUB)
    if CLONER:
        (base,) = (path for name, path in snapshots(manifest, CLONER).items() if name.startswith("Qwen3-TTS"))
        at = options.index("--qwen3_tts_model_name") + 1
        weights.update(custom_voice=options[at], base=str(Path(cache).resolve() / base))
        # A cloned voice that speaks first starts with the Base weights, as upstream clones.
        if own := cloned(OWN):
            options[at] = weights["base"]
            options += ["--qwen3_tts_ref_audio", own[0], "--qwen3_tts_ref_text", own[1]]
    return options


def read_aloud():
    """Let a text to read aloud name who reads it: `voice`, one of Qwen3-TTS's speakers or a voice
    cloned on this Mac.

    Any other name, such as OpenAI's own, keeps the voice Boubou started with, as every other
    model does. voice.py reads in that voice only, and stays as the Linux runner published it.
    """
    import threading

    from mlx_audio.tts.utils import load_model
    from speech_to_speech.api.openai_realtime import server

    one_voice()
    create_app = server.create_app

    def with_voices(pool, **options):
        app = create_app(pool=pool, **options)
        # Qwen3-TTS, the one voice with speakers to choose from.
        reader = next((handler for handler in pool[0].handlers if hasattr(handler, "_initial_speaker")), None)
        if reader is None:
            return app
        # Base weights, which a cloned voice starts with, have no speakers of their own.
        speakers = {name.lower(): name for name in reader._supported_speakers() or (CLONER and SPEAKERS) or ()}
        speaker = OWN if cloned(OWN) else reader._initial_speaker
        own, process, turn = (speaker, reader.language), reader.process, threading.Lock()
        # A named speaker reads a text in the text's language, as the Whisper entries speak, unless
        # --language named one other than English: Qwen3-TTS's own guess garbles German, for one.
        reading = "auto" if str(reader.language).lower() in ("english", "auto") else reader.language
        weights[reader._model_type()] = reader.model
        asked = None
        route = next(route for route in app.routes if getattr(route, "path", None) == "/v1/audio/speech")
        app.router.routes.remove(route)

        @app.post(route.path)
        def speech(request: dict):
            nonlocal asked
            with turn:
                name = str(request.get("voice") or "").lower()
                asked = speakers.get(name) or (name if cloned(name) else None)
                try:
                    return route.endpoint(request)
                finally:
                    asked = None

        def named(sentence):
            # Upstream keeps a speaker until a conversation ends, and a reading is none.
            reader.speaker, reader.language = (asked, reading) if asked else own
            clone = cloned(reader.speaker)
            if CLONER:
                kind = "base" if clone else "custom_voice"
                if isinstance(weights[kind], str):
                    weights[kind] = load_model(weights[kind])
                reader.model = weights[kind]
            reader.ref_audio, reader.ref_text = clone or (None, reader.ref_text)
            if clone:
                # The recording is kept as Qwen3-TTS hears it. Read it afresh, as it may be recorded again.
                reader._mlx_ref_audio_cache[clone[0]] = clone[0]
            return process(sentence)

        reader.process = named
        return app

    server.create_app = with_voices


# run() builds the pipeline from voice.py's module-level names.
voice.arguments, voice.read_aloud = arguments, read_aloud
run = voice.run


def clone(profile, cache, manifest, *extra):
    """Serve the page that clones a voice: from a recording of a sentence the page shows, or from a
    clip whose words Whisper writes down. Each voice stays in BOUBOU_VOICES, on this Mac.

    `boubou voices`, at BOUBOU_BIN, keeps the folder: it lists it, moves it, exports and imports.
    This writes down what each recording dropped into the folder says."""
    import argparse
    import io
    import secrets
    import subprocess
    import threading
    from tempfile import TemporaryDirectory

    import numpy as np
    import soundfile
    import uvicorn
    from fastapi import FastAPI, Form, HTTPException, Request, UploadFile
    from fastapi.responses import HTMLResponse, Response
    from mlx_audio.stt.generate import load_model as ears
    from mlx_audio.tts.utils import load_model as mouth
    from scipy.signal import resample_poly

    address = argparse.ArgumentParser()
    address.add_argument("--host")
    address.add_argument("--port", type=int)
    address = address.parse_args(extra)
    # Links made in temporary storage must not point at a cache named relative to here.
    cache, key, mlx = Path(cache).resolve(), os.environ["BOUBOU_KEY"], threading.Lock()
    # Why each recording dropped into the folder has no words yet, by its path, size and time.
    unheard = {}
    found = snapshots(manifest, profile)
    with TemporaryDirectory(prefix="boubou-clone-") as work:
        os.environ.update(HF_HUB_OFFLINE="1", HF_HUB_DISABLE_TELEMETRY="1", XDG_CACHE_HOME=f"{work}/cache")
        # MLX's Whisper conversion omits its processor, which the original snapshot holds.
        whisper = Path(work, "whisper")
        whisper.mkdir()
        for name in ("whisper-large-v3-turbo", "whisper-large-v3"):
            for path in (cache / found[name]).iterdir():
                if not (whisper / path.name).exists():
                    (whisper / path.name).symlink_to(path)
        hear = ears(str(whisper))
        (base,) = (path for name, path in found.items() if name.startswith("Qwen3-TTS"))
        speak = mouth(str(cache / base))
        page = (HERE / "clone.html").read_text()
        app = FastAPI(docs_url=None, redoc_url=None, openapi_url=None)

        @app.middleware("http")
        async def keyed(request: Request, call_next):
            # The page is public; everything it does needs the key in the link Boubou opened.
            sent = request.headers.get("authorization", "").encode()
            if request.url.path != "/" and not secrets.compare_digest(sent, f"Bearer {key}".encode()):
                return Response(status_code=403)
            return await call_next(request)

        def samples(clip, rate):
            try:
                audio, given = soundfile.read(io.BytesIO(clip), dtype="float32", always_2d=True)
            except RuntimeError:
                raise HTTPException(400, "Send the voice as a WAV file.") from None
            return resample_poly(audio.mean(axis=1), rate, given).astype(np.float32)

        def wav(audio, rate):
            file = io.BytesIO()
            soundfile.write(file, audio, rate, format="WAV", subtype="PCM_16")
            return file.getvalue()

        def keep(path, content):
            # Whole or not at all: the folder's listing skips hidden files.
            (path.parent / f".{path.name}").write_bytes(content)
            os.replace(path.parent / f".{path.name}", path)

        def library(*options):
            """The folder and its voices, each with the commands that use it, after `options`."""
            done = subprocess.run([os.environ["BOUBOU_BIN"], "--voices-dir", CLONES, "voices", "--json", *options],
                                  capture_output=True, text=True)
            if done.returncode:
                said = [line for line in done.stderr.splitlines() if line.strip()] or ["Boubou could not do that."]
                raise HTTPException(400, said[-1].removeprefix("boubou: "))
            return json.loads(done.stdout)

        def write_down(path):
            """Write down what a recording dropped into the folder says, or say why not."""
            try:
                clip = samples(path.read_bytes(), 16000)
            except (HTTPException, OSError):
                return "Boubou cannot read this WAV file. Export it again as a 16-bit WAV."
            if np.abs(clip - clip.mean()).max() < 0.01:
                return "Boubou heard no voice in it."
            with mlx:
                heard = hear.generate(clip, verbose=False)
            text, language = " ".join(heard.text.split()), getattr(heard, "language", None)
            if language not in LANGUAGES:
                return "Boubou cannot tell its language, or Qwen3-TTS does not speak it. Upload it on this page instead."
            if not text:
                return "Boubou heard no words in it."
            keep(path.with_suffix(".json"), json.dumps({"text": text, "language": language}).encode())

        @app.get("/")
        def home():
            return HTMLResponse(page, headers={"Cache-Control": "no-store"})

        @app.get("/voices")
        def voices():
            listed, waiting = library(), []
            for name in listed["unheard"]:
                path = Path(CLONES, f"{name}.wav")
                seen = (str(path), path.stat().st_size, path.stat().st_mtime_ns)
                if seen not in unheard:
                    unheard[seen] = write_down(path)
                if unheard[seen]:
                    waiting.append({"file": path.name, "hint": unheard[seen]})
            if len(waiting) < len(listed["unheard"]):
                listed = library()
            listed["attention"] += waiting
            return listed

        @app.post("/folder")
        def choose():
            """Keep voices in a folder the person picks in macOS's own window: a page cannot name one."""
            global CLONES
            picked = subprocess.run(
                ["osascript", "-e", "on run argv", "-e", "activate", "-e",
                 'POSIX path of (choose folder with prompt "Choose the folder that keeps your voices." '
                 "default location (POSIX file (item 1 of argv) as alias))", "-e", "end run", CLONES],
                capture_output=True, text=True)
            # Cancelled.
            if picked.returncode:
                return library()
            listed = library("--folder", picked.stdout.strip())
            CLONES = listed["folder"]
            return listed

        @app.post("/folder/open")
        def show(name: str = ""):
            # Finder opens the folder, with a voice's launcher selected when one is named.
            launcher = Path(CLONES, f"{name}.command")
            if NAME.fullmatch(name) and launcher.is_file():
                subprocess.run(["open", "-R", str(launcher)], check=False)
            else:
                subprocess.run(["open", CLONES], check=False)
            return Response(status_code=204)

        @app.get("/export")
        def export(name: str = ""):
            if name and not NAME.fullmatch(name):
                raise HTTPException(404, "No voice has that name.")
            with TemporaryDirectory(prefix="boubou-export-") as temp:
                archive = Path(temp, "voices.zip")
                library("--export", str(archive), *([name] if name else []))
                content = archive.read_bytes()
            named = f"{name or 'boubou-voices'}.zip"
            return Response(content, media_type="application/zip",
                            headers={"Content-Disposition": f'attachment; filename="{named}"'})

        @app.post("/import")
        async def take(archive: UploadFile):
            with TemporaryDirectory(prefix="boubou-import-") as temp:
                path = Path(temp, "voices.zip")
                path.write_bytes(await archive.read())
                return library("--import", str(path))

        @app.post("/transcribe")
        async def transcribe(audio: UploadFile):
            clip = samples(await audio.read(), 16000)
            with mlx:
                heard = hear.generate(clip, verbose=False)
            language = getattr(heard, "language", None)
            return {"text": heard.text.strip(), "language": language if language in LANGUAGES else ""}

        @app.post("/voices")
        async def save(audio: UploadFile, name: str = Form(), text: str = Form(), language: str = Form(),
                       consent: str = Form("")):
            name, text = name.strip().lower(), " ".join(text.split())
            if consent != "yes":
                raise HTTPException(400, "Confirm the voice is yours, or that you may use it.")
            if not NAME.fullmatch(name) or name in SPEAKERS:
                raise HTTPException(400, "Choose another name: lowercase letters and digits, not a built-in voice.")
            if not 0 < len(text) <= 500 or language not in LANGUAGES:
                raise HTTPException(400, "Write what the voice says.")
            clip = samples(await audio.read(), speak.sample_rate)
            if not 3 <= len(clip) / speak.sample_rate <= 30:
                raise HTTPException(400, "Give 3 to 30 seconds of voice.")
            if np.abs(clip - clip.mean()).max() < 0.01:
                raise HTTPException(400, "Boubou heard no voice. Check the microphone or the clip, then try again.")
            # The words last: a voice is listed once they are there.
            keep(Path(CLONES, f"{name}.wav"), wav(clip, speak.sample_rate))
            keep(Path(CLONES, f"{name}.json"), json.dumps({"text": text, "language": language}).encode())
            return {"name": name}

        @app.delete("/voices/{name}")
        def delete(name: str):
            if not NAME.fullmatch(name):
                raise HTTPException(404)
            for suffix in (".json", ".wav"):
                Path(CLONES, f"{name}{suffix}").unlink(missing_ok=True)
            return Response(status_code=204)

        @app.post("/speech")
        def speech(request: dict):
            clip, text = cloned(request.get("voice")) or (None, None)
            if clip is None or not str(request.get("input", "")).strip():
                raise HTTPException(404, "No voice has that name.")
            with mlx:
                parts = [np.array(part.audio) for part in speak.generate(
                    text=str(request["input"]), ref_audio=clip, ref_text=text, lang_code="auto")]
            return Response(wav(np.concatenate(parts), speak.sample_rate), media_type="audio/wav")

        uvicorn.run(app, host=address.host, port=address.port, log_level="warning")


if __name__ == "__main__":
    # Boubou owns the microphone and speakers; the pipeline serves the Realtime API on loopback.
    if sys.argv[1] == "clone":
        clone(*sys.argv[2:])
    else:
        run("serve", *sys.argv[1:])
