#!/usr/bin/env python3
"""Real isolated daemon: tool/ask facts -> reload capture -> checkpoint receipts.

Usage: python3 tests/e2e/turn_activity_daemon.py /absolute/built/jcode
Only a copied executable, temporary HOME, private socket and localhost fixture
provider are used. JCODE_TEST_SESSION is deliberately absent, so exec is real.
"""
import hashlib
import json
import os
from pathlib import Path
import select
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def eventually(label, operation, seconds=30):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = operation()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError("timeout: " + label)


class Connection:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(str(path))
        self.buffer = b""

    def close(self):
        self.sock.close()

    def send(self, value):
        self.sock.sendall(json.dumps(value).encode() + b"\n")

    def event(self, label, predicate, seconds=30):
        deadline = time.monotonic() + seconds
        seen = []
        while time.monotonic() < deadline:
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                event = json.loads(line)
                seen.append(event.get("type"))
                if event.get("type") == "error":
                    raise AssertionError(event)
                if predicate(event):
                    return event
            if select.select([self.sock], [], [], 0.1)[0]:
                data = self.sock.recv(65536)
                if not data:
                    raise AssertionError("daemon disconnected waiting for " + label)
                self.buffer += data
        raise AssertionError(f"timeout {label}: {seen}")


def main(binary, expected_sha=None):
    scratch = Path(os.environ["JCODE_SCRATCH_DIR"])
    scratch.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="turn-activity-", dir=scratch) as temporary:
        root = Path(temporary)
        home, jhome, runtime = [root / name for name in ("home", "jcode", "run")]
        for directory in (home, jhome, runtime):
            directory.mkdir()
        executable = root / "fixture-jcode"
        shutil.copy2(binary, executable)
        executable.chmod(0o700)
        version = subprocess.check_output([str(executable), "--version"], text=True).strip()
        if expected_sha:
            assert expected_sha in version, f"wrong build provenance: {version}, expected {expected_sha}"
        digest = hashlib.sha256(executable.read_bytes()).hexdigest()
        for channel in ("current", "shared-server"):
            path = jhome / "builds" / channel / "jcode"
            path.parent.mkdir(parents=True)
            path.symlink_to(executable)
        started = root / "bash-started"

        class Provider(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):
                body = json.dumps({"data": [{"id": "fake-model", "object": "model"}]}).encode()
                self.send_response(200)
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers["content-length"])))
                messages = request.get("messages", [])
                text = json.dumps(messages)
                already_called = any(message.get("tool_calls") for message in messages)
                tool = None
                if not already_called and "activity-hold-bash" in text:
                    tool = ("bash", {"command": f"printf ready > {shlex.quote(str(started))}; sleep 60"})
                elif not already_called and "activity-hold-ask" in text:
                    tool = ("ask", {"questions": [{"question": "Choose a fixture answer?", "options": [{"label": "Yes (Recommended)"}, {"label": "No"}]}]})
                if tool:
                    name, arguments = tool
                    delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": "fixture-" + name, "type": "function", "function": {"name": name, "arguments": json.dumps(arguments)}}]}
                    finish = "tool_calls"
                else:
                    delta, finish = {"role": "assistant", "content": "fixture complete"}, "stop"
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.end_headers()
                for value in [
                    {"choices": [{"index": 0, "delta": delta}]},
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}], "usage": {"prompt_tokens": 1, "completion_tokens": 1}},
                ]:
                    value.update({"id": "fixture", "object": "chat.completion.chunk", "model": "fake-model"})
                    try:
                        self.wfile.write(b"data: " + json.dumps(value).encode() + b"\n\n")
                        self.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError):
                        return
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()

        provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
        threading.Thread(target=provider.serve_forever, daemon=True).start()
        path = runtime / "activity.sock"
        env = {
            "PATH": "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            "HOME": str(home), "USER": os.environ.get("USER", "test"), "TERM": "xterm-256color",
            "TMPDIR": str(runtime), "XDG_RUNTIME_DIR": str(runtime), "JCODE_RUNTIME_DIR": str(runtime),
            "JCODE_HOME": str(jhome), "JCODE_SOCKET": str(path), "JCODE_PROVIDER": "openai-compatible",
            "JCODE_MODEL": "fake-model", "JCODE_OPENAI_COMPAT_DEFAULT_MODEL": "fake-model",
            "JCODE_OPENAI_COMPAT_API_BASE": f"http://127.0.0.1:{provider.server_port}/v1",
            "OPENAI_COMPAT_API_KEY": "fixture-only", "JCODE_SWARM_MODEL": "",
            "JCODE_NO_TELEMETRY": "1", "JCODE_TELEMETRY": "off", "JCODE_NO_AUTO_UPDATE": "1",
        }
        log = open(root / "daemon.log", "w+")
        daemon = subprocess.Popen([str(executable), "--no-update", "--no-selfdev", "serve", "--socket", str(path)],
                                  cwd=root, env=env, stdout=log, stderr=log, start_new_session=True)
        connections = []
        try:
            def connect():
                try:
                    connection = Connection(path)
                    connections.append(connection)
                    return connection
                except OSError:
                    return None

            def attach(marker=None):
                connection = eventually("private daemon socket", connect)
                connection.send({"type": "subscribe", "id": 1, "working_dir": str(root), "continue_on_disconnect": True})
                event = connection.event("session", lambda event: event.get("session_id") and event.get("type") in ("history", "session_id", "session"))
                if marker:
                    connection.send({"type": "message", "id": 2, "content": marker})
                return connection, event["session_id"]

            tool, tool_id = attach("activity-hold-bash")
            tool.event("bash execution", lambda event: event.get("type") == "tool_exec" and event.get("name") == "bash")
            eventually("real tool participant", started.exists)
            asking, ask_id = attach("activity-hold-ask")
            asking.event("pending user question", lambda event: event.get("type") == "ask_user_request")
            control, initiator = attach()
            control.send({"type": "reload", "id": 3, "force": True})
            control.event("forced reload acknowledgement", lambda event: event.get("type") == "reloading")

            def traces():
                result = []
                for trace in (jhome / "reload-traces").glob("*.jsonl"):
                    for line in trace.read_text().splitlines():
                        try:
                            result.append(json.loads(line))
                        except ValueError:
                            pass
                return result

            def phase(name):
                return next((event for event in traces() if event.get("phase") == name), None)

            captured = eventually("canonical capture", lambda: phase("candidate_snapshot"))
            facts = {fact["session_id"]: fact for fact in captured["activity"]["sessions"]}
            assert captured["activity"]["admissions_closed"], captured
            assert facts[tool_id]["active"] and facts[tool_id]["phase"] == "in_tool", facts
            assert facts[ask_id]["active"] and facts[ask_id]["phase"] == "ask_blocked", facts
            assert "bash" in facts[tool_id]["reason"], facts
            wait = eventually("durable checkpoint receipts", lambda: phase("checkpoint_wait_complete"))
            assert wait["unfinished"] == [], wait
            assert wait["initiator_excluded"] == initiator, wait
            eventually("real exec", lambda: phase("exec_start"))
            eventually("replacement listener", lambda: phase("socket_ready"), seconds=45)
            receipt = {
                "binary": str(Path(binary).resolve()), "binary_version": version, "binary_sha256": digest,
                "tool_session": tool_id, "ask_session": ask_id,
                "initiator": initiator, "captured": [facts[tool_id], facts[ask_id]],
                "checkpoint_wait": wait, "exec": True, "socket_ready": True,
            }
            evidence = scratch / f"turn-activity-acceptance-{int(time.time())}.json"
            evidence.write_text(json.dumps(receipt, indent=2) + "\n")
            print(f"PASS: tool and ask lifecycle captured, both durably checkpointed, real isolated exec; evidence={evidence}")
        except Exception:
            log.flush()
            print((root / "daemon.log").read_text(errors="replace")[-10000:], file=sys.stderr)
            for path in (jhome / "logs").glob("*.log"):
                print(path.read_text(errors="replace")[-18000:], file=sys.stderr)
            raise
        finally:
            for connection in connections:
                connection.close()
            if daemon.poll() is None:
                os.killpg(daemon.pid, signal.SIGTERM)
                try:
                    daemon.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(daemon.pid, signal.SIGKILL)
                    daemon.wait()
            provider.shutdown()
            log.close()


if __name__ == "__main__":
    main(str(Path(sys.argv[1]).resolve()), sys.argv[2] if len(sys.argv) > 2 else None)
