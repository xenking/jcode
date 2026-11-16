#!/usr/bin/env python3
"""Exercise /btw in a real TUI while the parent's provider request is held.

Requires tmux and a freshly built jcode binary. The daemon and provider are
isolated under a temporary home and a private tmux server.
"""
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Provider(BaseHTTPRequestHandler):
    held = threading.Event()
    release = threading.Event()

    def log_message(self, *args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "fake-model", "object": "model"}]}).encode()
        self.send_response(200)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["content-length"])))
        prompt = json.dumps(request.get("messages", []))
        if "hold-parent-turn" in prompt and "unique-side-question" not in prompt:
            self.held.set()
            if not self.release.wait(45):
                self.send_error(504)
                return
        response = "side-answer-unique" if "unique-side-question" in prompt else "parent-finished"
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        for choice in [{"index": 0, "delta": {"role": "assistant", "content": response}}, {"index": 0, "delta": {}, "finish_reason": "stop"}]:
            self.wfile.write(b"data: " + json.dumps({"id": "fixture", "object": "chat.completion.chunk", "model": "fake-model", "choices": [choice]}).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def main(binary):
    provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=provider.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="jcode-btw-") as temp:
        root = pathlib.Path(temp).resolve()
        home, runtime, jcode = (root / name for name in ("home", "run", "jcode"))
        for path in (home, runtime, jcode):
            path.mkdir()
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home), "USER": "test", "TERM": "xterm-256color", "TMPDIR": str(runtime), "XDG_RUNTIME_DIR": str(runtime), "JCODE_HOME": str(jcode), "JCODE_RUNTIME_DIR": str(runtime), "JCODE_SOCKET": str(runtime / "j.sock"), "JCODE_OPENAI_COMPAT_API_BASE": f"http://127.0.0.1:{provider.server_port}/v1", "OPENAI_COMPAT_API_KEY": "fixture-only", "JCODE_OPENAI_COMPAT_DEFAULT_MODEL": "fake-model", "JCODE_NO_TELEMETRY": "1", "JCODE_TELEMETRY": "off", "JCODE_NO_AUTO_UPDATE": "1"}
        base = [str(pathlib.Path(binary).resolve()), "--no-update", "--no-selfdev", "--provider", "openai-compatible", "--model", "fake-model"]
        tmux = ["tmux", "-L", f"jcode-btw-test-{os.getpid()}"]
        def tm(*args):
            return subprocess.run(tmux + list(args), env=env, cwd=root, text=True, capture_output=True, check=True)
        def screen():
            return tm("capture-pane", "-p", "-t", "test").stdout
        def send(text):
            tm("send-keys", "-t", "test", "-l", text)
            tm("send-keys", "-t", "test", "Enter")
        try:
            tm("new-session", "-d", "-s", "test", "-x", "140", "-y", "40", *base)
            # The composer has to finish initializing before keys are sent.
            time.sleep(2)
            send("hold-parent-turn")
            assert Provider.held.wait(20), f"parent did not reach the provider: {screen()}"
            send("/btw unique-side-question")
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                output = screen()
                if "side-answer-unique" in output:
                    break
                time.sleep(0.1)
            else:
                raise AssertionError(f"side question did not finish while parent was held:\n{screen()}")
            assert not Provider.release.is_set()
            print("PASS: side question answered while parent agent remained locked")
        finally:
            Provider.release.set()
            subprocess.run(tmux + ["kill-server"], env=env, cwd=root, capture_output=True)
            subprocess.run(base + ["server", "stop", "--force", "--json"], env=env, cwd=root, capture_output=True, timeout=20)
            provider.shutdown()


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} /path/to/jcode")
    main(sys.argv[1])
