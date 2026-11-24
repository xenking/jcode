#!/usr/bin/env python3
"""Isolated acceptance for the routing service in the daemon.

Run with a freshly built jcode binary as the only argument. A fixture
OpenAI-compatible provider stands in for OpenCode Go: it serves chat, models and
the per-key `/usage` windows the routing meters read, and it records which key
each request used. No real provider, shared daemon or user session is touched.
Every scenario gets its own JCODE_HOME and socket.

Covers: mode off is untouched, shadow logs and changes nothing, auto moves a
fresh session off a spent account, a gate round-trips over the socket, a
non-interactive run is blocked instead of gated, a 429 re-decides onto another
account, and `routing_intent status` answers.
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



class Fixture(BaseHTTPRequestHandler):
    used = {"key-main": 10, "key-second": 10}  # percent, per key
    limited = set()  # keys answered with a 429 usage-limit exhaustion
    throttled = set()  # keys answered with a plain 429 rate limit
    seen = []  # keys of chat requests, in order
    bodies = []  # request bodies, same order
    lock = threading.Lock()

    def log_message(self, *args):
        pass

    def key(self):
        return (self.headers.get("authorization") or "").removeprefix("Bearer ").strip()

    def reply(self, status, value):
        data = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/usage"):
            percent = self.used.get(self.key(), 0)
            window = {"status": "ok", "percent": percent, "resetsAt": "2099-01-01T00:00:00.000Z"}
            return self.reply(200, {"usage": {"rolling": window}})
        self.reply(200, {"data": [{"id": "kimi-k2.5", "object": "model"}]})

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0))).decode("utf-8", "replace")
        key = self.key()
        with self.lock:
            self.seen.append(key)
            self.bodies.append(body)
        if key in self.throttled:
            return self.reply(429, {"error": {"message": "Too many requests, rate limit exceeded", "code": "rate_limit_exceeded"}})
        if key in self.limited:
            return self.reply(429, {"error": {"message": "usage_limit_reached", "code": "usage_limit_reached"}})
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        text = f"served by {key}"
        chunks = [{"index": 0, "delta": {"role": "assistant", "content": text}}, {"index": 0, "delta": {}, "finish_reason": "stop"}]
        for choice in chunks:
            self.wfile.write(b"data: " + json.dumps({"id": "fx", "object": "chat.completion.chunk", "model": "kimi-k2.5", "choices": [choice]}).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


class Client:
    def __init__(self, path):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(30)
        self.socket.connect(path)
        self.reader = self.socket.makefile("rb")
        self.events = []

    def send(self, **request):
        self.socket.sendall(json.dumps(request).encode() + b"\n")

    def until(self, predicate):
        while True:
            line = self.reader.readline()
            if not line:
                raise AssertionError("daemon disconnected before the expected event")
            event = json.loads(line)
            self.events.append(event)
            if predicate(event):
                return event

    def close(self):
        self.reader.close()
        self.socket.close()


def types(client):
    return [event.get("type") for event in client.events]


class Daemon:
    def __init__(self, binary, root, port, config):
        self.root = root
        home, runtime, jcode = (root / name for name in ("home", "run", "jcode"))
        for path in (home, runtime, jcode):
            path.mkdir()
        self.jcode = jcode
        self.sock = str(runtime / "j.sock")
        (jcode / "config.toml").write_text(config.replace("PORT", str(port)))
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home), "USER": "test", "TERM": "xterm-256color",
            "TMPDIR": str(runtime), "XDG_RUNTIME_DIR": str(runtime), "JCODE_HOME": str(jcode),
            "JCODE_RUNTIME_DIR": str(runtime), "JCODE_SOCKET": self.sock, "JCODE_TEST_SESSION": "1",
            "OPENCODE_GO_API_KEY": "key-main", "JCODE_NO_TELEMETRY": "1", "JCODE_TELEMETRY": "off", "JCODE_NO_AUTO_UPDATE": "1",
        }
        self.base = [str(pathlib.Path(binary).resolve()), "--no-update", "--no-selfdev", "--provider", "opencode-go", "--model", "kimi-k2.5"]

    def cli(self, *args, check=True):
        result = subprocess.run(self.base + list(args), env=self.env, cwd=self.root, capture_output=True, text=True, timeout=90)
        if check and result.returncode:
            raise AssertionError(f"{args}: {result.stdout} {result.stderr}")
        return result

    def start(self):
        self.cli("server", "start", "--json")

    def stop(self):
        self.cli("server", "stop", check=False)

    def decisions(self):
        path = self.jcode / "routing-decisions.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def session(self):
        client = Client(self.sock)
        client.send(type="subscribe", id=1, working_dir=str(self.root), continue_on_disconnect=True)
        client.until(lambda e: e.get("type") == "done" and e.get("id") == 1)
        return client


CONFIG = """
[routing]
mode = "{mode}"

[[routing.accounts]]
id = "opencode-go"
kind = "subscription"
priority = 1
on_limit = "{on_limit}"

[[routing.accounts]]
id = "opencode-go:second"
kind = "subscription"
priority = 2

[api_key_profiles.opencode-go]
base_url = "http://127.0.0.1:PORT/v1"

[api_key_profiles.opencode-go.keys.second]
key_command = ["echo", "key-second"]
"""


def scenario(binary, port, mode, on_limit="switch"):
    root = pathlib.Path(tempfile.mkdtemp(prefix="jcode-routing-")).resolve()
    daemon = Daemon(binary, root, port, CONFIG.format(mode=mode, on_limit=on_limit))
    Fixture.seen.clear()
    Fixture.bodies.clear()
    return daemon


def turn(client, request_id, text="hello"):
    client.send(type="message", id=request_id, content=text)
    return client.until(lambda e: e.get("type") in ("done", "error") and e.get("id") == request_id)


def check_off(binary, port):
    Fixture.used["key-main"], Fixture.limited = 99, set()
    daemon = scenario(binary, port, "off")
    daemon.start()
    try:
        client = daemon.session()
        done = turn(client, 2)
        assert done["type"] == "done", done
        assert Fixture.seen == ["key-main"], Fixture.seen
        assert not any(str(t).startswith("routing") for t in types(client)), types(client)
        assert not daemon.decisions(), "mode off writes no decision log"
        assert not (daemon.jcode / "routing-health.json").exists()
        print("ok off: spent account is left alone, no routing events or files")
    finally:
        daemon.stop()


def check_shadow(binary, port):
    Fixture.used["key-main"], Fixture.limited = 99, set()
    daemon = scenario(binary, port, "shadow")
    daemon.start()
    try:
        client = daemon.session()
        done = turn(client, 2)
        assert done["type"] == "done", done
        assert Fixture.seen == ["key-main"], f"shadow must not move the session: {Fixture.seen}"
        assert not any(str(t).startswith("routing") for t in types(client)), types(client)
        lines = daemon.decisions()
        assert len(lines) == 1, lines
        line = lines[0]
        assert line["mode"] == "shadow" and line["applied"] is False, line
        assert line["decision"] == "move" and line["account"] == "opencode-go:second", line
        assert line["current"] == "opencode-go" and line["model"] == "kimi-k2.5", line
        assert not (daemon.jcode / "api-key-accounts.json").exists(), "shadow never binds a credential"
        print("ok shadow: decision logged (would move to opencode-go:second), served by key-main")
    finally:
        daemon.stop()


def check_auto_move(binary, port):
    Fixture.used["key-main"], Fixture.limited = 99, set()
    daemon = scenario(binary, port, "auto")
    daemon.start()
    try:
        client = daemon.session()
        done = turn(client, 2)
        assert done["type"] == "done", done
        assert Fixture.seen == ["key-second"], f"fresh session must leave the spent account: {Fixture.seen}"
        decision = next(e for e in client.events if e.get("type") == "routing_decision")
        assert decision["account"] == "opencode-go:second", decision
        lines = daemon.decisions()
        assert lines[-1]["applied"] is True and lines[-1]["decision"] == "move", lines
        print("ok auto: fresh session moved off the spent account and was served by key-second")
        health = daemon.jcode / "routing-health.json"
        assert health.exists(), "burn samples are persisted"
        status = Client(daemon.sock)
        status.send(type="subscribe", id=1, working_dir=str(daemon.root), continue_on_disconnect=True)
        status.until(lambda e: e.get("type") == "done" and e.get("id") == 1)
        status.send(type="routing_intent", id=9, action={"action": "status"})
        text = status.until(lambda e: e.get("type") == "routing_status")["text"]
        assert "routing: auto" in text and "opencode-go:second" in text, text
        print("ok status: routing_intent status answered with text")
    finally:
        daemon.stop()


def check_gate(binary, port):
    Fixture.used["key-main"], Fixture.limited = 99, set()
    daemon = scenario(binary, port, "auto", on_limit="ask")
    daemon.start()
    try:
        client = daemon.session()
        client.send(type="message", id=2, content="hello")
        gate = client.until(lambda e: e.get("type") == "routing_gate")
        offered = [o for o in gate["options"] if o["kind"] == "switch"]
        assert any(o["account"] == "opencode-go:second" for o in offered), gate
        assert not Fixture.seen, "the turn is held until the human answers"
        wrong = Client(daemon.sock)
        wrong.send(type="subscribe", id=1, working_dir=str(daemon.root), continue_on_disconnect=True)
        wrong.until(lambda e: e.get("type") == "done" and e.get("id") == 1)
        wrong.send(type="routing_gate_answer", id=3, gate_id=gate["gate_id"], choice={"kind": "cancel"})
        refused = wrong.until(lambda e: e.get("type") == "error")
        assert "No routing gate" in refused["message"], refused
        client.send(type="routing_gate_answer", id=4, gate_id=gate["gate_id"], choice={"kind": "switch", "account": "opencode-go:second"})
        done = client.until(lambda e: e.get("type") == "done" and e.get("id") == 2)
        assert Fixture.seen == ["key-second"], Fixture.seen
        assert any(e.get("type") == "routing_gate_closed" and e["gate_id"] == gate["gate_id"] for e in client.events), types(client)
        print("ok gate: held, answered over the socket, re-decided onto opencode-go:second")
    finally:
        daemon.stop()


def check_headless_block(binary, port):
    Fixture.used["key-main"], Fixture.limited = 99, set()
    daemon = scenario(binary, port, "auto", on_limit="ask")
    daemon.start()
    try:
        result = daemon.cli("run", "hello", check=False)
        output = result.stdout + result.stderr
        assert "Routing blocked this turn" in output, output
        assert not Fixture.bodies, f"a blocked turn never reaches the provider: {Fixture.seen}"
        lines = daemon.decisions()
        assert lines and lines[-1]["decision"] == "block", lines
        print("ok block: a non-interactive run is blocked with the reason, not gated")
    finally:
        daemon.stop()


def check_subagent_block(binary, port):
    Fixture.used["key-main"], Fixture.limited = 99, set()
    daemon = scenario(binary, port, "auto", on_limit="ask")
    daemon.start()
    try:
        client = daemon.session()
        session = next(e for e in client.events if e.get("type") == "session")["session_id"]
        client.send(type="comm_spawn", id=5, session_id=session, initial_message="WORKER-PROMPT", spawn_mode="headless")
        client.until(lambda e: e.get("type") == "comm_spawn_response" and e.get("id") == 5)
        import time
        for _ in range(60):
            if daemon.decisions():
                break
            time.sleep(0.5)
        line = daemon.decisions()[-1]
        assert line["role"] == "subagent" and line["decision"] == "block", line
        assert not any("WORKER-PROMPT" in body for body in Fixture.bodies), "a blocked worker turn never reaches the provider"
        assert not any(e.get("type") == "routing_gate" for e in client.events), "a subagent is never gated"
        print("ok subagent: the spawned worker got Block, no gate, no provider request")
    finally:
        daemon.stop()


def check_failure_redecides(binary, port):
    Fixture.used["key-main"], Fixture.used["key-second"] = 10, 10
    Fixture.limited = {"key-main"}
    daemon = scenario(binary, port, "auto")
    daemon.start()
    try:
        client = daemon.session()
        done = turn(client, 2)
        assert done["type"] == "done", done
        # The provider retries a 429 on its own first, then routing takes over.
        assert Fixture.seen[0] == "key-main" and Fixture.seen[-1] == "key-second", f"429 re-decides onto the other account: {Fixture.seen}"
        assert set(Fixture.seen[:-1]) == {"key-main"}, Fixture.seen
        assert any(e.get("type") == "routing_decision" and e["account"] == "opencode-go:second" for e in client.events), types(client)
        health = json.loads((daemon.jcode / "routing-health.json").read_text())
        assert "opencode-go" in health["health"], health
        print("ok failure: 429 on key-main evicted it and the turn finished on key-second")
    finally:
        daemon.stop()
        Fixture.limited = set()


def check_rate_limit_reaches_routing_first(binary, port):
    Fixture.used["key-main"], Fixture.used["key-second"] = 10, 10
    Fixture.limited, Fixture.throttled = set(), {"key-main"}
    daemon = scenario(binary, port, "auto")
    daemon.start()
    try:
        client = daemon.session()
        done = turn(client, 2)
        assert done["type"] == "done", done
        # A plain rate limit used to be retried on the limited key about eight
        # times, with backoff, before routing saw it. With another account
        # ready it must reach routing on the first request.
        assert Fixture.seen == ["key-main", "key-second"], f"one request to the limited key: {Fixture.seen}"
        assert any(e.get("type") == "routing_decision" and e["account"] == "opencode-go:second" for e in client.events), types(client)
        print("ok rate limit: one request to the limited key, then the turn finished on key-second")
    finally:
        daemon.stop()
        Fixture.throttled = set()


def check_concurrent_sessions(binary, port):
    Fixture.used["key-main"], Fixture.used["key-second"] = 10, 10
    Fixture.limited = set()
    daemon = scenario(binary, port, "auto")
    daemon.start()
    try:
        one, two = daemon.session(), daemon.session()
        ids = {name: next(e for e in client.events if e.get("type") == "session")["session_id"] for name, client in (("one", one), ("two", two))}
        one.send(type="routing_intent", id=8, action={"action": "switch", "account": "opencode-go:second"})
        one.until(lambda e: e.get("type") == "routing_status")
        def run(client, request_id):
            done = turn(client, request_id)
            assert done["type"] == "done", done

        for round_id in (2, 3):
            threads = [threading.Thread(target=run, args=(client, round_id)) for client in (one, two)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
        # Each turn answers with the key that served it, so read it off the stream.
        def served_by(client):
            return [e.get("text", "") for e in client.events if e.get("type") == "text_delta"]

        assert all("key-second" in text for text in served_by(one)) and served_by(one), served_by(one)
        assert all("key-main" in text for text in served_by(two)) and served_by(two), served_by(two)
        assert sorted(Fixture.seen) == ["key-main"] * 2 + ["key-second"] * 2, Fixture.seen
        lines = [line for line in daemon.decisions() if line["session"] in ids.values()]
        by_session = {name: [line for line in lines if line["session"] == session] for name, session in ids.items()}
        later = by_session["one"][-1]
        assert later["current"] == "opencode-go:second" and later["decision"] == "stay", f"session one keeps its own key: {later}"
        later = by_session["two"][-1]
        assert later["current"] == "opencode-go" and later["decision"] == "stay", f"session two keeps its own key: {later}"
        assert not (daemon.jcode / "api-key-accounts.json").exists(), "routing never rewrites the global active key"
        print("ok concurrent: two sessions served from different keys, global active key untouched")
    finally:
        daemon.stop()


def main(binary):
    server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port = server.server_port
    for check in (check_off, check_shadow, check_auto_move, check_gate, check_headless_block, check_subagent_block, check_failure_redecides, check_concurrent_sessions, check_rate_limit_reaches_routing_first):
        check(binary, port)
    print("routing daemon acceptance: PASS")


if __name__ == "__main__":
    main(sys.argv[1])
