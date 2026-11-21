#!/usr/bin/env python3
"""Exercise real macOS computer-use tool turns against an isolated AppKit fixture."""

import argparse
import base64
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager, ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import select
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
        self.executor_pid = None
        self.send(type="subscribe", id=1, working_dir=str(root))
        self.session_id = None
        for event in self.events(1):
            if event["type"] == "session":
                self.session_id = event["session_id"]
        require(self.session_id, "no session id returned by fixture daemon")

    def send(self, **request):
        self.sock.sendall(json.dumps(request).encode() + b"\n")

    def events(self, request_id, timeout=35):
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            require(remaining > 0, f"request {request_id} exceeded {timeout} seconds")
            self.sock.settimeout(remaining)
            line = self.reader.readline()
            require(line, "daemon disconnected before turn completed")
            event = json.loads(line)
            if event["type"] == "error":
                raise RuntimeError(event.get("message", "daemon returned an error"))
            if event["type"] == "done" and event.get("id") == request_id:
                return
            yield event

    def tool(self, action, *, timeout=35, **arguments):
        self.sequence += 2
        request_id = self.sequence
        call_id = f"call_fixture_{self.session_id[-8:]}_{request_id}"
        self.provider.pending = (call_id, {"action": action, **arguments})
        self.send(type="message", id=request_id, content=f"Run fixture check {request_id}.")
        result, images = None, []
        for event in self.events(request_id, timeout):
            kind = event["type"]
            if kind == "ask_user_request":
                raise RuntimeError("daemon asked for consent instead of deferring to the local Desktop")
            elif kind == "tool_done" and event["id"] == call_id:
                require(result is None and event["name"] == TOOL, f"unexpected {action} tool result")
                require(not event.get("error"), f"{action} failed: {event.get('output')}")
                result = event["output"]
            elif kind == "side_pane_images" and event["session_id"] == self.session_id:
                for image in event["images"]:
                    if image.get("anchor") == {"kind": "tool_call", "id": call_id}:
                        images.append(image)
        require(result is not None, f"missing {action} tool result")
        return result, images

    def receipt(self, action, **arguments):
        output, images = self.tool(action, **arguments)
        require(not images, f"unexpected image from {action}")
        receipt = json.loads(output)
        if self.executor_pid is not None and receipt.get("reason") not in {
            "consent_denied", "consent_required", "desktop_unavailable",
        }:
            self.check_executor(receipt, action)
        return receipt

    def check_executor(self, receipt, action):
        require(receipt.get("executor_pid") == self.executor_pid,
                f"{action} did not run in Desktop PID {self.executor_pid}: {receipt}")


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


def state(fixture, command="state"):
    fixture.stdin.write(command + "\n")
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
    output, images = active.tool("check_permissions")
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


def live(active, fixture, binary, root, scope, snapshot, ax, screen):
    window = scope["window_id"]
    original = [ORIGINAL, DECOY, "false"]
    require(state(fixture) == original, "fixture must start in the background with original values")
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
        print("BLOCKED: grant Accessibility to the isolated Desktop's macOS TCC identity.")
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
    active.check_executor(capture, "image observe")
    if not screen:
        require(capture.get("reason") == "screen_recording_permission_required" and not images,
                f"Screen Recording status disagrees with capture refusal: {capture}")
        print("PASS Screen Recording refusal: no image was returned.")
        print("BLOCKED: grant Screen & System Audio Recording to the isolated Desktop's TCC identity.")
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
    require(state(fixture, "focus-input") == ["ready"], "fixture could not focus its input")
    try:
        active.tool("type", text=" JCODE KEYBOARD")
        expected = UPDATED + " JCODE KEYBOARD"
        require(state(fixture) == [expected, DECOY, "true"], "native keyboard input did not reach the fixture")
        check_snapshot(active.receipt("observe", view="ax", **scope), fixture.pid, window, expected)
    finally:
        state(fixture, "hide")
    print(f"PASS keyboard input and AX readback, Desktop executor PID {active.executor_pid}.")
    return 0


def until(probe, label, process, timeout=20):
    deadline = time.monotonic() + timeout
    while process.poll() is None and time.monotonic() < deadline:
        value = probe()
        if value:
            return value
        time.sleep(0.15)
    raise RuntimeError(f"timed out waiting for {label} (process status: {process.poll()})")


def desktop_request(endpoint, **request):
    try:
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(0.75)
            sock.connect(str(endpoint))
            sock.sendall(json.dumps({"v": 1, **request}).encode() + b"\n")
            with sock.makefile("rb") as reader:
                return json.loads(reader.readline())
    except (OSError, ValueError):
        return {}


def desktop_target(control, session_id, pid):
    targets = []
    for endpoint in control.glob("*.sock"):
        reply = desktop_request(endpoint, action="computer_target", session_id=session_id)
        if reply.get("status") == "ok":
            target = reply["result"]
            require(target["session_id"] == session_id and target["window_id"].startswith(f"{pid}:"),
                    f"target is not the isolated Desktop session: {target}")
            targets.append((endpoint, target))
    require(len(targets) <= 1, f"multiple Desktop windows matched {session_id}")
    return targets[0] if targets else None


def decide(active, fixture, binary, desktop, route, button, action, **arguments):
    endpoint, target = route
    focus = {"action": "focus", "target": target, "allow_user_window": True}
    reply = desktop_request(endpoint, **focus)
    require(reply.get("status") == "ok", f"could not focus test window: {reply}")
    before = state(fixture)[:2]
    with ThreadPoolExecutor(max_workers=1) as worker:
        result = worker.submit(active.receipt, action, timeout=170, **arguments)
        try:
            for _ in range(8):
                if result.done():
                    raise RuntimeError(f"request completed before Desktop consent: {result.result()}")
                probe = subprocess.run([binary, "sheet", str(desktop.pid), active.session_id],
                                       capture_output=True, text=True, timeout=3)
                if probe.returncode in (0, BLOCKED):
                    break
                time.sleep(0.25)
            require(state(fixture)[:2] == before, "fixture changed before consent")
            if probe.stdout.strip():
                print(probe.stdout.strip(), flush=True)
            print(f"PENDING session {active.session_id}, Desktop PID {desktop.pid}; fixture unchanged. "
                  f"Click '{button}' only on the 'Computer use' sheet with that session ID.", flush=True)
            return result.result()
        except BaseException:
            active.sock.shutdown(socket.SHUT_RDWR)
            raise


def run(args, root, log):
    path = Path(args.jcode).resolve(strict=True)
    running_user = os.environ.get("USER", "")
    version = subprocess.check_output([path, "--version"], text=True, timeout=5).strip()
    print(f"Jcode: {version} ({path}); uid={os.getuid()}", flush=True)
    jcode, home, runtime, control, xdg = (root / p for p in ("jcode", "home", "run", "c", "xdg"))
    for directory in (jcode, home, runtime, control, xdg):
        directory.mkdir(mode=0o700)
    socket_path = root / "j.sock"
    api_socket = root / "a.sock"
    config = root / "desktop.toml"
    config.write_text("[workspace]\naccount_sign_in_handled = true\n"
                      "session_refresh_seconds = 5\ncoaching_hints = false\n")
    with ThreadingHTTPServer(("127.0.0.1", 0), Provider) as provider:
        thread = threading.Thread(target=provider.serve_forever, daemon=True)
        thread.start()
        try:
            env = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(home), "USER": running_user,
                   "JCODE_HOME": str(jcode), "JCODE_RUNTIME_DIR": str(runtime), "TMPDIR": str(runtime),
                   "XDG_RUNTIME_DIR": str(xdg), "JCODE_SOCKET": str(socket_path), "JCODE_API_SOCKET": str(api_socket),
                   "JCODE_DESKTOP_CONTROL_DIR": str(control), "JCODE_DESKTOP_CONFIG": str(config),
                   "JCODE_DESKTOP_WORKING_DIR": str(root), "JCODE_DESKTOP_MUTE_SOUNDS": "1",
                   "JCODE_TEST_SESSION": "1", "JCODE_NO_TELEMETRY": "1", "JCODE_TELEMETRY": "off",
                   "JCODE_NO_AUTO_UPDATE": "1", "OPENAI_COMPAT_API_KEY": "fixture-only",
                   "JCODE_OPENAI_COMPAT_DEFAULT_MODEL": MODEL,
                   "JCODE_OPENAI_COMPAT_API_BASE": f"http://127.0.0.1:{provider.server_port}/v1"}
            command = [path, "--no-update", "--no-selfdev", "--provider", "openai-compatible", "--model", MODEL,
                       "--tools", TOOL, "--socket", socket_path, "serve"]
            with ExitStack() as stack:
                def spawn(command, **kwargs):
                    return stack.enter_context(child(command, cwd=root, env=env, stderr=log, **kwargs))

                daemon = spawn(command, stdout=log)
                allowed = stack.enter_context(session(socket_path, provider, root, daemon))
                missing = allowed.receipt("observe", view="ax", app="pid:2147483647")
                require(missing.get("reason") == "desktop_unavailable" and "executor_pid" not in missing,
                        f"a session without Desktop did not fail closed: {missing}")
                print(f"PASS no Desktop, no execution (daemon PID {daemon.pid}).", flush=True)
                if args.no_desktop:
                    return 0

                binary = root / "jcode-ax-fixture"
                swift = Path(__file__).resolve().parent / "fixtures/macos-computer-acceptance.swift"
                subprocess.run(["/usr/bin/swiftc", swift, "-o", binary], check=True, timeout=90)
                if subprocess.run([binary, "--check-aqua"], timeout=5).returncode == BLOCKED:
                    print("BLOCKED: unlock the macOS Aqua session before showing the consent sheets.", flush=True)
                    return BLOCKED
                denied = stack.enter_context(session(socket_path, provider, root, daemon))
                denied.tool("discover")  # Persist a second real session for the Desktop catalog.
                require(allowed.session_id != denied.session_id, "sessions must be distinct")
                bridge = spawn([Path(args.bridge).resolve(strict=True)], stdout=log)
                until(lambda: api_socket.is_socket(), "private harness API", bridge)
                desktop_path = Path(args.desktop).resolve(strict=True)
                desktop = spawn([desktop_path, "--single-panel", "--no-hot-reload",
                                 f"--session={allowed.session_id}"], stdout=log)
                print(f"Isolated Desktop: {desktop_path}; PID {desktop.pid} != daemon {daemon.pid}", flush=True)
                require(desktop.pid != daemon.pid, "Desktop must have its own process")
                endpoint, target = until(lambda: desktop_target(control, allowed.session_id, desktop.pid),
                                         "Desktop window attached to allowed session", desktop)
                allowed.executor_pid = denied.executor_pid = desktop.pid

                fixture = spawn([binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
                window, decoy = map(int, fixture_line(fixture).split("|"))
                require(window > 0 and decoy > 0 and window != decoy, "invalid fixture WindowServer IDs")
                scope = {"app": f"pid:{fixture.pid}", "window_id": window}

                snapshot = decide(allowed, fixture, binary, desktop, (endpoint, target),
                                  "Allow for this session", "observe", view="ax", **scope)
                allowed.check_executor(snapshot, "approved observe")
                ax, screen = status(allowed)
                live_exit = live(allowed, fixture, binary, root, scope, snapshot, ax, screen)
                before_denial = state(fixture)[:2]
                print("Opening the second test session. Choose 'Deny' on its separate consent sheet.", flush=True)
                until(lambda: desktop_request(endpoint, action="open_session", session_id=denied.session_id,
                                              destination="new_window").get("status") == "ok",
                      "new session in the private Desktop catalog", desktop)
                denied_endpoint, denied_target = until(
                    lambda: desktop_target(control, denied.session_id, desktop.pid),
                    "second session attached to the same Desktop", desktop)
                require(target != denied_target, "both session targets were identical")
                receipt = decide(denied, fixture, binary, desktop, (denied_endpoint, denied_target),
                                 "Deny", "act", verb="set_value",
                                 snapshot_id=snapshot.get("snapshot_id", "none"),
                                 element_ref="none", value="DO NOT WRITE")
                require(receipt.get("reason") == "consent_denied" and "executor_pid" not in receipt,
                        f"Desktop denied but executed the operation: {receipt}")
                require(denied.receipt("observe", view="ax", **scope).get("reason") == "consent_denied",
                        "the second session did not remember its denial")
                direct = desktop_request(denied_endpoint, action="computer_use", target=denied_target,
                                         input={"action": "observe", "view": "ax", **scope})
                require(direct.get("code") == "consent_denied", f"direct socket bypassed the denial: {direct}")
                require(state(fixture)[:2] == before_denial, "denial changed the fixture")
                print("PASS second session asked independently, denied before dispatch, no second prompt.", flush=True)
                return live_exit
        finally:
            provider.shutdown()
            thread.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jcode", required=True, help="pinned Jcode executable")
    parser.add_argument("--desktop", help="pinned Jcode Desktop executable (not an app bundle)")
    parser.add_argument("--bridge", help="pinned jcode-harness-api-bridge executable")
    parser.add_argument("--no-desktop", action="store_true", help="only assert the missing broker fails closed")
    args = parser.parse_args()
    if not args.no_desktop and not (args.desktop and args.bridge):
        parser.error("--desktop and --bridge are required for the interactive test")
    if sys.platform != "darwin":
        print("BLOCKED: macOS is required.", file=sys.stderr)
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
