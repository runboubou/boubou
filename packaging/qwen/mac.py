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


def arguments(*args):
    options = pinned(*args)
    os.environ.update(HUB)
    return options


# run() builds the pipeline from voice.py's module-level mapping.
voice.arguments = arguments
run = voice.run

if __name__ == "__main__":
    # Boubou owns the microphone and speakers; the pipeline serves the Realtime API on loopback.
    run("serve", *sys.argv[1:])
