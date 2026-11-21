#!/usr/bin/env python3
"""Exercise real macOS computer-use tool turns against an isolated AppKit fixture."""

import argparse
import base64
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import select
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time


TOOL = "macos_computer_use"
MODEL = "fixture-model"
ORIGINAL = "JCODE TEXT ORIGINAL"
UPDATED = "JCODE TEXT UPDATED"
DECOY = "JCODE DECOY ORIGINAL"
BLOCKED = 77


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps({"data": [{"id": MODEL, "object": "model"}]}).encode())

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["content-length"])))
        call_id, arguments = self.server.pending
        returned = any(m.get("tool_call_id") == call_id for m in request["messages"])
        if self.path != "/v1/chat/completions" or request.get("model") != MODEL:
            self.send_error(400, "not the fixture route")
            return
        if returned:
            delta = {"role": "assistant", "content": "Fixture turn finished."}
            finish = "stop"
        else:
            names = {tool["function"]["name"] for tool in request.get("tools", [])}
            if TOOL not in names:
                self.send_error(400, "macOS computer-use tool missing from session")
                return
            delta = {"role": "assistant", "tool_calls": [{
                "index": 0, "id": call_id, "type": "function",
                "function": {"name": TOOL, "arguments": json.dumps(arguments)},
            }]}
            finish = "tool_calls"
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        for choice in [{"delta": delta}, {"delta": {}, "finish_reason": finish}]:
            chunk = {"id": "fixture", "object": "chat.completion.chunk", "model": MODEL,
                     "choices": [{"index": 0, **choice}]}
            self.wfile.write(b"data: " + json.dumps(chunk).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")


@contextmanager
def child(command, **kwargs):
    process = subprocess.Popen(command, **kwargs)
    try:
        yield process
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


class Session:
    def __init__(self, sock, provider, root):
        self.sock = sock
        self.reader = sock.makefile("rb")
        self.provider = provider
        self.sequence = 1
        self.send(type="subscribe", id=1, working_dir=str(root))
        self.session_id = None
        for event in self.events(1):
            if event["type"] == "session":
                self.session_id = event["session_id"]
        require(self.session_id, "no session id returned by fixture daemon")

    def send(self, **request):
        self.sock.sendall(json.dumps(request).encode() + b"\n")

    def events(self, request_id):
        deadline = time.monotonic() + 35
        while True:
            remaining = deadline - time.monotonic()
            require(remaining > 0, f"request {request_id} exceeded 35 seconds")
            self.sock.settimeout(remaining)
            line = self.reader.readline()
            require(line, "daemon disconnected before turn completed")
            event = json.loads(line)
            if event["type"] == "error":
                raise RuntimeError(event.get("message", "daemon returned an error"))
            if event["type"] == "done" and event.get("id") == request_id:
                return
            yield event

    def tool(self, action, *, consent=None, **arguments):
        self.sequence += 2
        request_id = self.sequence
        call_id = f"call_fixture_{request_id}"
        self.provider.pending = (call_id, {"action": action, **arguments})
        self.send(type="message", id=request_id, content=f"Run fixture check {request_id}.")
        result, images, asked = None, [], False
        for event in self.events(request_id):
            kind = event["type"]
            if kind == "ask_user_request":
                require(not asked and consent in {"Deny", "Allow for this session"},
                        "unexpected question in synthetic session")
                questions = event["questions"]
                require(event["request_id"] == f"ask-computer-{call_id}"
                        and event["tool_call_id"] == call_id
                        and len(questions) == 1 and questions[0].get("header") == "Computer use"
                        and [o["label"] for o in questions[0]["options"]]
                        == ["Deny", "Allow for this session"], "not the fixture's Jcode consent")
                asked = True
                self.send(type="ask_user_response", id=request_id + 1,
                          request_id=event["request_id"], response={"answers": [{"selected": [consent]}]})
            elif kind == "tool_done" and event["id"] == call_id:
                require(result is None and event["name"] == TOOL, f"unexpected {action} tool result")
                require(not event.get("error"), f"{action} failed: {event.get('output')}")
                result = event["output"]
            elif kind == "side_pane_images" and event["session_id"] == self.session_id:
                for image in event["images"]:
                    if image.get("anchor") == {"kind": "tool_call", "id": call_id}:
                        images.append(image)
        require(result is not None, f"missing {action} tool result")
        require(asked == (consent is not None), f"unexpected Jcode consent count for {action}")
        return result, images

    def receipt(self, action, **arguments):
        output, images = self.tool(action, **arguments)
        require(not images, f"unexpected image from {action}")
        return json.loads(output)


@contextmanager
def session(socket_path, provider, root, daemon):
    deadline = time.monotonic() + 10
    with socket.socket(socket.AF_UNIX) as sock:
        while True:
            try:
                sock.connect(str(socket_path))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                require(daemon.poll() is None and time.monotonic() < deadline,
                        "isolated daemon did not bind its socket")
                time.sleep(0.05)
        active = Session(sock, provider, root)
        try:
            yield active
        finally:
            active.reader.close()


def fixture_line(fixture):
    require(select.select([fixture.stdout], [], [], 10)[0], "fixture did not reply (check Aqua session)")
    line = fixture.stdout.readline().strip()
    require(line, "fixture exited (check Aqua session and WindowServer)")
    return line


def state(fixture):
    fixture.stdin.write("state\n")
    fixture.stdin.flush()
    return fixture_line(fixture).split("|")


def check_snapshot(snapshot, fixture_pid, window, value):
    require(not snapshot.get("reason"), f"AX observation failed: {snapshot}")
    target = snapshot.get("target", {})
    require(target.get("pid") == fixture_pid and target.get("window_id") == window,
            "AX result did not bind to the fixture PID and target window")
    rows = snapshot.get("tree", [])
    matches = [row for row in rows if "Acceptance input" in row and value in row]
    require(len(matches) == 1 and not any("JCODE DECOY" in row for row in rows),
            f"AX did not read the expected fixture value: {rows}")
    handle = matches[0].split()[0]
    require(re.fullmatch(r"e[0-9a-f]+", handle) and snapshot.get("snapshot_id"), "missing AX handle")
    return handle


def status(active):
    output, images = active.tool("check_permissions", consent="Allow for this session")
    require(not images, "permission status must not capture an image")
    fields = {}
    for line in output.splitlines():
        label = line.split("(", 1)[0].strip()
        value = line.split(":", 1)[-1].strip()
        if label in {"Accessibility", "Screen Recording"}:
            require(value in {"granted", "NOT granted"}, f"ambiguous setup status: {line}")
            fields[label] = value == "granted"
    require(len(fields) == 2, f"setup status did not report both permissions: {output}")
    print("SETUP STATUS (non-interactive preflight):", output.splitlines()[0],
          "|", output.splitlines()[1], flush=True)
    return fields["Accessibility"], fields["Screen Recording"]


def live(active, fixture, binary, root, ax, screen):
    window, decoy = map(int, fixture_line(fixture).split("|"))
    require(window > 0 and decoy > 0 and window != decoy, "fixture windows lack distinct WindowServer IDs")
    scope = {"app": f"pid:{fixture.pid}", "window_id": window}
    original = [ORIGINAL, DECOY, "false"]
    require(state(fixture) == original, "fixture must start in the background with original values")
    snapshot = active.receipt("observe", view="ax", **scope)
    for _ in range(5):
        if snapshot.get("reason") not in {"window_not_found", "window_changed"}:
            break
        time.sleep(0.2)
        snapshot = active.receipt("observe", view="ax", **scope)
    if not ax:
        reason = "accessibility_permission_required"
        require(snapshot.get("reason") == reason, f"AX status disagrees with observe: {snapshot}")
        image_refusal = active.receipt("observe", view="image", **scope)
        require(image_refusal.get("reason") == reason, "image observation did not stop at the AX gate")
        action = active.receipt("act", verb="set_value", snapshot_id="none", element_ref="none", value=UPDATED)
        require(action.get("delivery") == "not_sent" and action.get("reason") == reason,
                f"AX refusal did not prevent dispatch: {action}")
        require(state(fixture) == original, "AX refusal changed the fixture")
        print("PASS AX permission refusal: observe and act blocked, no image, fixture unchanged.")
        print("BLOCKED: grant Accessibility to the macOS TCC identity of the fixture daemon.")
        return BLOCKED

    ref = check_snapshot(snapshot, fixture.pid, window, ORIGINAL)
    action = active.receipt("act", verb="set_value", snapshot_id=snapshot["snapshot_id"],
                            element_ref=ref, value=UPDATED)
    require(action.get("delivery") == "sent" and action.get("effect") == "verified", f"AX update failed: {action}")
    after = [UPDATED, DECOY, "false"]
    require(state(fixture) == after, "AX update activated the fixture or changed the decoy")
    updated = action.get("next_state") or {}
    check_snapshot(updated, fixture.pid, window, UPDATED)
    miss = active.receipt("wait", snapshot_id=updated["snapshot_id"], predicate="exists",
                          query=ORIGINAL, timeout_ms=150)
    require(miss.get("matched") is False and miss.get("reason") == "timeout", "wait matched the old text")
    wait = active.receipt("wait", snapshot_id=updated["snapshot_id"], predicate="exists",
                          query=UPDATED, timeout_ms=1500)
    require(wait.get("matched") is True, f"wait did not observe the changed text: {wait}")
    check_snapshot(active.receipt("observe", view="ax", **scope), fixture.pid, window, UPDATED)
    print("PASS AX observe, background act, wait, and independent readback; decoy unchanged.", flush=True)

    output, images = active.tool("observe", view="image", **scope)
    capture, _ = json.JSONDecoder().raw_decode(output)
    if not screen:
        require(capture.get("reason") == "screen_recording_permission_required" and not images,
                f"Screen Recording status disagrees with capture refusal: {capture}")
        print("PASS Screen Recording refusal: no image was returned.")
        print("BLOCKED: grant Screen & System Audio Recording to the macOS TCC identity of the fixture daemon.")
        return BLOCKED

    frame = capture.get("frame") or {}
    require(frame.get("window_id") == window and frame.get("width_pixels", 0) > 0
            and frame.get("height_pixels", 0) > 0 and len(images) == 1,
            f"window capture did not return one image, matching frame and pixel sizes: {capture}")
    image = images[0]
    suffixes = {"image/png": ".png", "image/jpeg": ".jpg"}
    require(image["media_type"] in suffixes, "unknown capture image format")
    image_path = root / ("window-capture" + suffixes[image["media_type"]])
    image_path.write_bytes(base64.b64decode(image["data"], validate=True))
    ocr = subprocess.check_output([binary, image_path], text=True, timeout=20).upper()
    require("JCODE ACCEPTANCE WINDOW" in ocr and UPDATED in ocr and "JCODE DECOY" not in ocr,
            "captured window OCR did not contain the current fixture text, or included the decoy")
    require(state(fixture) == after, "window capture activated or changed the fixture")
    print("PASS window-scoped screenshot and Vision OCR on the returned image only.")
    return 0


def run(args, root, log):
    path = Path(args.jcode).resolve(strict=True)
    running_user = os.environ.get("USER", "")
    version = subprocess.check_output([path, "--version"], text=True, timeout=5).strip()
    print(f"Jcode: {version} ({path}); uid={os.getuid()}", flush=True)
    jcode, home, runtime = (root / p for p in ("jcode", "home", "run"))
    for directory in (jcode, home, runtime):
        directory.mkdir()
    socket_path = root / "j.sock"
    with ThreadingHTTPServer(("127.0.0.1", 0), Provider) as provider:
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        try:
            env = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(home), "USER": running_user,
                   "JCODE_HOME": str(jcode), "JCODE_RUNTIME_DIR": str(runtime), "TMPDIR": str(runtime),
                   "JCODE_TEST_SESSION": "1", "JCODE_NO_TELEMETRY": "1", "JCODE_TELEMETRY": "off",
                   "JCODE_NO_AUTO_UPDATE": "1", "OPENAI_COMPAT_API_KEY": "fixture-only",
                   "JCODE_OPENAI_COMPAT_DEFAULT_MODEL": MODEL,
                   "JCODE_OPENAI_COMPAT_API_BASE": f"http://127.0.0.1:{provider.server_port}/v1"}
            command = [path, "--no-update", "--no-selfdev", "--provider", "openai-compatible", "--model", MODEL,
                       "--tools", TOOL, "--socket", socket_path, "serve"]
            with child(command, cwd=root, env=env, stdout=log, stderr=log) as daemon:
                print(f"Isolated executor PID: {daemon.pid}", flush=True)
                with session(socket_path, provider, root, daemon) as denied:
                    receipt = denied.receipt("check_permissions", consent="Deny")
                    require(receipt.get("reason") == "consent_denied", f"Jcode consent was not honored: {receipt}")
                    require(denied.receipt("check_permissions").get("reason") == "consent_denied",
                            "Jcode denial did not persist in the same session")
                    print("PASS session consent denied before any permission probe.")
                if not args.allow_fixture:
                    print("BLOCKED: positive path needs --allow-fixture for a new synthetic session.")
                    return BLOCKED
                with session(socket_path, provider, root, daemon) as allowed:
                    ax, screen = status(allowed)
                    binary = root / "jcode-ax-fixture"
                    swift = Path(__file__).resolve().parent / "fixtures/macos-computer-acceptance.swift"
                    subprocess.run(["/usr/bin/swiftc", swift, "-o", binary], check=True, timeout=90)
                    with child([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=log, text=True) as fixture:
                        return live(allowed, fixture, binary, root, ax, screen)
        finally:
            provider.shutdown()
            thread.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jcode", default=shutil.which("jcode"), help="installed/pinned Jcode executable")
    parser.add_argument("--allow-fixture", action="store_true",
                        help="allow the synthetic session to target only its two fixture windows")
    args = parser.parse_args()
    if sys.platform != "darwin" or not args.jcode:
        print("BLOCKED: macOS and a Jcode executable are required.", file=sys.stderr)
        return BLOCKED
    with tempfile.TemporaryDirectory(prefix="jcode-ax-", dir="/tmp") as temp:
        root = Path(temp).resolve()
        log_path = root / "fixture-daemon.log"
        with log_path.open("w") as log:
            try:
                return run(args, root, log)
            except (RuntimeError, ValueError, OSError, subprocess.SubprocessError) as error:
                print(f"FAIL: {error}", file=sys.stderr)
                lines = log_path.read_text(errors="replace").splitlines()
                print("\n".join(lines[-12:]), file=sys.stderr)
                return 1


if __name__ == "__main__":
    raise SystemExit(main())
