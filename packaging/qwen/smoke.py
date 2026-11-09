"""Exercise the installed offline speech pipeline without opening audio devices."""

import base64
import json
import os
import runpy
import socket
import subprocess
import sys
import time
from pathlib import Path

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
                        return
                    raise RuntimeError(f"{model} produced no completed spoken response")
                elif event["type"] == "error":
                    raise RuntimeError(f"{model}: {event['error']}")
            raise TimeoutError(f"{model} timed out waiting for audio")
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
