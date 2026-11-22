#!/usr/bin/env python3
"""Run the daemon's model-picker route contract against local HTTP and WS fixtures.

Usage: python3 tests/e2e/model_browse_cache_daemon.py /absolute/path/to/jcode
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from turn_activity_daemon import Connection, eventually


A, B = "gpt-5.6-sol", "gpt-5.4"


def send_frame(stream, opcode, payload):
    size = len(payload)
    if size >= 126:
        header = bytes([126]) + struct.pack("!H", size)
    else:
        header = bytes([size])
    stream.write(bytes([128 | opcode]) + header + payload)


def receive_frame(stream):
    header = stream.read(2)
    if not header:
        return 8, b""
    opcode, size = header[0] & 15, header[1] & 127
    if size == 126:
        size = struct.unpack("!H", stream.read(2))[0]
    elif size == 127:
        size = struct.unpack("!Q", stream.read(8))[0]
    mask = stream.read(4)
    return opcode, bytes(byte ^ mask[i % 4] for i, byte in enumerate(stream.read(size)))


class Responses(ThreadingHTTPServer):
    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.lock = threading.Lock()
        self.wires = []
        self.handshakes = 0
        self.closed = []

    def respond(self, request, connection):
        with self.lock:
            previous = next((obj for _, obj in reversed(self.wires) if obj.get("generate") is not False), None)
            self.wires.append((connection, request))
            response_id = f"resp_{len(self.wires)}"
        fields = ("model", "prompt_cache_key", "instructions", "tools", "reasoning", "service_tier")
        cached = previous is not None and all(request.get(field) == previous.get(field) for field in fields) and (
            request.get("previous_response_id") or request["input"][:len(previous["input"])] == previous["input"]
        )
        reply = [] if request.get("generate") is False else [{
            "id": f"msg_{response_id}", "type": "message", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": "fixture answer", "annotations": []}],
        }]
        events = [{"type": "response.created", "response": {"id": response_id}}]
        if reply:
            events += [
                {"type": "response.output_text.delta", "delta": "fixture answer", "item_id": reply[0]["id"], "output_index": 0, "content_index": 0},
                {"type": "response.output_item.done", "item": reply[0], "output_index": 0},
            ]
        events.append({"type": "response.completed", "response": {
            "id": response_id, "status": "completed", "output": reply,
            "usage": {"input_tokens": 4096, "input_tokens_details": {"cached_tokens": 4096 if cached else 0}, "output_tokens": 3},
        }})
        return events


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def write_body(self, content_type, body, status=200):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        assert self.path == "/v1/responses", self.path
        assert self.headers["Authorization"] == "Bearer sk-fixture-only"
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        events = self.server.respond(request, None)
        body = "".join(f"data: {json.dumps(event)}\n\n" for event in events).encode()
        self.write_body("text/event-stream", body)

    def do_GET(self):
        if self.path == "/v1/models":
            self.write_body("application/json", json.dumps({"data": [{"id": model, "object": "model"} for model in (A, B)]}).encode())
            return
        if self.path != "/v1/responses" or self.headers.get("Upgrade", "").lower() != "websocket":
            self.write_body("application/json", b"{}", 404)
            return
        assert self.headers["Authorization"] == "Bearer sk-fixture-only"
        with self.server.lock:
            self.server.handshakes += 1
            connection = self.server.handshakes
        accept = hashlib.sha1((self.headers["Sec-WebSocket-Key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()
        self.send_response(101)
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", base64.b64encode(accept).decode())
        self.end_headers()
        self.close_connection = True
        try:
            while True:
                opcode, payload = receive_frame(self.rfile)
                if opcode == 8:
                    return
                if opcode == 9:
                    send_frame(self.wfile, 10, payload)
                elif opcode == 1:
                    for event in self.server.respond(json.loads(payload), connection):
                        send_frame(self.wfile, 1, json.dumps(event).encode())
                else:
                    assert opcode == 10, opcode
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            with self.server.lock:
                self.server.closed.append(connection)


def scenario(binary, root, transport):
    root.mkdir()
    jhome, runtime = root / "jcode", root / "run"
    for path in (jhome, runtime):
        path.mkdir()
    path = runtime / "browse.sock"
    provider = Responses()
    thread = threading.Thread(target=provider.serve_forever, daemon=True)
    thread.start()
    env = {
        "PATH": "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": str(root), "USER": "fixture", "TERM": "xterm-256color",
        "JCODE_HOME": str(jhome), "JCODE_RUNTIME_DIR": str(runtime), "JCODE_SOCKET": str(path),
        "XDG_RUNTIME_DIR": str(runtime), "TMPDIR": str(runtime),
        "JCODE_TEST_SESSION": "1", "JCODE_NO_TELEMETRY": "1", "JCODE_NO_AUTO_UPDATE": "1",
        "JCODE_PROVIDER": "openai-api", "JCODE_MODEL": A, "JCODE_OPENAI_MODEL": A,
        "JCODE_OPENAI_TRANSPORT": transport, "OPENAI_API_KEY": "sk-fixture-only",
        "JCODE_OPENAI_API_BASE": f"http://127.0.0.1:{provider.server_port}/v1",
    }
    with (root / "daemon.log").open("w+") as log:
        daemon = subprocess.Popen([binary, "--no-update", "--no-selfdev", "serve", "--socket", str(path)], cwd=root, env=env, stdout=log, stderr=log)
        try:
            def connect():
                try:
                    return Connection(path)
                except OSError:
                    return None

            wire = eventually("private daemon socket", connect)
            def call(kind, request_id, reply="done", observed=None, **fields):
                def received(event):
                    if observed is not None:
                        observed.append(event)
                    return event.get("id") == request_id and event["type"] == reply

                wire.send({"type": kind, "id": request_id, **fields})
                return wire.event(kind, received)

            def select(model, request_id, effort="medium", structured=True):
                if structured:
                    selection = {
                        "model": model, "provider_label": "OpenAI", "api_method": "openai-api",
                        "runtime_key": {"kind": "open-a-i-api-key"},
                    }
                    event = call("set_route", request_id, "model_changed", selection=selection)
                else:
                    event = call("set_model", request_id, "model_changed", model=f"openai-api:{model}")
                assert event.get("model") == model, event
                applied = call("set_reasoning_effort", request_id + 1, "reasoning_effort_changed", effort=effort)
                assert applied.get("effort") == effort and not applied.get("error"), applied

            def wires():
                return [(connection, request) for connection, request in provider.wires if request.get("generate") is not False]

            call("subscribe", 1, working_dir=str(root))
            call("rename_session", 2, title="Model picker cache fixture")
            select(A, 10)
            call("message", 12, content="Warm the fixture prefix.")
            assert len(wires()) == 1, wires()
            count, handshakes = len(provider.wires), provider.handshakes
            connection, initial = wires()[0]
            assert initial["model"] == A and initial["prompt_cache_key"] and initial["reasoning"]["effort"] == "medium", initial
            select(A, 20, structured=False)
            select(B, 22, effort="high", structured=False)
            select(A, 24, structured=False)
            time.sleep(0.15)
            assert len(provider.wires) == count, "model selection generated or prewarmed"
            assert provider.handshakes == handshakes, "model selection opened a new socket"
            was_closed = connection in provider.closed
            resume_events = []
            call("message", 26, observed=resume_events, content="Resume the fixture prefix.")
            assert len(wires()) == 2, wires()
            kept_connection, resumed = wires()[1]
            assert all(initial.get(field) == resumed.get(field) for field in ("model", "reasoning", "instructions", "tools", "prompt_cache_key"))
            if transport == "websocket":
                assert connection is not None and kept_connection is not None, "not a WebSocket run"
                assert kept_connection == connection and resumed.get("previous_response_id") and not was_closed, (
                    f"A→B→A closed the established socket before send: {was_closed}; "
                    f"same socket: {kept_connection == connection}; response continuation: {resumed.get('previous_response_id')!r}"
                )
            else:
                assert connection is None and kept_connection is None and not resumed.get("previous_response_id")
                assert resumed["input"][:len(initial["input"])] == initial["input"]
            assert any(event["type"] == "request_usage" and event.get("cache_read_input") == 4096 for event in resume_events)
            assert not any(event["type"] == "kv_cache_miss" for event in resume_events), resume_events
            select(B, 30)
            call("message", 32, content="Send once on the changed model.")
            assert len(wires()) == 3, wires()
            changed_connection, changed = wires()[2]
            assert changed["model"] == B and not changed.get("previous_response_id")
            if transport == "websocket":
                assert changed_connection is not None and changed_connection != kept_connection
            print(f"{transport}: no inference or prewarm on selection, same affinity, changed model isolated")
            wire.close()
        except Exception:
            log.seek(0)
            print(log.read(), file=sys.stderr)
            raise
        finally:
            daemon.terminate()
            daemon.wait(timeout=10)
            provider.shutdown()
            provider.server_close()
            thread.join()


if __name__ == "__main__":
    binary = str(Path(sys.argv[1]).resolve())
    print(subprocess.check_output([binary, "--version"], text=True).strip(), flush=True)
    with tempfile.TemporaryDirectory(prefix="jcb-", dir=os.environ.get("JCODE_SCRATCH_DIR")) as temporary:
        for transport in ("https", "websocket"):
            scenario(binary, Path(temporary) / transport, transport)
