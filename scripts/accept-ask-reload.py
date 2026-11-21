#!/usr/bin/env python3
"""Isolated acceptance: pending ask survives a real forced daemon exec reload.

Run with the freshly built jcode binary as the only argument. No real provider,
shared daemon, or user sessions are used.
"""
import json
import os
import pathlib
import socket
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.reply({"data": [{"id": "fake-model", "object": "model"}]})

    def reply(self, value):
        data = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["content-length"])))
        messages = request.get("messages", [])
        has_result = any(m.get("role") == "tool" for m in messages)
        if not has_result:
            delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": "call-ask-reload", "type": "function", "function": {"name": "ask", "arguments": json.dumps({"intent": "fixture", "questions": [{"question": "Choose?", "options": [{"label": "Alpha"}, {"label": "Beta"}]}]})}}]}
            finish = "tool_calls"
        else:
            delta = {"role": "assistant", "content": "continued"}
            finish = "stop"
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        for choice in [{"index": 0, "delta": delta}, {"index": 0, "delta": {}, "finish_reason": finish}]:
            self.wfile.write(b"data: " + json.dumps({"id": "fake", "object": "chat.completion.chunk", "model": "fake-model", "choices": [choice]}).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


class Client:
    def __init__(self, path):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(20)
        self.socket.connect(path)
        self.reader = self.socket.makefile("rb")

    def send(self, **request):
        self.socket.sendall(json.dumps(request).encode() + b"\n")

    def until(self, predicate):
        while True:
            line = self.reader.readline()
            if not line:
                raise AssertionError("daemon disconnected before expected event")
            event = json.loads(line)
            if predicate(event):
                return event

    def close(self):
        self.reader.close()
        self.socket.close()


def main(binary):
    provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=provider.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="jcode-ask-") as temp:
        root = pathlib.Path(temp).resolve()
        home, runtime, jcode = (root / name for name in ("home", "run", "jcode"))
        for path in (home, runtime, jcode):
            path.mkdir()
        sock = str(runtime / "j.sock")
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home), "USER": "test", "TERM": "xterm-256color", "TMPDIR": str(runtime), "XDG_RUNTIME_DIR": str(runtime), "JCODE_HOME": str(jcode), "JCODE_RUNTIME_DIR": str(runtime), "JCODE_SOCKET": sock, "JCODE_OPENAI_COMPAT_API_BASE": f"http://127.0.0.1:{provider.server_port}/v1", "OPENAI_COMPAT_API_KEY": "fixture-only", "JCODE_OPENAI_COMPAT_DEFAULT_MODEL": "fake-model", "JCODE_NO_TELEMETRY": "1", "JCODE_TELEMETRY": "off", "JCODE_NO_AUTO_UPDATE": "1"}
        base = [str(pathlib.Path(binary).resolve()), "--no-update", "--no-selfdev", "--provider", "openai-compatible", "--model", "fake-model"]
        def cli(*args):
            result = subprocess.run(base + list(args), env=env, cwd=root, capture_output=True, text=True, timeout=45)
            if result.returncode:
                raise AssertionError(f"{args}: {result.stdout} {result.stderr}")
            return result
        try:
            cli("server", "start", "--json")
            first = Client(sock)
            first.send(type="subscribe", id=1, working_dir=str(root), continue_on_disconnect=True)
            session = first.until(lambda e: e.get("type") == "session")["session_id"]
            first.until(lambda e: e.get("type") == "done" and e.get("id") == 1)
            first.send(type="message", id=2, content="Choose one")
            original = first.until(lambda e: e.get("type") == "ask_user_request")
            reload_result = json.loads(cli("server", "reload", "--force", "--json").stdout)
            assert reload_result["reloaded"] and reload_result["handoff_ready"], reload_result
            first.socket.settimeout(5)
            for line in first.reader:
                event = json.loads(line)
                assert not (event.get("type") == "tool_done" and event.get("name") == "ask"), event
            first.close()
            second = Client(sock)
            second.send(type="subscribe", id=3, target_session_id=session, working_dir=str(root), continue_on_disconnect=True)
            history = second.until(lambda e: e.get("type") == "history")
            assert not any("interrupted by server reload" in row.get("content", "") for row in history["messages"]), history["messages"]
            directive = history["reload_recovery"]["continuation_message"]
            assert directive
            second.until(lambda e: e.get("type") == "done" and e.get("id") == 3)
            second.send(type="message", id=4, content="", system_reminder=directive)
            replayed = second.until(lambda e: e.get("type") == "ask_user_request")
            assert replayed["request_id"] == original["request_id"]
            assert replayed["tool_call_id"] == original["tool_call_id"]
            second.send(type="ask_user_response", id=5, request_id=replayed["request_id"], response={"answers": [{"selected": ["Beta"]}]})
            result = second.until(lambda e: e.get("type") == "tool_done" and e.get("name") == "ask")
            assert result["id"] == original["tool_call_id"] and "Beta" in result["output"], result
            second.until(lambda e: e.get("type") == "done" and e.get("id") == 4)
            second.send(type="get_history", id=6)
            finished = second.until(lambda e: e.get("type") == "history" and e.get("id") == 6)
            rows = finished["messages"]
            assert sum("Beta" in row.get("content", "") for row in rows) == 1, rows
            assert not any("interrupted by server reload" in row.get("content", "") for row in rows), rows
            stored = json.loads((jcode / "sessions" / f"{session}.json").read_text())
            messages = stored["messages"]
            call_index = next(i for i, message in enumerate(messages) if any(block.get("id") == original["tool_call_id"] and block.get("type") == "tool_use" for block in message["content"]))
            result_index = next(i for i, message in enumerate(messages) if any(block.get("tool_use_id") == original["tool_call_id"] and "Beta" in block.get("content", "") for block in message["content"]))
            assert result_index == call_index + 1, messages
            assert stored["model_usage_turn_id"].endswith(messages[call_index - 1]["id"]), stored["model_usage_turn_id"]
            second.close()
            print("PASS: original ask replayed after forced reload; answer persisted once")
        finally:
            cli("server", "stop", "--force", "--json")
            provider.shutdown()


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} /path/to/jcode")
    main(sys.argv[1])
