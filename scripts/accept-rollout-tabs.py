#!/usr/bin/env python3
"""Check daemon rollout with real macOS Desktop windows and a private runtime.

Pass the CLI under test, the previous bundled CLI, and a built Desktop bundle.
The supplied bundle is read-only and copied to a scratch directory. The CLI
stays on the same generation so the private test mode can suppress daemon exec.
The fixture restores synthetic session IDs and never submits a prompt.
"""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import threading

spec = importlib.util.spec_from_file_location(
    "rollout", Path(__file__).with_name("accept-rollout-daemon.py")
)
rollout = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rollout)

PRIMARY = [("session_rollout_first", "first draft"), ("session_rollout_second", "second draft")]
SECONDARY = [("session_rollout_auxiliary", "auxiliary draft")]


class Provider(BaseHTTPRequestHandler):
    turns = 0

    def do_POST(self):
        type(self).turns += 1
        self.send_error(500, "unexpected provider turn")

    def log_message(self, *_):
        pass


def dump(path, value):
    path.write_text(json.dumps(value))
    path.chmod(0o600)


def snapshot(tabs):
    return {
        "format_version": 1, "active": len(tabs) - 1, "active_row": 0,
        "row_focus": [len(tabs) - 1, None, None, None], "previous": None,
        "camera_x": [0] * 4, "camera_target": [0] * 4, "overview": False,
        "hints_overlay": False, "folder_picker_dir": None, "folder_picker_error": None,
        "folder_search": None, "focus": "Workspace", "launch": {"args": [], "env": []},
        "slots": [
            {"row": 0, "width_fraction": 0.5, "restore_fraction": None,
             "panel": {"session_id": session, "title": session, "working_dir": "/",
                       "draft": {"content": draft, "selection_start": 0, "selection_end": 0,
                                 "selection_reversed": False, "history": [], "history_index": None,
                                 "live_draft": "", "attachments": []},
                       "scroll_x": 0, "scroll_y": 0, "stick_to_bottom": True}}
            for session, draft in tabs
        ],
    }


def windows(directory, old_pids=()):
    result = {}
    processes = subprocess.check_output(["/bin/ps", "-axo", "pid=,stat="], text=True)
    alive = {int(pid) for pid, status in map(str.split, processes.splitlines()) if "Z" not in status}
    for file in directory.glob("*.json"):
        manifest = json.loads(file.read_text())
        pid = manifest["pid"]
        if pid in old_pids or pid not in alive:
            continue
        saved = manifest["snapshot"]
        tabs = [(slot["panel"]["session_id"], slot["panel"]["draft"]["content"])
                for slot in saved["slots"]]
        result[file] = (pid, tabs, saved["active"])
    return result


def assert_windows(directory, old_pids=()):
    observed = windows(directory, old_pids)
    if sorted((tabs, active) for _, tabs, active in observed.values()) == sorted(
        [(PRIMARY, 1), (SECONDARY, 0)]
    ):
        return observed
    return None


def copy_bundle(source, target, cli):
    shutil.rmtree(target)
    subprocess.run(["/usr/bin/ditto", source, target], check=True)
    binary = target / "Contents/MacOS/jcode"
    binary.unlink()
    shutil.copy2(cli, binary)


def desktop_hash(bundle):
    version = subprocess.check_output([bundle / "Contents/MacOS/jcode-desktop", "--version"], text=True)
    return version.strip().rsplit("(", 1)[1].removesuffix(")")


def scenario(root, args, provider_url, case):
    host = rollout.Host(root, args.new, args.old)
    manifests = root / "restart/windows"
    for key in list(host.env):
        if key.startswith(("OPENAI_", "ANTHROPIC_", "OPENROUTER_", "XDG_", "JCODE_DESKTOP_")):
            host.env.pop(key)
    host.env.update({
        "JCODE_TEST_SESSION": "1",
        "XDG_STATE_HOME": str(root / "state"),
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_DATA_HOME": str(root / "data"),
        "XDG_CACHE_HOME": str(root / "cache"),
        "XDG_RUNTIME_DIR": str(host.runtime),
        "JCODE_DESKTOP_RESTART_DIR": str(manifests.parent),
        "ANTHROPIC_API_KEY": "private-fixture-not-a-real-key",
        "ANTHROPIC_BASE_URL": provider_url,
        "OPENAI_API_KEY": "private-fixture-not-a-real-key",
        "OPENAI_BASE_URL": provider_url + "/v1",
    })
    copy_bundle(args.bundle, host.app, args.old)
    copy_bundle(args.bundle, host.package, args.new)
    desktop = host.app / "Contents/MacOS/jcode-desktop"
    if case == "rollback":
        plist = host.package / "Contents/Info.plist"
        plist.write_bytes(plist.read_bytes().replace(b"SUPublicEDKey", b"XUPublicEDKey"))
    tracked = []
    try:
        host.start()
        auxiliary_runtime = root / "other-instance"
        auxiliary_runtime.mkdir(mode=0o700)
        # Both use the default workspace mode and one Jcode runtime, but own
        # different Desktop instance sockets, as Launch Services and a shell can.
        for index, (tabs, runtime) in enumerate(((PRIMARY, host.runtime), (SECONDARY, auxiliary_runtime))):
            file = root / f"seed-{index}.json"
            dump(file, snapshot(tabs))
            log = root / f"desktop-seed-{index}.log"
            environment = dict(host.env, XDG_RUNTIME_DIR=str(runtime))
            with log.open("wb") as output:
                process = subprocess.Popen([desktop, f"--restore-state={file}"], env=environment,
                                           stdout=output, stderr=output, start_new_session=True)
            tracked.append(process)
            rollout.wait_for(f"seed {index} consumed", lambda: not file.exists())
        original = rollout.wait_for("two real windows and ordered drafts", lambda: assert_windows(manifests), 35)
        for path in original:
            manifest = json.loads(path.read_text())
            manifest["updated_at_ms"] -= 120_000
            dump(path, manifest)
        old_pids = {pid for pid, _, _ in original.values()}
        if case == "missing":
            next(iter(original)).unlink()
        intent = host.intent(binary=args.new, label=host.old_label)
        intent["desktop"] = desktop_hash(args.bundle)
        host.submit(intent)
        expected = "completed" if case == "success" else "rolled_back"

        def terminal():
            state = host.state()
            if state["phase"] in ("completed", "rolled_back", "failed"):
                assert state["phase"] == expected, state
                return state
            return None

        state = rollout.wait_for(f"{case} {expected}", terminal, 120)
        if case == "missing":
            assert "no valid restart manifest" in state["error"], state
            assert all(child.poll() is None for child in tracked)
            assert not any(m["name"] == "quit_desktop" for m in state["mutations"]), state
        else:
            observed = rollout.wait_for("replayed windows and drafts", lambda: assert_windows(manifests, old_pids), 45)
            assert all(child.poll() is not None for child in tracked)
            assert len({pid for pid, _, _ in observed.values()}) == 2
            if case == "rollback":
                assert "Sparkle public key" in state["error"], state
                assert any(m["name"] == "quit_failed_desktop" for m in state["mutations"]), state
        print(f"{case}: {expected}; private Desktop windows and drafts accounted for", flush=True)
    finally:
        current = subprocess.check_output(["/bin/ps", "-axo", "pid=,comm="], text=True)
        for line in current.splitlines():
            pid, executable = line.strip().split(maxsplit=1)
            if executable in (str(desktop), str(host.app / "Contents/MacOS/jcode")):
                try:
                    os.kill(int(pid), signal.SIGKILL)
                except ProcessLookupError:
                    pass
        for child in tracked:
            child.wait(timeout=10)
        host.stop()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("old", "new", "bundle"):
        parser.add_argument(f"--{flag}", type=lambda path: Path(path).resolve(), required=True)
    parser.add_argument("--case", choices=("success", "rollback", "missing"), action="append")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("requires macOS and a logged-in WindowServer")
    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="jrt.", dir="/private/tmp") as scratch:
            for case in args.case or ("success", "rollback", "missing"):
                scenario(Path(scratch) / case, args, f"http://127.0.0.1:{server.server_port}", case)
        assert Provider.turns == 0, f"unexpected provider turns: {Provider.turns}"
        print("provider turns: 0")
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
