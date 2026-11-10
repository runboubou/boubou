"""Exercise the installed offline speech pipeline without opening audio devices."""

import base64
import io
import json
import os
import runpy
import socket
import subprocess
import sys
import time
import wave
from pathlib import Path
from urllib.request import ProxyHandler, Request, build_opener

from websockets.sync.client import connect


def serve(model, cache, manifest, port):
    entry = Path(sys.executable).resolve().parents[1] / ("cuda.py" if sys.platform == "linux" else "voice.py")
    run = runpy.run_path(str(entry))["run"]
    run("serve", model, cache, manifest, "--host", "127.0.0.1", "--port", port)


def check(model, cache, manifest):
    with socket.socket() as reserved:
        reserved.bind(("127.0.0.1", 0))
        port = str(reserved.getsockname()[1])
    command = [sys.executable, "-I", "-B", __file__, "--serve", model, cache, manifest, port]
    env = dict(os.environ, HTTPS_PROXY="http://127.0.0.1:9", HTTP_PROXY="http://127.0.0.1:9",
               ALL_PROXY="http://127.0.0.1:9", NO_PROXY="localhost,127.0.0.1")
    child = subprocess.Popen(command, env=env)
    try:
        deadline = time.monotonic() + 480
        while child.poll() is None and time.monotonic() < deadline:
            try:
                ws = connect(f"ws://127.0.0.1:{port}/v1/realtime", proxy=None, open_timeout=3)
                break
            except (OSError, TimeoutError):
                time.sleep(1)
        else:
            raise RuntimeError(f"{model} server failed to become ready (exit {child.poll()})")
        with ws:
            ws.send(json.dumps({"type": "conversation.item.create", "item": {
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "请用中文打个招呼。" if model.endswith("-whisper") or model == "chattts:whisper" else "Please say hello in English."}]}}))
            ws.send(json.dumps({"type": "response.create"}))
            start = time.monotonic()
            deadline = start + 180
            heard = False
            while time.monotonic() < deadline:
                event = json.loads(ws.recv(timeout=max(1, deadline - time.monotonic())))
                if event["type"] == "response.output_audio.delta" and not heard:
                    audio = base64.b64decode(event["delta"], validate=True)
                    if len(audio) >= 2 and any(audio):
                        print(f"{model}: first non-silent PCM after {time.monotonic()-start:.2f}s", flush=True)
                        heard = True
                elif event["type"] == "response.done":
                    if event["response"]["status"] == "completed" and heard:
                        break
                    raise RuntimeError(f"{model} produced no completed spoken response")
                elif event["type"] == "error":
                    raise RuntimeError(f"{model}: {event['error']}")
            else:
                raise TimeoutError(f"{model} timed out waiting for audio")
        # After the conversation, the same voice reads a text aloud.
        request = Request(f"http://127.0.0.1:{port}/v1/audio/speech", b'{"input": "Hello. Dinner is ready."}',
                          {"Content-Type": "application/json"})
        start = time.monotonic()
        with build_opener(ProxyHandler({})).open(request, timeout=180) as response:
            with wave.open(io.BytesIO(response.read())) as wav:
                speech = wav.readframes(wav.getnframes())
                if wav.getparams()[:3] != (1, 2, 24000) or not any(speech):
                    raise RuntimeError(f"{model} read nothing aloud")
        print(f"{model}: read {len(speech) / 48000:.2f}s of speech aloud in {time.monotonic()-start:.2f}s", flush=True)
    finally:
        child.terminate()
        try:
            child.wait(timeout=15)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()


if __name__ == "__main__":
    if sys.argv[1] == "--serve":
        serve(*sys.argv[2:])
    else:
        check(*sys.argv[1:])
