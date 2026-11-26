#!/usr/bin/env python3
"""Prove that the real daemon stops paid cache misses. All HTTP stays on loopback."""
import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import time
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

Client = runpy.run_path(str(Path(__file__).with_name("accept-ask-reload.py")))["Client"]
MODEL = "claude-sonnet-5-5"


class Anthropic(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_CONNECT(self):
        self.send_error(403, "fixture may not access external services")

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        if self.path != "/v1/messages" or body.get("model") != MODEL:
            self.send_error(400, "unexpected fixture route")
            return
        with self.server.hits_lock:
            self.server.hits += 1
            count = self.server.hits
        tool = count <= 3
        block = {"type": "tool_use", "id": f"toolu_fixture_{count}", "name": "read", "input": {}} if tool else {"type": "text", "text": ""}
        delta = {"type": "input_json_delta", "partial_json": '{"file_path":"fixture.txt"}'} if tool else {"type": "text_delta", "text": "Finished."}
        events = [
            {"type": "message_start", "message": {"id": f"msg_fixture_{count}", "type": "message", "role": "assistant", "model": MODEL, "content": [], "stop_reason": None, "stop_sequence": None, "usage": {"input_tokens": 200000, "cache_read_input_tokens": 130721, "cache_creation_input_tokens": 0, "output_tokens": 0}}},
            {"type": "content_block_start", "index": 0, "content_block": block},
            {"type": "content_block_delta", "index": 0, "delta": delta},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": "tool_use" if tool else "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 8}},
            {"type": "message_stop"},
        ]
        data = "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events).encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def exercise(binary, root, provider):
    home, runtime, history = (root / name for name in ("home", "run", "history"))
    for path in (home, runtime, history):
        path.mkdir()
    (root / "fixture.txt").write_text("Known text from a private fixture.\n")
    sock = str(runtime / "j.sock")
    env = dict(
        PATH=os.environ.get("PATH", "/usr/bin:/bin"), HOME=str(home), USER="test",
        TMPDIR=str(runtime), XDG_RUNTIME_DIR=str(runtime), JCODE_RUNTIME_DIR=str(runtime),
        JCODE_HOME=str(history), JCODE_SOCKET=sock, JCODE_TEST_SESSION="1",
        JCODE_NO_TELEMETRY="1", JCODE_NO_AUTO_UPDATE="1", ANTHROPIC_API_KEY="fixture-only",
        ANTHROPIC_BASE_URL=f"http://127.0.0.1:{provider.server_port}/v1",
        HTTPS_PROXY=f"http://127.0.0.1:{provider.server_port}",
        HTTP_PROXY=f"http://127.0.0.1:{provider.server_port}", NO_PROXY="127.0.0.1,localhost",
    )
    base = [str(Path(binary).resolve()), "--no-update", "--no-selfdev", "--provider", "anthropic-api", "--model", MODEL]

    child = None

    def server(subcommand):
        nonlocal child
        if subcommand == "stop":
            if child and child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)
            Path(sock).unlink(missing_ok=True)
            return
        with (root / "daemon.log").open("ab") as log:
            child = subprocess.Popen(base + ["serve", "--socket", sock], env=env, cwd=root,
                                     stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        for _ in range(150):
            assert child.poll() is None, (root / "daemon.log").read_text()
            if Path(sock).exists():
                try:
                    probe = Client(sock)
                    probe.send(type="ping", id=0)
                    probe.until(lambda e: e.get("type") == "pong")
                    probe.close()
                    return
                except ConnectionRefusedError:
                    pass
            time.sleep(0.05)
        raise AssertionError((root / "daemon.log").read_text())

    def attach(session=None):
        client = Client(sock)
        client.send(type="subscribe", id=1, target_session_id=session, working_dir=str(root), continue_on_disconnect=True)
        attached = client.until(lambda e: e.get("type") == "session")["session_id"]
        assert session is None or attached == session, (session, attached)
        client.until(lambda e: e.get("type") == "done" and e.get("id") == 1)
        return client, attached

    def command(client, identifier, arg):
        for attempt in range(50):
            request_id = identifier * 100 + attempt
            client.send(type="runtime_command", id=request_id, command="cache", args=[arg])
            event = client.until(lambda e: e.get("id") == request_id and e.get("type") in ("error", "runtime_command_result"))
            if event["type"] == "runtime_command_result":
                return event["data"]
            if "Session is busy" not in event.get("message", ""):
                raise AssertionError(event)
            time.sleep(0.1)
        raise AssertionError(f"cache {arg} remained busy: {event}")

    def message(client, identifier, text, trace=None):
        client.send(type="message", id=identifier, content=text)
        def terminal(event):
            if trace is not None:
                trace.append(event)
            return event.get("id") == identifier and event.get("type") in ("error", "done")
        return client.until(terminal)

    def rejected(event):
        assert event["type"] == "error" and "Paused by cache protection" in event["message"], event

    server("start")
    try:
        client, session = attach()
        try:
            rejected(message(client, 2, "Read fixture.txt and continue."))
            assert provider.hits == 3, provider.hits
            rejected(message(client, 3, "Try before restart."))
            pause = command(client, 4, "status")["cache_pause"]
            assert pause["uncached_input"] == 600000 and pause["requests"] == 3, pause
            assert provider.hits == 3, provider.hits
        finally:
            client.close()
        server("stop")
        server("start")
        client, _ = attach(session)
        try:
            rejected(message(client, 5, "Try after restart."))
            assert command(client, 6, "status")["cache_pause"] == pause
            assert command(client, 7, "resume") == {"resumed": True}
            time.sleep(0.5)
            assert provider.hits == 3, provider.hits
            trace = []
            try:
                finished = message(client, 8, "Read after explicit resume.", trace)
                assert finished["type"] == "done", finished
                assert provider.hits == 4, provider.hits
            except Exception:
                print("Final message event trace:", json.dumps(trace), file=sys.stderr)
                print("Daemon log:", (root / "daemon.log").read_text(), file=sys.stderr)
                raise
        finally:
            client.close()
    finally:
        server("stop")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} /path/to/jcode")
    with ThreadingHTTPServer(("127.0.0.1", 0), Anthropic) as provider:
        provider.hits = 0
        provider.hits_lock = threading.Lock()
        threading.Thread(target=provider.serve_forever, daemon=True).start()
        try:
            with tempfile.TemporaryDirectory(prefix="jc-cache-", dir="/tmp") as temp:
                exercise(sys.argv[1], Path(temp).resolve(), provider)
        finally:
            provider.shutdown()
    print("PASS: real daemon stopped after 3 costly Anthropic requests, persisted the block, and sent the fourth only after explicit resume and message")
