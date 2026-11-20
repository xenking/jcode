#!/usr/bin/env python3
"""Deterministic OpenAI-compatible provider for isolated acceptance tests.

Only the model is substituted: tests still use Jcode's HTTP client, tools,
transcripts, daemons, SSH transport and TUI. Optional file gates let a test
hold a real streamed turn while submitting /cloud, without timing sleeps.
"""
import json
import os
from pathlib import Path
import re
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = os.environ.get("FAKE_LOG", "")
LABEL = os.environ.get("FAKE_LABEL", "model")
CONTROL = Path(os.environ["FAKE_CONTROL_DIR"]) if os.environ.get("FAKE_CONTROL_DIR") else None
LOCK = threading.Lock()


def text_of(message):
    content = message.get("content") or ""
    if isinstance(content, str):
        return content
    return "\n".join(part.get("text", "") for part in content if isinstance(part, dict))


class H(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "fake-model", "object": "model"}]}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        msgs = req.get("messages", [])
        with LOCK, open(LOG, "a") as log:
            log.write(json.dumps({"label": LABEL, "messages": msgs, "agent": bool(req.get("tools")),
                                  "time": time.time()}) + "\n")
        flat = json.dumps(msgs)
        seen_notice = "moved from" in flat and "to cloud host" in flat
        text = f"[{LABEL}] ack. saw_cloud_notice={seen_notice}"
        if "[e2e:pad]" in json.dumps(msgs[-1:] if msgs else []):
            # Grow real context so manual /compact clears its usage threshold.
            text += " " + " ".join(f"pad{i}" for i in range(6000))
        last_user = max((i for i, m in enumerate(msgs) if m.get("role") == "user"), default=-1)
        prompt = text_of(msgs[last_user]) if last_user >= 0 else ""
        if ("You just received a direct swarm message" in prompt and last_user > 0
                and "[e2e:worker-step:" in text_of(msgs[last_user - 1])):
            last_user -= 1
            prompt = text_of(msgs[last_user])
        has_tool_result = any(m.get("role") == "tool" for m in msgs[last_user + 1:])
        tool_calls = None
        if CONTROL and "[e2e:verify-env]" in prompt:
            if has_tool_result:
                text = text_of(next(m for m in reversed(msgs) if m.get("role") == "tool"))
            else:
                tool_calls = [{"id": "call_verification_env", "type": "function", "function": {
                    "name": "bash", "arguments": json.dumps({
                        "intent": "Read the isolated seat environment through real Bash",
                        "command": "echo \"$FOO_SEAT\"",
                    })}}]
        elif CONTROL and "[e2e:verify-write]" in prompt and not has_tool_result:
            tool_calls = [{"id": "call_verification_write", "type": "function", "function": {
                "name": "write", "arguments": json.dumps({
                    "intent": "Prove a real tool side effect in the isolated workspace",
                    "file_path": "verify-tool.txt", "content": "verification tool side effect\n",
                })}}]
        elif CONTROL and "[e2e:todo]" in prompt and not has_tool_result:
            # Go through the real tool executor, rather than mutating TUI state.
            tool_calls = [{"id": "call_acceptance_todo", "type": "function", "function": {
                "name": "todo", "arguments": json.dumps({
                    "intent": "Create an open task for the handoff acceptance test",
                    "todos": [{"id": "acceptance-open", "content": "Continue on cloud",
                               "status": "in_progress", "priority": "high", "confidence": "plausible"}],
                })}}]
        elif CONTROL and "[e2e:longtool]" in prompt and not has_tool_result:
            # A tool still running when the daemon dies: the turn must be
            # resumed by the next daemon, with no client sending anything.
            tool_calls = [{"id": "call_verification_longtool", "type": "function", "function": {
                "name": "bash", "arguments": json.dumps({
                    "intent": "Run a long command the daemon will not outlive",
                    "command": "sleep 600",
                })}}]
        elif CONTROL and (match := re.search(r"\[e2e:worker-spawn:([a-z0-9_]+)\]", prompt)) and not has_tool_result:
            tool_calls = [{"id": "call_worker_reload_spawn", "type": "function", "function": {
                "name": "swarm", "arguments": json.dumps({
                    "action": "spawn", "intent": "Spawn the isolated restart verification worker",
                    "label": "verify-reload-child",
                    "prompt": match.group(1) + " Acknowledge. [e2e:worker-finish]",
                    "spawn_mode": "headless",
                })}}]
        elif CONTROL and "[e2e:worker-finish]" in prompt and not has_tool_result:
            tool_calls = [{"id": "call_worker_reload_finish", "type": "function", "function": {
                "name": "todo", "arguments": json.dumps({
                    "intent": "Complete the spawned worker's original lane",
                    "update": [{"name": "verify-reload-child", "status": "completed",
                                "completion_confidence": "verified"}],
                })}}]
        elif CONTROL and (match := re.search(r"\[e2e:worker-dm:([a-z0-9_]+):([a-z0-9_]+)\]", prompt)) and not has_tool_result:
            session_id, text_to_worker = match.groups()
            call_id = "call_worker_reload_dm_" + text_to_worker
            if text_to_worker.endswith("_running"):
                text_to_worker += f" [e2e:worker-step:{text_to_worker.split('_running')[0]}]"
            tool_calls = [{"id": call_id, "type": "function", "function": {
                "name": "swarm", "arguments": json.dumps({
                    "action": "message", "intent": "Send a real DM to the same worker",
                    "to_session": session_id, "message": text_to_worker,
                })}}]
        elif CONTROL and (match := re.search(r"\[e2e:worker-step:([a-z0-9_]+)\]", prompt)):
            token = match.group(1)
            if not has_tool_result:
                tool_calls = [{"id": "call_worker_reload_step", "type": "function", "function": {
                    "name": "edit", "arguments": json.dumps({
                        "intent": "Append one marker in the isolated workspace to detect duplicate execution",
                        "file_path": "worker-reload-step.txt",
                        "old_string": "worker-step-anchor\n",
                        "new_string": f"worker-step-anchor\n{token}\n",
                    })}}]
            elif not self.hold(token):
                return
        elif CONTROL and "[e2e:spawn]" in prompt and not has_tool_result:
            # Real swarm spawn through the tool executor; the child's provider
            # requests land in the same FAKE_LOG.
            tool_calls = [{"id": "call_verification_spawn", "type": "function", "function": {
                "name": "swarm", "arguments": json.dumps({
                    "action": "spawn", "intent": "Prove spawned workers inherit the session mode",
                    "label": "verify-child", "prompt": "[e2e:child] Acknowledge.",
                    "spawn_mode": "headless",
                })}}]
        elif CONTROL and (match := re.search(r"\[e2e:hold:([a-z0-9_-]+)\]", prompt)):
            if not self.hold(match.group(1)):
                return
        if not req.get("stream"):
            message = {"role": "assistant", "content": None if tool_calls else text}
            if tool_calls:
                message["tool_calls"] = tool_calls
            body = json.dumps({
                "id": "x", "object": "chat.completion", "model": "fake-model",
                "choices": [{"index": 0, "finish_reason": "tool_calls" if tool_calls else "stop",
                             "message": message}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            }).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()

        def event(obj):
            self.wfile.write(b"data: " + json.dumps(obj).encode() + b"\n\n")
            self.wfile.flush()

        delta = {"role": "assistant", "content": text}
        if tool_calls:
            delta = {"role": "assistant", "tool_calls": [dict(tool_calls[0], index=0)]}
        event({"id": "x", "object": "chat.completion.chunk", "model": "fake-model",
               "choices": [{"index": 0, "delta": delta}]})
        event({"id": "x", "object": "chat.completion.chunk", "model": "fake-model",
               "choices": [{"index": 0, "delta": {},
                            "finish_reason": "tool_calls" if tool_calls else "stop"}],
               "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def hold(self, token):
        CONTROL.mkdir(parents=True, exist_ok=True)
        (CONTROL / (token + ".held")).write_text("provider request received\n")
        deadline = time.monotonic() + 90
        while not (CONTROL / (token + ".release")).exists():
            if time.monotonic() >= deadline:
                self.send_error(504, "acceptance test did not release provider gate")
                return False
            time.sleep(0.05)
        return True


if __name__ == "__main__":
    if not LOG:
        raise SystemExit("FAKE_LOG must name an isolated request log")
    server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H)
    if os.environ.get("FAKE_PORT_FILE"):
        Path(os.environ["FAKE_PORT_FILE"]).write_text(str(server.server_port))
    server.serve_forever()
