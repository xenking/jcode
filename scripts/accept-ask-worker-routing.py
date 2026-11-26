#!/usr/bin/env python3
"""Isolated acceptance: a headless worker's ask waits for a person, however long.

Run with the freshly built jcode binary. A worker is spawned from a root session;
its fake model asks a question. With no client attached the ask stays pending
(nothing answers it), a client attaching to the root is shown the card, its
answer resumes the worker, and a forced daemon exec reload while the ask is
pending brings the card back on the next client. No real provider, shared
daemon or user sessions are used.
"""
import json
import os
import pathlib
import runpy
import subprocess
import sys
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer

fixture = runpy.run_path(str(pathlib.Path(__file__).with_name("accept-ask-reload.py")))
Client = fixture["Client"]
BaseProvider = fixture["Provider"]


class Provider(BaseProvider):
    requests = []

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["content-length"])))
        messages = request.get("messages", [])
        self.requests.append(messages)
        text = json.dumps(messages)
        worker = "WORKER-TASK" in text
        has_result = any(m.get("role") == "tool" for m in messages)
        if worker and not has_result:
            args = {"intent": "fixture", "questions": [{"question": "Choose?", "options": [{"label": "Alpha (Recommended)"}, {"label": "Beta"}]}]}
            delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": "call-worker-ask", "type": "function", "function": {"name": "ask", "arguments": json.dumps(args)}}]}
            finish = "tool_calls"
        else:
            delta = {"role": "assistant", "content": "worker continued" if worker else "ok"}
            finish = "stop"
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        for choice in [{"index": 0, "delta": delta}, {"index": 0, "delta": {}, "finish_reason": finish}]:
            self.wfile.write(b"data: " + json.dumps({"id": "fake", "object": "chat.completion.chunk", "model": "fake-model", "choices": [choice]}).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def main(binary, out):
    out = pathlib.Path(out)
    out.mkdir(parents=True, exist_ok=True)
    log = (out / "proof.log").open("w")

    def say(line):
        print(line)
        log.write(line + "\n")
        log.flush()

    provider = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=provider.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="jcode-askw-", dir="/tmp") as temp:
        root = pathlib.Path(temp).resolve()
        home, runtime, jcode = (root / name for name in ("home", "run", "jcode"))
        for path in (home, runtime, jcode):
            path.mkdir()
        sock = str(runtime / "j.sock")
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home), "USER": "test", "TERM": "xterm-256color", "TMPDIR": str(runtime), "XDG_RUNTIME_DIR": str(runtime), "JCODE_HOME": str(jcode), "JCODE_RUNTIME_DIR": str(runtime), "JCODE_SOCKET": sock, "JCODE_OPENAI_COMPAT_API_BASE": f"http://127.0.0.1:{provider.server_port}/v1", "OPENAI_COMPAT_API_KEY": "fixture-only", "JCODE_OPENAI_COMPAT_DEFAULT_MODEL": "fake-model", "JCODE_NO_TELEMETRY": "1", "JCODE_TELEMETRY": "off", "JCODE_NO_AUTO_UPDATE": "1"}
        base = [str(pathlib.Path(binary).resolve()), "--no-update", "--no-selfdev", "--provider", "openai-compatible", "--model", "fake-model"]

        def cli(*args):
            result = subprocess.run(base + list(args), env=env, cwd=root, capture_output=True, text=True, timeout=60)
            if result.returncode:
                raise AssertionError(f"{args}: {result.stdout} {result.stderr}")
            return result

        try:
            cli("server", "start", "--json")
            # The root session attaches, spawns a headless worker, then detaches.
            first = Client(sock)
            first.send(type="subscribe", id=1, working_dir=str(root), continue_on_disconnect=True)
            rootid = first.until(lambda e: e.get("type") == "session")["session_id"]
            first.until(lambda e: e.get("type") == "done" and e.get("id") == 1)
            first.send(type="comm_spawn", id=2, session_id=rootid, working_dir=str(root), initial_message="WORKER-TASK ask the owner", label="asker")
            first.until(lambda e: e.get("type") in ("comm_spawn_response", "done") and e.get("id") == 2)
            first.close()
            say(f"root={rootid} worker spawned; root client detached")
            time.sleep(6)
            asks = [m for m in Provider.requests if "WORKER-TASK" in json.dumps(m)]
            assert asks, "the worker never reached the model"
            say("1. no client attached: worker asked and is parked; waiting 20s with nobody to answer")
            time.sleep(20)
            worker_calls = [m for m in Provider.requests if "WORKER-TASK" in json.dumps(m)]
            assert not any(any(x.get("role") == "tool" for x in m) for m in worker_calls), "something answered for the owner"
            say("   PASS: worker stayed pending, no tool result was ever produced")

            second = Client(sock)
            second.send(type="subscribe", id=3, target_session_id=rootid, working_dir=str(root), continue_on_disconnect=True)
            card = second.until(lambda e: e.get("type") == "ask_user_request")
            assert card["tool_call_id"] == "call-worker-ask" and "Worker" in card["questions"][0]["question"], card
            say(f"2. root client attached: card appeared: {card['questions'][0]['question']!r}")
            second.close()
            say("3. client detached again; forcing a daemon exec reload while the ask is pending")
            # The CLI gives up waiting for the handoff before a daemon that drains a
            # parked worker is ready, so only the request matters; readiness is
            # proven by a client getting the card from the new process.
            reload = subprocess.run(base + ["server", "reload", "--force", "--json"], env=env, cwd=root, capture_output=True, text=True, timeout=240)
            say(f"   reload exit={reload.returncode}")
            deadline = time.time() + 180
            while True:
                try:
                    third = Client(sock)
                    third.send(type="subscribe", id=4, target_session_id=rootid, working_dir=str(root), continue_on_disconnect=True)
                    card = third.until(lambda e: e.get("type") == "ask_user_request")
                    break
                except (OSError, AssertionError):
                    assert time.time() < deadline, "the card never reappeared after reload"
                    time.sleep(2)
            assert card["tool_call_id"] == "call-worker-ask", card
            say("   PASS: after reload the card reappeared on the root client")
            third.send(type="ask_user_response", id=5, request_id=card["request_id"], response={"answers": [{"selected": ["Beta"]}]})
            third.until(lambda e: e.get("type") == "done" and e.get("id") == 5)
            deadline = time.time() + 30
            while time.time() < deadline:
                done = [m for m in Provider.requests if "WORKER-TASK" in json.dumps(m) and any(x.get("role") == "tool" and "Beta" in str(x.get("content")) for x in m)]
                if done:
                    break
                time.sleep(0.5)
            assert done, "the worker did not continue with the answer"
            say("4. answered Beta: worker continued with the owner's answer in its next model request")
            say("PASS")
        finally:
            for name in ("j.sock",):
                pass
            subprocess.run(base + ["server", "stop", "--force", "--json"], env=env, cwd=root, capture_output=True, text=True, timeout=60)
            provider.shutdown()
            for path in (jcode / "logs").glob("*.log") if (jcode / "logs").exists() else []:
                (out / path.name).write_text(path.read_text(errors="replace"))


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit(f"usage: {sys.argv[0]} /path/to/jcode /tmp/w5-ask-proof")
    main(sys.argv[1], sys.argv[2])
