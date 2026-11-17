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

# Voices cloned on this Mac: <name>.wav, a recording, and <name>.json, what it says and its language.
# Boubou names the folder once it has verified the weights that read them: the Qwen3-TTS Base weights
# of the profile BOUBOU_CLONER for a language Qwen3-TTS speaks, VoxCPM2's of BOUBOU_VOXCPM for twenty
# more, and OmniVoice's of BOUBOU_OMNIVOICE for the rest Whisper hears, once the person accepted its
# noncommercial terms. BOUBOU_VOICE names a cloned voice that speaks first. The clone page may choose
# another folder.
CLONES, CLONER, VOXCPM, OMNIVOICE, OWN = (os.environ.get(name) for name in (
    "BOUBOU_VOICES", "BOUBOU_CLONER", "BOUBOU_VOXCPM", "BOUBOU_OMNIVOICE", "BOUBOU_VOICE"))
NAME = re.compile(r"[a-z0-9][a-z0-9_-]{0,31}")
# Qwen3-TTS CustomVoice's own speakers keep their names.
SPEAKERS = {"aiden", "ryan", "ono_anna", "sohee", "vivian", "serena", "uncle_fu", "dylan", "eric"}
# The languages Qwen3-TTS speaks, and the twenty more VoxCPM2 reads cloned voices in. OmniVoice reads
# the other languages Whisper hears.
LANGUAGES = {"en", "ja", "zh", "ko", "es", "fr", "de", "pt", "it", "ru"}
OTHERS = {"nl", "hi", "ar", "my", "da", "fi", "el", "he", "id", "km", "lo", "ms", "no", "pl", "sw", "sv",
          "tl", "th", "tr", "vi"}
# Every language Whisper writes down, the ones OmniVoice reads included.
WHISPER = LANGUAGES | OTHERS | {
    "ca", "uk", "cs", "ro", "hu", "ta", "ur", "hr", "bg", "lt", "la", "mi", "ml", "cy", "sk",
    "te", "fa", "lv", "bn", "sr", "az", "sl", "kn", "et", "mk", "br", "eu", "is", "hy", "ne",
    "mn", "bs", "kk", "sq", "gl", "mr", "pa", "si", "sn", "yo", "so", "af", "oc", "ka", "be",
    "tg", "sd", "gu", "am", "yi", "uz", "fo", "ht", "ps", "tk", "nn", "mt", "sa", "lb", "bo",
    "mg", "as", "tt", "haw", "ln", "ha", "ba", "jw", "su", "yue"
}
# Weights by kind, "base", "custom_voice", "voxcpm" or "omnivoice": a path until a reading first needs
# them.
weights = {}


def cloned(name):
    """The recording, words and language of the voice cloned as `name`, if there is one."""
    name = str(name or "").lower()
    folder = Path(CLONES or "")
    if not (CLONES and NAME.fullmatch(name) and (folder / f"{name}.json").is_file()
            and (folder / f"{name}.wav").is_file()):
        return None
    words = json.loads((folder / f"{name}.json").read_text())
    return str((folder / f"{name}.wav").resolve()), words["text"], words.get("language", "")


class VoxCPMReader:
    """VoxCPM2 behind the calls that read a voice with Qwen3-TTS Base weights, for a voice cloned in a
    language Qwen3-TTS does not speak: it continues from the recording and its words."""

    # The Qwen3-TTS handler tells the kinds of weights apart by their config, and clones with Base.
    config = types.SimpleNamespace(tts_model_type="base")

    def __init__(self, model):
        self.model, self.sample_rate = model, model.sample_rate

    def generate(self, text, ref_audio=None, ref_text=None, **_):
        return self.model.generate(text=text, prompt_audio=ref_audio, prompt_text=ref_text)


class OmniVoiceReader(VoxCPMReader):
    """OmniVoice behind the same calls, for a voice cloned in a language neither Qwen3-TTS nor VoxCPM2
    speaks: it clones from the recording and its words."""

    def __init__(self, model):
        self.model, self.sample_rate = model, model.config.sample_rate

    def generate(self, text, ref_audio=None, ref_text=None, **_):
        return self.model.generate(text=text, ref_audio=ref_audio, ref_text=ref_text)


# The weights that read cloned voices besides the Base weights: their profile, repository and reader.
READERS = {"voxcpm": (VOXCPM, "VoxCPM2", VoxCPMReader), "omnivoice": (OMNIVOICE, "OmniVoice", OmniVoiceReader)}


def kind(clone):
    """Which weights read `clone`, a voice from `cloned`, or Qwen3-TTS's own speakers for None."""
    if not clone:
        return "custom_voice"
    return "base" if clone[2] in LANGUAGES else "voxcpm" if clone[2] in OTHERS else "omnivoice"


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
    if CLONER or VOXCPM or OMNIVOICE:
        at = options.index("--qwen3_tts_model_name") + 1
        weights["custom_voice"] = options[at]
        for profile, model, name in ((CLONER, "Qwen3-TTS", "base"),
                                     *((profile, repo, name) for name, (profile, repo, _) in READERS.items())):
            if profile:
                (path,) = (path for repo, path in snapshots(manifest, profile).items() if repo.startswith(model))
                weights[name] = str(Path(cache).resolve() / path)
        # A cloned voice Qwen3-TTS reads that speaks first starts with the Base weights, as upstream
        # clones. Any other starts with the entry's own, and its sentences go to VoxCPM2 or OmniVoice.
        if (own := cloned(OWN)) and kind(own) == "base":
            options[at] = weights["base"]
            options += ["--qwen3_tts_ref_audio", own[0], "--qwen3_tts_ref_text", own[1]]
    return options


def read_aloud():
    """Let a text to read aloud name who reads it: `voice`, one of Qwen3-TTS's speakers or a voice
    cloned on this Mac, which Qwen3-TTS's Base weights, VoxCPM2 or OmniVoice read, by its language.

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
                # A cloned voice speaks when Boubou passed the weights that read it.
                clone = cloned(name)
                asked = speakers.get(name) or (name if clone and kind(clone) in weights else None)
                try:
                    return route.endpoint(request)
                finally:
                    asked = None

        def named(sentence):
            # Upstream keeps a speaker until a conversation ends, and a reading is none.
            reader.speaker, reader.language = (asked, reading) if asked else own
            clone = cloned(reader.speaker)
            if CLONER or VOXCPM or OMNIVOICE:
                which = kind(clone)
                if isinstance(weights[which], str):
                    model = load_model(weights[which])
                    weights[which] = READERS[which][2](model) if which in READERS else model
                reader.model = weights[which]
            reader.ref_audio, reader.ref_text = clone[:2] if clone else (None, reader.ref_text)
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
    clip whose words Whisper writes down, which `profile` holds. The Base weights that BOUBOU_CLONER
    holds speak each voice. Each voice stays in BOUBOU_VOICES, on this Mac.

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
    from fastapi.concurrency import run_in_threadpool
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
        # The Base weights of the size Boubou chose for this computer, BOUBOU_CLONER's.
        (base,) = (path for name, path in snapshots(manifest, CLONER).items() if name.startswith("Qwen3-TTS"))
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
            if not language or language not in WHISPER:
                return "Boubou cannot tell its language, or no voice model here speaks it. Upload it on this page instead."
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

        def listen(clip, language=""):
            """What Whisper writes down from 16 kHz `clip`, in `language` when the person named it: Whisper
            names many languages wrongly on its own, and writes them down well once told."""
            with mlx:
                heard = hear.generate(clip, verbose=False, **({"language": language} if language in WHISPER else {}))
            return " ".join(heard.text.split()), language if language in WHISPER else getattr(heard, "language", None)

        @app.post("/check")
        async def check(audio: UploadFile, words: str = Form(""), language: str = Form("")):
            """What Whisper hears in a take, and what to fix before cloning it: `boubou voices --check`
            compares the words heard with `words`, the words the take should say, when given. `language`
            is the one the person chose, when Whisper's own guess was wrong."""
            take = await audio.read()
            text, language = listen(samples(take, 16000), language)
            with TemporaryDirectory(prefix="boubou-check-") as temp:
                path = Path(temp, "take.wav")
                path.write_bytes(take)
                compared = [f"--words={words}", f"--heard={text}"] if words.strip() else []
                found = library("--check", str(path), *compared)
            known = language if language in WHISPER else ""
            # OmniVoice reads a voice in this language: the page asks for its noncommercial terms.
            noncommercial = bool(known) and kind((None, None, known)) == "omnivoice"
            return {**found, "text": text, "language": known, "noncommercial": noncommercial}

        def given(clip, name, text, language, consent=None, noncommercial=""):
            """A voice as the page gives it: its recording at the level it is kept, at the rate the Base
            weights read, after OmniVoice's terms when they read its language. `consent` and `name`
            are checked when the voice is saved."""
            if kind((None, None, language)) == "omnivoice" and not library()["noncommercial_accepted"]:
                if noncommercial != "yes":
                    raise HTTPException(400, "Tick the box to accept OmniVoice's noncommercial terms.")
                accept()
            if consent is not None and consent != "yes":
                raise HTTPException(400, "Confirm the voice is yours, or that you may use it.")
            if name is not None and (not NAME.fullmatch(name) or name in SPEAKERS):
                raise HTTPException(400, "Choose another name: lowercase letters and digits, not a built-in voice.")
            if not 0 < len(text) <= 500 or language not in WHISPER:
                raise HTTPException(400, "Write what the voice says.")
            clip = samples(clip, speak.sample_rate)
            if not 3 <= len(clip) / speak.sample_rate <= 30:
                raise HTTPException(400, "Give 3 to 30 seconds of voice.")
            # The page sends the take at the level it was recorded, so the check hears any
            # distortion. A voice is kept with its loudest moment near full scale.
            clip = clip - clip.mean()
            if np.abs(clip).max() < 0.01:
                raise HTTPException(400, "Boubou heard no voice. Check the microphone or the clip, then try again.")
            return wav(clip * (0.9 / np.abs(clip).max()), speak.sample_rate)

        @app.post("/voices")
        async def save(audio: UploadFile, name: str = Form(), text: str = Form(), language: str = Form(),
                       consent: str = Form(""), noncommercial: str = Form("")):
            name, text = name.strip().lower(), " ".join(text.split())
            recording = given(await audio.read(), name, text, language, consent, noncommercial)
            # The words last: a voice is listed once they are there.
            keep(Path(CLONES, f"{name}.wav"), recording)
            keep(Path(CLONES, f"{name}.json"), json.dumps({"text": text, "language": language}).encode())
            return {"name": name}

        @app.post("/takes")
        async def takes(audio: UploadFile, text: str = Form(), language: str = Form(), input: str = Form(),
                        seed: int = Form(0), noncommercial: str = Form("")):
            """A take of `input` in a voice the page has not saved yet, read from the recording as it
            would be saved. Each `seed` reads it a little differently."""
            text, line = " ".join(text.split()), " ".join(input.split())
            if not 0 < len(line) <= 300:
                raise HTTPException(400, "Type a sentence to hear.")
            recording = given(await audio.read(), None, text, language, noncommercial=noncommercial)
            with TemporaryDirectory(prefix="boubou-take-") as temp:
                path = Path(temp, "voice.wav")
                path.write_bytes(recording)
                # Off the event loop: the page stays answerable while a take is read.
                return await run_in_threadpool(read, (str(path), text, language), line, seed)

        @app.post("/voices/{name}/language")
        def relabel(name: str, request: dict):
            """Give a kept voice the language the person says it speaks, and its words heard in that
            language. Its reader follows the language."""
            clone, language = cloned(name), str(request.get("language", ""))
            if clone is None:
                raise HTTPException(404, "No voice has that name.")
            if language not in WHISPER:
                raise HTTPException(400, "Choose a language from the list.")
            if kind((None, None, language)) == "omnivoice" and not library()["noncommercial_accepted"]:
                raise HTTPException(403, "This language is read by OmniVoice, which allows noncommercial use only. "
                                         "Accept its terms on this page first.")
            audio, rate = soundfile.read(clone[0], dtype="float32", always_2d=True)
            text, _ = listen(resample_poly(audio.mean(axis=1), 16000, rate).astype(np.float32), language)
            if not text:
                raise HTTPException(400, "Boubou heard no words in it in that language.")
            keep(Path(CLONES, f"{name}.json"), json.dumps({"text": text, "language": language}).encode())
            return library()

        @app.delete("/voices/{name}")
        def delete(name: str):
            if not NAME.fullmatch(name):
                raise HTTPException(404)
            for suffix in (".json", ".wav"):
                Path(CLONES, f"{name}{suffix}").unlink(missing_ok=True)
            return Response(status_code=204)

        other = {}

        def boubou(*options, failure="Boubou could not do that."):
            """Run `boubou` on this page's cache and folder, and say why it failed, if it did."""
            done = subprocess.run([os.environ["BOUBOU_BIN"], "--cache-dir", str(cache.parent), "--voices-dir", CLONES,
                                   *options], capture_output=True, text=True)
            if done.returncode:
                said = [line for line in done.stderr.splitlines() if line.strip()] or [failure]
                raise HTTPException(503, said[-1].removeprefix("boubou: "))

        def accept():
            # Only after the person ticked the box that shows OmniVoice's terms.
            boubou("voices", "--accept-noncommercial", "--yes")

        def reader(clone):
            """What reads `clone`: the Base weights, or VoxCPM2 or OmniVoice for a language Qwen3-TTS does
            not speak, fetched the first time such a voice speaks. OmniVoice's voices wait for its terms."""
            which = kind(clone)
            if which == "base":
                return speak
            if which == "omnivoice" and not library()["noncommercial_accepted"]:
                raise HTTPException(403, "This voice is read by OmniVoice, which allows noncommercial use only. "
                                         "Accept its terms on this page first.")
            if which not in other:
                profile, repo, wrap = READERS[which]
                (path,) = (path for name, path in snapshots(manifest, profile).items() if name.startswith(repo))
                # Boubou fetches what is missing and verifies every file before the first load: a fetch
                # cut short may have left only some of them.
                boubou("voices", "--prepare", profile, failure="Boubou could not get the voice model.")
                other[which] = wrap(mouth(str(cache / path)))
            return other[which]

        @app.post("/noncommercial")
        def take_terms():
            accept()
            return library()

        def read(clone, line, seed=None):
            """`line` read aloud in `clone`, as a WAV; with `seed`, the same each time."""
            import mlx.core as mx

            reading = reader(clone)
            with mlx:
                if seed is not None:
                    mx.random.seed(seed)
                parts = [np.array(part.audio).reshape(-1) for part in reading.generate(
                    text=line, ref_audio=clone[0], ref_text=clone[1], lang_code="auto")]
            return Response(wav(np.concatenate(parts), reading.sample_rate), media_type="audio/wav")

        @app.post("/speech")
        def speech(request: dict):
            clone = cloned(request.get("voice"))
            if clone is None or not str(request.get("input", "")).strip():
                raise HTTPException(404, "No voice has that name.")
            return read(clone, str(request["input"]))

        uvicorn.run(app, host=address.host, port=address.port, log_level="warning")


if __name__ == "__main__":
    # Boubou owns the microphone and speakers; the pipeline serves the Realtime API on loopback.
    if sys.argv[1] == "clone":
        clone(*sys.argv[2:])
    else:
        run("serve", *sys.argv[1:])
