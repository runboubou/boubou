"""Run the pinned HF conversation on a Mac, with the models any pipeline options name."""

import importlib.util
import os
import sys
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


def arguments(*args):
    options = pinned(*args)
    os.environ.update(HUB)
    return options


def read_aloud():
    """Let a text to read aloud name who reads it: `voice`, one of Qwen3-TTS's speakers.

    Any other name, such as OpenAI's own, keeps the voice Boubou started with, as every other
    model does. voice.py reads in that voice only, and stays as the Linux runner published it.
    """
    import threading

    from speech_to_speech.api.openai_realtime import server

    one_voice()
    create_app = server.create_app

    def with_voices(pool, **options):
        app = create_app(pool=pool, **options)
        # Qwen3-TTS, the one voice with speakers to choose from.
        reader = next((handler for handler in pool[0].handlers if hasattr(handler, "_initial_speaker")), None)
        if reader is None:
            return app
        speakers = {name.lower(): name for name in reader._supported_speakers() or ()}
        own, process, turn = (reader._initial_speaker, reader.language), reader.process, threading.Lock()
        asked = None
        route = next(route for route in app.routes if getattr(route, "path", None) == "/v1/audio/speech")
        app.router.routes.remove(route)

        @app.post(route.path)
        def speech(request: dict):
            nonlocal asked
            with turn:
                asked = speakers.get(str(request.get("voice")).lower())
                try:
                    return route.endpoint(request)
                finally:
                    asked = None

        def named(sentence):
            # Upstream keeps a speaker until a conversation ends, and a reading is none. A named
            # speaker reads a text in the text's language, as the Whisper entries speak.
            reader.speaker, reader.language = (asked, "auto") if asked else own
            return process(sentence)

        reader.process = named
        return app

    server.create_app = with_voices


# run() builds the pipeline from voice.py's module-level names.
voice.arguments, voice.read_aloud = arguments, read_aloud
run = voice.run

if __name__ == "__main__":
    # Boubou owns the microphone and speakers; the pipeline serves the Realtime API on loopback.
    run("serve", *sys.argv[1:])
