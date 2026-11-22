#!/usr/bin/env python3
"""Deterministic OpenAI-compatible provider for isolated acceptance tests.

Only the model is substituted: tests still use Jcode's HTTP client, tools,
transcripts, daemons, SSH transport and TUI. Optional file gates let a test
hold a real streamed turn while submitting /cloud, without timing sleeps.
"""
import json
import shlex
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
PREMIUM_MODEL = os.environ.get("FAKE_PREMIUM_MODEL")
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
        models = ["fake-model"] + ([PREMIUM_MODEL] if PREMIUM_MODEL else [])
        body = json.dumps({"data": [{"id": model, "object": "model"} for model in models]}).encode()
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
                                  "model": req.get("model"), "reasoning_effort": req.get("reasoning_effort"),
                                  "tools": req.get("tools", []),
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
        elif CONTROL and "[e2e:line-anchors]" in prompt:
            replies = {m.get("tool_call_id"): text_of(m) for m in msgs[last_user + 1:] if m.get("role") == "tool"}
            def call(key, name, **args):
                return [{"id": key, "type": "function", "function": {"name": name,
                    "arguments": json.dumps(dict(args, intent="Verify anchored file edit"))}}]
            if "call_find" not in replies:
                tool_calls = call("call_find", "agentgrep", mode="grep", path="verify-anchor.txt", query="repeat")
            elif (match := re.search(r"\[edit anchor (L2-[a-f0-9]+)\] repeat", replies["call_find"])):
                if "call_apply" not in replies:
                    tool_calls = call("call_apply", "edit", file_path="verify-anchor.txt", anchor=match[1],
                                      edits=[{"old_string": "repeat", "new_string": "selected"}])
                elif "call_stale" not in replies:
                    tool_calls = call("call_stale", "edit", file_path="verify-anchor.txt", anchor=match[1],
                                      edits=[{"old_string": "selected", "new_string": "unexpected"}])
                elif "call_reread" not in replies:
                    tool_calls = call("call_reread", "read", file_path="verify-anchor.txt")
        elif CONTROL and "[e2e:read-overlap]" in prompt and not has_tool_result:
            tool_calls = [{"id": "call_" + name, "type": "function", "function": {"name": "read",
                "arguments": json.dumps({"file_path": name + ".txt", "start_line": 220001, "limit": 1,
                                         "intent": "Check overlapping native reads"})}} for name in ("left", "right")]
        elif CONTROL and "[e2e:read-window]" in prompt and not has_tool_result:
            tool_calls = [{"id": "call_window", "type": "function", "function": {
                "name": "read", "arguments": json.dumps({"file_path": "verify-large.txt", "start_line": 1,
                    "limit": 1, "intent": "Read the first line of the large fixture"})}}]
        elif CONTROL and "[e2e:file-tools]" in prompt and not has_tool_result:
            read = {"file_path": "verify-window.txt", "limit": 1}
            calls = [("before", "read", read), ("past_eof", "read", dict(read, start_line=1000)),
                     ("change", "edit", {"file_path": read["file_path"],
                        "edits": [{"old_string": "initial", "new_string": "updated"}]}),
                     ("after", "read", read)]
            tool_calls = [{"id": f"call_{key}", "type": "function", "function": {
                "name": name, "arguments": json.dumps(dict(args, intent=f"Verify file contract: {key}"))}}
                for key, name, args in calls]
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
        elif CONTROL and not has_tool_result and (match := re.search(r"\[e2e:reload-bg:([a-z0-9_]+)\]", prompt)):
            prefix = CONTROL / match.group(1)
            child, progress, release = [shlex.quote(str(prefix.with_suffix(suffix)))
                                        for suffix in (".child", ".progress", ".release")]
            command = (
                f"printf '%s\\n' \"$$\" > {child}; "
                f"while [ ! -f {progress} ]; do sleep 0.1; done; "
                "echo 'JCODE_PROGRESS {\"message\":\"post-exec progress\",\"percent\":50}'; "
                f"while [ ! -f {release} ]; do sleep 0.1; done; "
                "echo 'completed by original shell'; exit "
                + ("0" if "verify-bg-success" in prompt else "23")
            )
            tool_calls = [{"id": "call_reload_background", "type": "function", "function": {
                "name": "bash", "arguments": json.dumps({"intent": "Verify background exec reload",
                    "command": command, "run_in_background": True, "notify": True,
                    "wake": "verify-no-bg-wake" not in prompt, "timeout": 60000})}}]
        elif CONTROL and not has_tool_result and "[e2e:reload-wait-latest]" in prompt:
            tool_calls = [{"id": "call_reload_wait", "type": "function", "function": {
                "name": "bg", "arguments": json.dumps({"action": "wait", "latest": True,
                    "intent": "Verify original wait survives daemon exec", "max_wait_seconds": 30,
                    "return_on_progress": False})}}]
        elif CONTROL and not has_tool_result and (match := re.search(r"\[e2e:reload-selfdev:(build|test)\]", prompt)):
            action = match.group(1)
            arguments = {"action": action, "reason": f"verify queued selfdev {action}",
                         "notify": True, "wake": False}
            if action == "build":
                arguments.update(target="tui", release_notes="Verification fixture only.")
            else:
                arguments["command"] = "printf 'verify test fixture\\n'"
            tool_calls = [{"id": f"call_reload_selfdev_{action}", "type": "function", "function": {
                "name": "selfdev", "arguments": json.dumps(arguments)}}]
        elif CONTROL and "[e2e:longtool]" in prompt and not has_tool_result:
            # A tool still running when the daemon dies: the turn must be
            # resumed by the next daemon, with no client sending anything.
            tool_calls = [{"id": "call_verification_longtool", "type": "function", "function": {
                "name": "bash", "arguments": json.dumps({
                    "intent": "Run a long command the daemon will not outlive",
                    "command": "sleep 600",
                })}}]
        elif CONTROL and PREMIUM_MODEL and not has_tool_result and (
                match := re.search(r"\[e2e:premium-spawn:([a-z0-9_]+)\]", prompt)):
            token = match.group(1)
            tool_calls = [{"id": "call_premium_" + token, "type": "function", "function": {
                "name": "swarm", "arguments": json.dumps({
                    "action": "spawn", "intent": "Exercise owner approval on the isolated fake provider",
                    "label": "premium-" + token, "prompt": "premium-child-" + token,
                    "spawn_mode": "headless",
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
        usage_marker = re.search(r"\[e2e:input-tokens:([0-9]{1,7})\]", prompt) if CONTROL else None
        input_tokens = int(usage_marker.group(1)) if usage_marker else 1
        usage = {"prompt_tokens": input_tokens, "completion_tokens": 1, "total_tokens": input_tokens + 1}
        if not req.get("stream"):
            message = {"role": "assistant", "content": None if tool_calls else text}
            if tool_calls:
                message["tool_calls"] = tool_calls
            body = json.dumps({
                "id": "x", "object": "chat.completion", "model": "fake-model",
                "choices": [{"index": 0, "finish_reason": "tool_calls" if tool_calls else "stop",
                             "message": message}],
                "usage": usage,
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
            delta = {"role": "assistant", "tool_calls": [dict(call, index=i) for i, call in enumerate(tool_calls)]}
        event({"id": "x", "object": "chat.completion.chunk", "model": "fake-model",
               "choices": [{"index": 0, "delta": delta}]})
        event({"id": "x", "object": "chat.completion.chunk", "model": "fake-model",
               "choices": [{"index": 0, "delta": {},
                            "finish_reason": "tool_calls" if tool_calls else "stop"}],
               "usage": usage})
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
