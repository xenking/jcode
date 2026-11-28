#!/usr/bin/env python3
"""Verify rollout wait drains and resumes through a real isolated daemon exec."""

import collections
import fcntl
import json
import os
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer


ask_fixture = runpy.run_path(str(Path(__file__).with_name("accept-ask-reload.py")))
Client = ask_fixture["Client"]
ROLLOUT = {"rollout_id": "rollout_wait_fixture", "cli_sha": "1" * 40, "desktop_sha": "2" * 40}


class Provider(ask_fixture["Provider"]):
    calls = collections.Counter()
    prompts = []

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["content-length"])))
        messages = request["messages"]
        text = "\n".join(json.dumps(m.get("content")) for m in messages if m.get("role") == "user")
        self.prompts.append(text[-250:])
        prompt = next((prompt for prompt in ("wait-one", "wait-two", "leader", "worker")
                       if f'"{prompt}"' in text or f'] {prompt}"' in text), None)
        if any(m.get("role") == "tool" for m in messages) or prompt is None:
            delta = {"role": "assistant", "content": "continued"}
            finish = "stop"
        else:
            self.calls[prompt] += 1
            if prompt.startswith("wait"):
                name, args = "rollout_wait", ROLLOUT
            else:
                name, args = "bash", {"intent": "fixture blocker",
                                      "command": f"for ((i=0;i<600;i++)); do [ ! -e {prompt}.release ] || exit 0; sleep 0.05; done; exit 2"}
            delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": f"call-{prompt}",
                     "type": "function", "function": {"name": name, "arguments": json.dumps(args)}}]}
            finish = "tool_calls"
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        for choice in [{"index": 0, "delta": delta}, {"index": 0, "delta": {}, "finish_reason": finish}]:
            event = {"id": "fake", "object": "chat.completion.chunk", "model": "fake-model", "choices": [choice]}
            self.wfile.write(b"data: " + json.dumps(event).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")


def verify(binary, root, port):
    home, runtime, storage = (root / name for name in ("home", "run", "jcode"))
    for path in (home, runtime, storage, storage / "rollout"):
        path.mkdir()
    (storage / "config.toml").write_text("[tools]\nauto_background_timeout_ms = 0\n")
    socket = str(runtime / "j.sock")
    env = {"PATH": os.environ["PATH"], "HOME": str(home), "USER": "test", "TERM": "xterm-256color",
           "TMPDIR": str(runtime), "XDG_RUNTIME_DIR": str(runtime), "JCODE_HOME": str(storage),
           "JCODE_RUNTIME_DIR": str(runtime), "JCODE_SOCKET": socket,
           "JCODE_ROLLOUT_APP": str(root / "Jcode.app"),
           "JCODE_OPENAI_COMPAT_API_BASE": f"http://127.0.0.1:{port}/v1", "OPENAI_COMPAT_API_KEY": "fixture-only",
           "JCODE_OPENAI_COMPAT_DEFAULT_MODEL": "fake-model", "JCODE_NO_TELEMETRY": "1",
           "JCODE_TELEMETRY": "off", "JCODE_NO_AUTO_UPDATE": "1"}
    base = [str(binary.resolve()), "--no-update", "--no-selfdev", "--provider", "openai-compatible",
            "--model", "fake-model"]

    def cli(*args):
        run = subprocess.run(base + list(args), env=env, cwd=root, capture_output=True, text=True, timeout=45)
        assert run.returncode == 0, (args, run.stdout, run.stderr)
        return json.loads(run.stdout)

    def debug(command, expected=True):
        client = Client(str(runtime / "j-debug.sock"))
        try:
            client.send(type="debug_command", id=7, command=f"server:{command}")
            event = client.until(lambda e: e.get("id") == 7)
            assert event.get("ok") == expected, event
            return json.loads(event["output"]) if expected else event["output"]
        finally:
            client.close()

    def subscribe(target=None):
        client = Client(socket)
        request = dict(type="subscribe", id=1, working_dir=str(root), continue_on_disconnect=True)
        if target:
            request["target_session_id"] = target
        client.send(**request)
        events = []
        client.until(lambda e: events.append(e) or (e.get("type") == "done" and e.get("id") == 1))
        return client, events

    clients = []
    journal = storage / "rollout/journal.json"

    def persist(state, path=journal):
        temporary = path.with_suffix(".tmp")
        temporary.write_text(json.dumps(state))
        temporary.replace(path)

    try:
        cli("server", "start", "--json")
        app = str(root / "Jcode.app")
        cli_sha, desktop_sha = ROLLOUT["cli_sha"], ROLLOUT["desktop_sha"]
        old_intent = {"cli": cli_sha[:9], "cli_sha": cli_sha, "desktop": desktop_sha,
                      "preparation_id": "prep_stale", "binary": "/private/new", "bundle": None,
                      "app": app, "now": False, "drain_timeout_secs": 120, "triggering_session": None}
        successor = {"id": "prep_successor", "owner_pid": os.getpid(), "session": None,
                     "cli": cli_sha, "desktop": desktop_sha, "include_desktop": False,
                     "app": app, "socket": socket}
        persist(successor, journal.with_name("preparation.json"))
        rejection = debug("rollout_submit:" + json.dumps(old_intent), expected=False)
        assert "was superseded" in rejection and not journal.exists(), rejection
        sessions = {}
        for prompt in ("leader", "worker", "wait-one", "wait-two"):
            client, events = subscribe()
            clients.append(client)
            sessions[prompt] = next(e["session_id"] for e in events if e.get("type") == "session")
        state = {"schema": 1, "id": ROLLOUT["rollout_id"], "phase": "draining", "owner_pid": os.getpid(),
                 "socket": socket, "previous_shared": "000000000", "previous_binary": "/private/old",
                 "staged_binary": "/private/new", "incoming_app": "/private/incoming", "backup_app": "/private/backup",
                 "backup_ready": False, "desktop_swapped": False, "serving": False, "mutations": [],
                 "intent": {"cli": ROLLOUT["cli_sha"][:9], "cli_sha": ROLLOUT["cli_sha"], "desktop": ROLLOUT["desktop_sha"],
                            "binary": "/private/new", "bundle": None, "app": str(root / "Jcode.app"), "now": False,
                            "drain_timeout_secs": 120, "triggering_session": sessions["leader"]}}
        # This is a synthetic ongoing transaction. Prevent the new daemon
        # from running its executor while testing only the wait/replay path.
        with (storage / "rollout/executor.lock").open("w") as executor:
            fcntl.flock(executor, fcntl.LOCK_EX)
            persist(state)
            for client, prompt in zip(clients, sessions):
                client.send(type="message", id=2, content=prompt)

            deadline = time.monotonic() + 12
            while True:
                snapshot = debug("turn_activity")["activity"]
                activity = {s["session_id"]: s for s in snapshot["sessions"]}
                if (activity.get(sessions["leader"], {}).get("phase") == "in_tool"
                    and activity.get(sessions["worker"], {}).get("phase") == "in_tool"
                    and all(activity.get(sessions[prompt], {}).get("waiting_rollout") == ROLLOUT["rollout_id"]
                            for prompt in ("wait-one", "wait-two"))):
                    break
                assert time.monotonic() < deadline, (activity, Provider.calls, Provider.prompts[-8:])
                time.sleep(0.08)
            status = cli("rollout", "--status")
            blockers = status["blockers"]
            assert status["intent"]["triggering_session"] == sessions["leader"], status
            assert [(b["session_id"], b["reason"]) for b in blockers] == [
                (sessions["worker"], "executing tool bash")], status
            (root / "worker.release").touch()
            clients[1].until(lambda e: e.get("type") == "done" and e.get("id") == 2)
            assert not cli("rollout", "--status")["blockers"]
            (root / "leader.release").touch()
            clients[0].until(lambda e: e.get("type") == "done" and e.get("id") == 2)

            clients[0].send(type="reload", id=3, force=True)
            deadline = time.monotonic() + 12
            while True:
                traces = [event for trace in (storage / "reload-traces").glob("reload_*.jsonl")
                          for line in trace.read_text().splitlines(keepends=True) if line.endswith("\n")
                          for event in [json.loads(line)]]
                execs = [e for e in traces if e.get("phase") == "exec_start"]
                ready = [e for e in traces if e.get("phase") == "socket_ready"]
                resumed = {e.get("session_id") for e in traces
                           if e.get("phase") == "recovery_outcome" and e.get("status") == "started"}
                if execs and ready and {sessions["wait-one"], sessions["wait-two"]} <= resumed:
                    assert execs[0]["pid"] == ready[0]["pid"], (execs, ready)
                    break
                assert time.monotonic() < deadline, traces
                time.sleep(0.08)
            # Store the same transaction's terminal result, then move latest to
            # a different ID to prove replays do not silently follow that one.
            state["phase"] = "completed"
            receipts = journal.parent / "receipts"
            receipts.mkdir()
            persist(state, receipts / (state["id"] + ".json"))
            persist(dict(state, id="rollout_another"))
        deadline = time.monotonic() + 12
        waiter_ids = {sessions["wait-one"], sessions["wait-two"]}
        while waiter_ids.intersection(debug("turn_activity")["busy_sessions"]):
            assert time.monotonic() < deadline, "replayed waiters did not finish"
            time.sleep(0.08)
        for prompt in ("wait-one", "wait-two"):
            client, events = subscribe(sessions[prompt])
            clients.append(client)
            history = next(e for e in events if e.get("type") == "history")
            result = json.dumps(dict(ROLLOUT, phase="completed", error=None), sort_keys=True, separators=(",", ":"))
            rows = [row for row in history["messages"] if result in row.get("content", "")]
            assert len(rows) == 1, history
            assert Provider.calls[prompt] == 1, Provider.calls
        print("PASS: daemon rejected stale prep; two typed waiters recovered the same receipt after real exec")
    finally:
        for name in ("leader", "worker"):
            (root / f"{name}.release").touch()
        for client in clients:
            client.close()
        cli("server", "stop", "--force", "--json")


def main(binary):
    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="jrw-", dir="/tmp") as root:
            verify(Path(binary), Path(root).resolve(), server.server_port)
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main(sys.argv[1])
