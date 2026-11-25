#!/usr/bin/env python3
"""Build and smoke a Jcode + Desktop commit pair without touching the installation."""

import argparse
import base64
from contextlib import ExitStack, contextmanager
from datetime import datetime, timezone
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from http.server import ThreadingHTTPServer


RECEIPT_SCHEMA = 1
CORE_FLOWS = (
    "send", "queue_while_busy", "btw", "ask_answer", "image_paste",
    "transcript_scroll", "restart_restore",
)


class Blocked(RuntimeError):
    """A prerequisite was unavailable, so the flow did not run."""


def output(*command, env=None):
    return subprocess.check_output(command, env=env, text=True, timeout=30).strip()


def git(repo, *args):
    return output("git", "-C", repo, *args)


@contextmanager
def checkout(repo, path, sha):
    if git(repo, "rev-parse", "--verify", f"{sha}^{{commit}}") != sha:
        raise Blocked(f"{repo}: {sha} does not resolve to the exact commit")
    git(repo, "worktree", "add", "--detach", "--quiet", path, sha)
    try:
        yield path
    finally:
        # Cargo can regenerate the Desktop lockfile from the sibling CLI SDK.
        git(repo, "worktree", "remove", "--force", path)


def build(repo, target, package, profile, env, log):
    with log.open("w") as stream:
        subprocess.run(
            ["justrust", "build", "-p", package, "--bin", package, "--profile", profile,
             "--target-dir", str(target)], cwd=repo, env=env, stdout=stream,
            stderr=subprocess.STDOUT, check=True, timeout=3600,
        )
    return target / ("debug" if profile == "dev" else profile) / package


def load_script(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def stop(_signal, _frame):
    raise KeyboardInterrupt("paired acceptance interrupted")


@contextmanager
def instance(verify, cls, evidence, fixture, cli, desktop, **fields):
    class PrivateInstance(cls):
        def native(self, action):
            deadline = time.monotonic() + 1
            while True:
                try:
                    return super().native(action)
                except RuntimeError as error:
                    # bind() publishes the socket just before Desktop applies mode 0600.
                    if str(error) != "refusing unsafe Desktop socket" or time.monotonic() >= deadline:
                        raise
                    time.sleep(.02)

    evidence.mkdir(mode=0o700)
    with tempfile.TemporaryDirectory(prefix="j-smoke-", dir="/tmp") as directory:
        scratch = Path(directory).resolve()
        token = secrets.token_hex(16)
        marker = scratch / ".owner"
        marker.write_text(token)
        marker.chmod(0o600)
        (scratch / "control").mkdir(mode=0o700)
        manifest = dict(schema=verify.SCHEMA, evidence=str(evidence), phase="starting",
                        scratch=str(scratch), token=token, fixture=fixture, binary=str(desktop),
                        jcode_bin=str(cli), **fields)
        app = PrivateInstance(evidence, manifest)
        try:
            try:
                app.start()
            except (OSError, RuntimeError, TimeoutError, subprocess.SubprocessError) as error:
                raise Blocked(f"could not start private Desktop fixture: {error}") from error
            yield app
        finally:
            app.cleanup("paired acceptance smoke finished")


def panel(app):
    return app.drive("state")["panel"]


def key(app, name):
    try:
        app.drive("press_key", {"key": f"fixture:{name}"})
    except RuntimeError as error:
        if '"code": "unsupported_key"' in str(error):
            raise Blocked(f"this Desktop build lacks the private {name} fixture key") from error
        raise


def submit(app, text, chord="enter"):
    app.drive("set_composer", {"text": text})
    key(app, chord)


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def visible_reply(app, marker):
    def final_reply():
        return any(text.get("phase") == "Final" and text.get("text") == marker
                   for row in panel(app)["loaded_transcript_items"] if isinstance(row, dict)
                   for text in [row.get("AssistantOutput", {})])

    app.runtime_wait(final_reply, f"settled assistant reply {marker!r}")
    frame = app.screenshot("reply.png")
    if marker in json.dumps(panel(app)["visible_transcript_items"]):
        return
    # Clean Chat can paint a different projection than the native control rows.
    if not shutil.which("tesseract"):
        raise Blocked("tesseract is required when hidden transcript rows are not exposed")
    painted = output("tesseract", frame["path"], "stdout", "--psm", "11")
    (app.run / "reply-ocr.txt").write_text(painted)
    normalize = lambda text: re.sub(r"\W|_", "", text).lower()
    # The fake provider uses a different spelling from the submitted prompt.
    check(any(normalize(line) == normalize(marker) for line in painted.splitlines()),
          f"Desktop did not paint the assistant reply {marker!r}")


def send(app):
    submit(app, "[e2e:stream-next:smoke_send]")
    visible_reply(app, "STAGEC_smoke_send")


def queue_while_busy(app):
    active = "[e2e:stream:smoke_busy]"
    queued = "[e2e:stream-next:smoke_queue]"
    submit(app, active)
    app.runtime_wait((app.profile / "smoke_busy.held").is_file, "held fake-provider stream")
    session = app.target["session_id"]

    def pending():
        _, response = app.harness_requests([
            ("attach_session", {"session_id": session}),
            ("get_queued_prompts", {"session_id": session}),
        ])
        return response["prompts"]

    check(not pending(), "fresh session already has queued prompts")
    try:
        submit(app, queued, "ctrl-enter")
        app.runtime_wait(lambda: any(p["content"] == queued and p["mode"] == "after_yield"
                                    for p in pending()), "an after-yield prompt while still busy")
    finally:
        (app.profile / "smoke_busy.release").touch()
    app.runtime_wait(lambda: not pending(), "drained queue")
    visible_reply(app, "STAGEC_smoke_queue")


def ask_answer(app):
    submit(app, "Choose one for the acceptance smoke")
    card = app.runtime_wait(lambda: panel(app)["ask"], "native ask card")
    app.drive("click", {"selector": card["option_selectors"][1]})
    key(app, "enter")
    visible_reply(app, "continued")
    rows = panel(app)["loaded_transcript_items"]
    check(panel(app)["ask"] is None and any(
        row.get("Tool", {}).get("done") and "-> Beta" in row["Tool"]["output"]
        for row in rows if isinstance(row, dict)),
          "selected answer did not reach the real ask tool")


def image_paste(app):
    prompt = "Smoke test pasted image"
    app.drive("set_composer", {"text": prompt})
    key(app, "super-v")
    key(app, "enter")

    def request_image():
        path = app.run / "fake-requests.jsonl"
        if not path.exists():
            return None
        for request in map(json.loads, path.read_text().splitlines()):
            for msg in request["messages"]:
                if msg["role"] != "user" or not isinstance(msg["content"], list):
                    continue
                parts = msg["content"]
                if not any(prompt in part.get("text", "") for part in parts):
                    continue
                for part in parts:
                    url = part.get("image_url", {}).get("url", "")
                    if url.startswith("data:image/png;base64,"):
                        return base64.b64decode(url.split(",", 1)[1], validate=True)
        return None

    png = app.runtime_wait(request_image, "pasted image in the actual provider request")
    check(png.startswith(b"\x89PNG\r\n\x1a\n"), "submitted attachment was not a PNG")


def restart_restore(app):
    session = app.target["session_id"]
    draft = "Unsent smoke draft before restart"
    submit(app, "[e2e:stream-next:smoke_restore]")
    visible_reply(app, "STAGEC_smoke_restore")
    app.drive("set_composer", {"text": draft})

    def snapshot():
        for path in (app.profile / "restart/windows").glob(f"{app.process.pid}-*.json"):
            value = json.loads(path.read_text())["snapshot"]
            encoded = json.dumps(value)
            if session in encoded and draft in encoded:
                return encoded
        return None

    restore = app.profile / "restore.json"
    restore.write_text(app.runtime_wait(snapshot, "saved workspace with the unsent draft"))
    app.runtime_stop(app.process)
    app.runtime_stop(app.api_bridge)
    app.runtime_stop(app.daemon)
    app.api_socket.unlink(missing_ok=True)
    app.legacy_socket.unlink(missing_ok=True)
    app.start_background_daemon()
    env = dict(app.desktop_env)
    del env["JCODE_DESKTOP_CONTROL_RUNTIME_SESSION"]
    app.start_desktop(env, [f"--restore-state={restore}"], suffix="-restarted")
    check(app.target["session_id"] == session and not restore.exists(), "Desktop did not consume its restart snapshot")
    app.runtime_wait(lambda: panel(app)["composer"] == draft, "restored unsent draft")
    app.runtime_wait(lambda: "STAGEC_smoke_restore" in json.dumps(panel(app)["loaded_transcript_items"]),
                     "restored transcript from the restarted daemon")
    app.screenshot("restored.png")


def exercise(verify, btw, ask, run, cli, desktop, results):
    def side_question(app):
        verify.save(app.run / "btw-proof.json", app.exercise())

    class Live(verify.Instance):
        def start_background_runtime(self, env):
            self.desktop_env = env
            if self.manifest.get("clean_chat"):
                with (self.profile / "desktop.toml").open("a") as config:
                    config.write("\n[display]\nclean_chat_default = true\n")
            if not self.manifest.get("ask_provider"):
                return super().start_background_runtime(env)
            self.legacy_socket = self.profile / "jcode/legacy.sock"
            self.api_socket = self.profile / "jcode/api.sock"
            self.runtime_cli = [self.manifest["jcode_bin"], "--no-update", "--no-selfdev"]
            self.runtime_env = dict(env, JCODE_PROVIDER="openai-compatible", JCODE_MODEL="fake-model",
                                    OPENAI_COMPAT_API_KEY="fixture-only", JCODE_OPENAI_COMPAT_DEFAULT_MODEL="fake-model",
                                    JCODE_OPENAI_COMPAT_API_BASE=self.manifest["ask_provider"],
                                    JCODE_TEST_SESSION="1", JCODE_SOCKET=str(self.legacy_socket),
                                    JCODE_API_SOCKET=str(self.api_socket))
            self.start_background_daemon()
            session = json.loads(self.runtime_debug(f"create_session:{self.profile / 'home'}"))["session_id"]
            env.update(JCODE_TEST_SESSION="1", JCODE_DESKTOP_CONTROL_RUNTIME_SOCKET=str(self.api_socket),
                       JCODE_DESKTOP_CONTROL_RUNTIME_SESSION=session)

        def start_background_daemon(self):
            self.daemon = self.runtime_spawn("private-daemon", [*self.runtime_cli, "serve", "--socket", str(self.legacy_socket)])
            self.runtime_wait(self.legacy_socket.exists, "private daemon socket")
            self.api_bridge = self.runtime_spawn("private-bridge", [*self.runtime_cli, "api-bridge", "--api-socket", str(self.api_socket)])
            self.runtime_wait(self.api_socket.exists, "private bridge socket")

        def start_desktop(self, env, modes, suffix=""):
            super().start_desktop(dict(env, JCODE_TEST_SESSION="1"), modes, suffix)

    steps = {
        "send": send,
        "queue_while_busy": queue_while_busy,
        "btw": side_question,
        "ask_answer": ask_answer,
        "image_paste": image_paste,
        "transcript_scroll": lambda app: check(app.transcript_scroll_journey().get("passed") is True,
                                               "scroll proof did not pass"),
        "restart_restore": restart_restore,
    }
    for flow, step in steps.items():
        evidence = run / flow
        cls, fixture, fields = Live, "swarm-dm", {}
        try:
            with ExitStack() as stack:
                if flow == "btw":
                    cls = btw.Fixture
                elif flow == "transcript_scroll":
                    fixture = "transcript-scroll"
                elif flow == "send":
                    fields["clean_chat"] = True
                if flow == "ask_answer":
                    server = stack.enter_context(ThreadingHTTPServer(("127.0.0.1", 0), ask.Provider))
                    threading.Thread(target=server.serve_forever, daemon=True).start()
                    stack.callback(server.shutdown)
                    fields["ask_provider"] = f"http://127.0.0.1:{server.server_port}/v1"
                if flow == "restart_restore":
                    fields["persist_restart_manifest"] = True
                with instance(verify, cls, evidence, fixture, cli, desktop, **fields) as app:
                    if cls is Live and fixture == "swarm-dm":
                        app.runtime_wait(lambda: panel(app)["model"] == "fake-model", "attached runtime model")
                    step(app)
        except Exception as error:
            evidence.mkdir(mode=0o700, exist_ok=True)
            (evidence / "error.log").write_text(traceback.format_exc())
            results[flow] = "BLOCKED" if isinstance(error, Blocked) else "ISSUES"
            print(f"{results[flow]} {flow}: {error}", flush=True)
        else:
            results[flow] = "PASS"
            print(f"PASS {flow}", flush=True)


def smoke(args, run, results):
    if sys.platform != "darwin":
        raise Blocked("paired smoke requires a macOS graphical login and hidden native Desktop")
    with ExitStack() as stack:
        cli_repo = stack.enter_context(checkout(args.jcode_repo, run / "jcode", args.jcode_sha))
        desktop_repo = stack.enter_context(checkout(args.desktop_repo, run / "jcode-desktop", args.desktop_sha))
        scripts = desktop_repo / "scripts"
        sys.path.insert(0, str(scripts))
        stack.callback(sys.path.remove, str(scripts))
        verify = load_script(scripts / "verify-jcode-desktop.py", "verify_desktop")
        verify.require_safe_host()
        ask = load_script(cli_repo / "scripts/accept-ask-reload.py", "ask_fixture")
        btw = load_script(scripts / "accept-btw-model.py", "btw_fixture")
        build_env = {k: v for k, v in os.environ.items() if not k.startswith(("JCODE_", "RUSTFLAGS", "CARGO_BUILD_TARGET"))}
        build_env.pop("SDKROOT", None)
        build_env["DEVELOPER_DIR"] = "/Applications/Xcode.app/Contents/Developer"
        target = args.receipt_dir / "target"
        cli = build(cli_repo, target / "cli", "jcode", "selfdev" if args.dev_build else "release",
                    dict(build_env, JCODE_BUILD_GIT_HASH=args.jcode_sha), run / "cli-build.log")
        desktop = build(desktop_repo, target / "desktop", "jcode-desktop", "dev" if args.dev_build else "release",
                        build_env, run / "desktop-build.log")
        with tempfile.TemporaryDirectory(prefix="j-smoke-version-", dir="/tmp") as directory:
            env = verify.environment(Path(directory), Path(directory) / "control")
            env["JCODE_TEST_SESSION"] = "1"
            cli_version = json.loads(output(cli, "version", "--json", env=env))
            desktop_version = output(desktop, "--version", env=env)
        match = re.fullmatch(r"Jcode Desktop \S+ \(([0-9a-f]{7,64})(?:, dirty)?\)", desktop_version)
        if (cli_version.get("git_hash") != args.jcode_sha or not match
                or git(desktop_repo, "rev-parse", "--verify", f"{match[1]}^{{commit}}") != args.desktop_sha
                or git(cli_repo, "status", "--porcelain")
                or git(desktop_repo, "status", "--porcelain") not in ("", "M Cargo.lock")):
            raise Blocked(f"built artifacts do not identify the selected pair: {cli_version}, {desktop_version}")
        (run / "versions.json").write_text(json.dumps(dict(cli=cli_version, desktop=desktop_version), indent=2))
        exercise(verify, btw, ask, run, cli, desktop, results)


def has_passing_receipt(path, pair):
    def unique_object(items):
        value = dict(items)
        if len(value) != len(items):
            raise ValueError("duplicate JSON key")
        return value

    def reject_constant(value):
        raise ValueError(f"invalid JSON constant: {value}")

    # Keep the acceptance checks in sync with src/cli/rollout_acceptance.rs.
    try:
        receipt = json.loads(path.read_text(encoding="utf-8"),
                             object_pairs_hook=unique_object, parse_constant=reject_constant)
        if (not isinstance(receipt, dict)
                or type(receipt.get("schema")) is not int
                or receipt["schema"] != RECEIPT_SCHEMA
                or receipt.get("result") != "PASS"
                or receipt.get("flows") != dict.fromkeys(CORE_FLOWS, "PASS")):
            return False
        saved_pair = receipt.get("pair")
        if not isinstance(saved_pair, dict) or any(saved_pair.get(key) != sha for key, sha in pair.items()):
            return False
        timestamp = receipt.get("finished_at")
        if not isinstance(timestamp, str):
            return False
        match = re.fullmatch(
            r"([0-9]{4}-[0-9]{2}-[0-9]{2}[Tt ][0-9]{2}:[0-9]{2}:[0-9]{2})"
            r"(?:\.[0-9]+)?(?:[Zz]|[+-](?:[01][0-9]|2[0-3]):[0-5][0-9])", timestamp
        )
        if not match:
            return False
        datetime.fromisoformat(match[1])
    except (OSError, ValueError, RecursionError):
        return False
    return True


def main():
    import fcntl
    sys.dont_write_bytecode = True

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jcode-repo", type=Path)
    parser.add_argument("--desktop-repo", type=Path)
    parser.add_argument("--jcode-sha", required=True)
    parser.add_argument("--desktop-sha", required=True)
    parser.add_argument("--receipt-dir", type=Path,
                        default=Path(os.environ.get("JCODE_HOME", Path.home() / ".jcode")) / "rollout-acceptance")
    parser.add_argument("--dev-build", action="store_true", help="use the repo's local development profiles")
    parser.add_argument("--force", action="store_true", help="rerun even if this pair already passed")
    args = parser.parse_args()
    for sha in (args.jcode_sha, args.desktop_sha):
        if not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", sha):
            parser.error("both target SHAs must be full lowercase Git object IDs")
    if args.jcode_repo is None and __file__ == "<stdin>":
        parser.error("--jcode-repo is required when reading the runner from stdin")
    args.jcode_repo = (args.jcode_repo or Path(__file__).resolve().parents[1]).resolve()
    args.desktop_repo = (args.desktop_repo or args.jcode_repo.with_name("jcode-desktop")).resolve()
    args.receipt_dir = args.receipt_dir.expanduser().resolve()
    args.receipt_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    pair = dict(cli=args.jcode_sha, desktop=args.desktop_sha)
    path = args.receipt_dir / f"{pair['cli']}-{pair['desktop']}.json"
    with (args.receipt_dir / ".build.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if not args.force and has_passing_receipt(path, pair):
            print(f"PASS receipt: {path}")
            return 0
        run = Path(tempfile.mkdtemp(prefix="smoke-", dir=args.receipt_dir))
        results = dict.fromkeys(CORE_FLOWS, "BLOCKED")
        if args.force:
            # Do not let an old PASS authorize rollout while the forced run is in flight.
            path.unlink(missing_ok=True)
        blocked = False
        try:
            smoke(args, run, results)
        except (Exception, KeyboardInterrupt) as error:
            blocked = True
            (run / "error.log").write_text(traceback.format_exc())
            print(f"BLOCKED: {error}", flush=True)
            for flow, status in results.items():
                if status == "BLOCKED":
                    print(f"BLOCKED {flow}: run did not reach this flow", flush=True)
        result = "ISSUES" if "ISSUES" in results.values() else "BLOCKED" if blocked or "BLOCKED" in results.values() else "PASS"
        receipt = dict(schema=RECEIPT_SCHEMA, pair=pair, flows=results,
                       result=result, finished_at=datetime.now(timezone.utc).isoformat(), evidence=str(run))
        with tempfile.NamedTemporaryFile(mode="w", dir=args.receipt_dir, delete=False) as stream:
            json.dump(receipt, stream, indent=2)
            stream.write("\n")
        os.replace(stream.name, path)
    print(f"{result} receipt: {path}")
    return {"PASS": 0, "ISSUES": 1, "BLOCKED": 2}[result]


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, stop)
    raise SystemExit(main())
