#!/usr/bin/env python3
"""Probe pinned modes on explicitly selected binaries with a private fake provider."""

import argparse
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
from types import SimpleNamespace


REPO = Path(__file__).resolve().parents[1]
NAME = "poteto-mode"
BODY = "POTETO_PROBE_PROJECT_BODY"
GLOBAL = "POTETO_PROBE_GLOBAL_BODY"
EPHEMERAL = "POTETO_PROBE_UNAVAILABLE_BODY"
PROJECT_RULE = "POTETO_PROBE_PROJECT_AGENTS"
GLOBAL_RULE = "POTETO_PROBE_HOME_AGENTS"


def fixture(path, name, marker, source=None):
    file = path / "skills" / name / "SKILL.md"
    file.parent.mkdir(parents=True, exist_ok=True)
    frontmatter = (f"---\nname: {name}\ndescription: Probe fixture\n"
                   "disable-model-invocation: true\n---\n")
    file.write_text((source or frontmatter) + f"\n{marker}\n")
    return file


def load_harness():
    sys.dont_write_bytecode = True
    path = REPO / ".jcode/skills/verify-jcode/scripts/verify-jcode"
    loader = importlib.machinery.SourceFileLoader("verify_jcode", str(path))
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
    loader.exec_module(module)
    return module


def record(harness, send, text):
    start = len(harness.records())
    send(text)
    return json.dumps(harness.request_after(text.split()[-1], start)["messages"], ensure_ascii=False)


def worker(harness, wait_for, send, *, missing_source=False):
    start = len(harness.records())
    prompt, marker = ("[e2e:worker-spawn:poteto_unavailable]", "poteto_unavailable Acknowledge.") if missing_source else ("[e2e:spawn]", "[e2e:child]")
    send(prompt)
    return wait_for(lambda: next((json.dumps(r["messages"], ensure_ascii=False)
                                 for r in harness.records()[start:] if r["agent"]
                                 and marker in json.dumps(r["messages"][-1:])), None),
                    "headless worker's first provider request")


def require(condition, description):
    if not condition:
        raise RuntimeError(description)


def prepare(root, source):
    workspace = root / "workspace"
    (workspace / "AGENTS.md").write_text(PROJECT_RULE)
    external = root / "data/external"
    external.mkdir()
    (external / "AGENTS.md").write_text(GLOBAL_RULE)
    fixture(root / "data", NAME, GLOBAL, source)
    fixture(workspace / ".claude", NAME, "POTETO_PROBE_PROJECT_CLAUDE")
    fixture(workspace / ".agents", NAME, "POTETO_PROBE_PROJECT_AGENTS_SKILL")
    fixture(workspace / ".jcode", NAME, BODY)
    fixture(workspace / ".jcode", "ephemeral-mode", EPHEMERAL)


def cli(harness, verify):
    workspace = harness.root / "workspace"

    def turn(text):
        result = record(harness, harness.send, text)
        harness.idle()
        return result

    cold = turn("probe_cli_cold")
    require(NAME in cold and BODY not in cold and GLOBAL not in cold, "catalog was absent or mode auto-activated")
    require(PROJECT_RULE in cold, "missing project AGENTS")
    require(GLOBAL_RULE in cold, "missing global AGENTS")

    bare_start = len(harness.records())
    harness.send("/poteto-mode")
    verify.wait_for(lambda: "Activated skill: poteto-mode" in harness.pane(), "bare slash activation")
    require(len(harness.records()) == bare_start, "bare slash unexpectedly called the provider")
    active = turn("probe_cli_active")
    require(BODY in active and GLOBAL not in active, "project .jcode overlay did not win")
    seen = worker(harness, verify.wait_for, harness.send)
    harness.idle()
    require(all(marker in seen for marker in (BODY, PROJECT_RULE, GLOBAL_RULE)),
            "headless worker lost active body or its own bootstrap AGENTS")

    (workspace / ".jcode/skills/poteto-mode/SKILL.md").unlink()
    harness.send("/mode off")
    verify.wait_for(lambda: "Session mode: off" in harness.pane(), "mode off")
    harness.send("/mode poteto-mode")
    verify.wait_for(lambda: "Session mode: poteto-mode" in harness.pane(), "mode on")
    overlay = turn("probe_cli_overlay")
    require("Active mode: poteto-mode" in overlay and "POTETO_PROBE_PROJECT_AGENTS_SKILL" in overlay,
            "overlay did not fall through to project .agents")
    for scope in (".claude", ".agents"):
        (workspace / scope / "skills" / NAME / "SKILL.md").unlink()
    harness.send("/mode off")
    verify.wait_for(lambda: "Session mode: off" in harness.pane(), "second mode off")
    harness.send("/mode poteto-mode")
    require(GLOBAL in turn("probe_cli_global"), "global skill did not win after removal of project overlays")
    harness.send("/mode ephemeral-mode")
    verify.wait_for(lambda: "Session mode: ephemeral-mode" in harness.pane(), "ephemeral mode activation")
    require(EPHEMERAL in turn("probe_cli_before_source_removal"), "coordinator never received ephemeral mode")
    (workspace / ".jcode/skills/ephemeral-mode/SKILL.md").unlink()
    print(json.dumps({"client": "tui", "fresh": "catalog only", "manual": "body",
                      "headless": "pinned body and project/home AGENTS",
                      "next": "pinned fallback with unavailable skill source"}), flush=True)
    seen = worker(harness, verify.wait_for, harness.send, missing_source=True)
    harness.idle()
    require(EPHEMERAL in seen, "headless worker lost the stored pinned body after source removal")
    print(json.dumps({"client": "tui", "unavailable_source_fallback": "passed"}), flush=True)


def rpc(path, request):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(15)
        connection.connect(str(path))
        connection.sendall(json.dumps(request).encode() + b"\n")
        with connection.makefile("rb") as reader:
            return json.loads(reader.readline())


def desktop(harness, verify, binary):
    root = harness.root
    api = root / "data/api.sock"
    control = root / "control/desktop"
    control.mkdir()
    harness.spawn("api-bridge", [harness.binary, "--no-update", "--no-selfdev", "--socket",
                                 root / "app.sock", "api-bridge", "--api-socket", api])
    verify.wait_for(api.is_socket, "private harness API")
    create = rpc(root / "app-debug.sock", {"type": "debug_command", "id": 1,
                                           "command": f"create_session:{root / 'workspace'}"})
    require(create.get("ok"), "private Desktop session creation failed")
    sid = json.loads(create["output"])["session_id"]
    env = dict(harness.env, JCODE_API_SOCKET=str(api), JCODE_SOCKET=str(root / "app.sock"),
               JCODE_DESKTOP_SCREENSHOT="1", JCODE_DESKTOP_SCREENSHOT_TRANSCRIPT="background-jobs",
               JCODE_DESKTOP_CONTROL_TEST="1", JCODE_DESKTOP_CONTROL_DIR=str(control),
               JCODE_DESKTOP_CONTROL_RUNTIME_SOCKET=str(api), JCODE_DESKTOP_CONTROL_RUNTIME_SESSION=sid,
               JCODE_DESKTOP_MUTE_SOUNDS="1", JCODE_DESKTOP_CONFIG=str(root / "desktop.toml"),
               JCODE_DESKTOP_WORKING_DIR=str(root / "workspace"))
    process = harness.spawn("desktop", [binary, "--no-hot-reload"], env)
    endpoint = verify.wait_for(lambda: next(iter(control.glob(f"{process.pid}-*.sock")), None),
                               "owned hidden Desktop socket")

    def native(action, **fields):
        reply = rpc(endpoint, {"v": 1, "action": action, **fields})
        require(reply.get("status") == "ok", f"Desktop control {action} failed: {reply}")
        return reply["result"]

    window = native("list")["window_id"]
    require(window.startswith(f"{process.pid}:"), "unexpected Desktop window owner")
    target = {"window_id": window, "session_id": sid}
    verify.wait_for(lambda: any(p["session"] == sid for row in native("list")["navigation"]["rows"]
                                for p in row["panels"]), "Desktop attached to the private session")

    def send(text):
        native("set_composer", target=target, text=text, allow_user_window=True)
        native("send", target=target, allow_user_window=True)

    def turn(text):
        result = record(harness, send, text)
        harness.persisted(sid, text.split()[-1])
        return result

    cold = turn("probe_desktop_cold")
    require(NAME in cold and BODY not in cold and GLOBAL not in cold, "Desktop catalog missing or mode auto-activated")
    active = turn("/poteto-mode probe_desktop_active")
    require(BODY in active and GLOBAL not in active, "Desktop did not deliver slash skill from the project overlay")
    seen = worker(harness, verify.wait_for, send)
    require(all(marker in seen for marker in (BODY, PROJECT_RULE, GLOBAL_RULE)),
            "Desktop headless worker lost active body or its own bootstrap AGENTS")
    print(json.dumps({"client": "desktop", "fresh": "catalog only", "manual": "body",
                      "headless": "pinned body and project/home AGENTS"}), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jcode", required=True, type=Path)
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--skill-file", type=Path, help="Optional local SKILL.md to copy to the isolated registry")
    parser.add_argument("--desktop", type=Path,
                        help="Run Desktop instead of TUI; supply a reviewed macOS binary with hidden control fixture")
    args = parser.parse_args()
    os.umask(0o077)
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    source = args.skill_file.read_text() if args.skill_file else None
    verify = load_harness()
    if args.desktop:
        require(sys.platform == "darwin", "Desktop probe needs the hidden macOS control fixture")
        args.desktop = args.desktop.resolve(strict=True)
        require(os.access(args.desktop, os.X_OK), "Desktop binary is not executable")
    verify.launch(SimpleNamespace(binary=args.jcode, evidence=args.evidence, cols=160,
                                  rows=48, loopback_exec_worker=False),
                  prepare_workspace=lambda root: prepare(root, source))
    harness = verify.Harness(args.evidence)
    try:
        if args.desktop:
            desktop(harness, verify, args.desktop)
        else:
            cli(harness, verify)
    finally:
        harness.cleanup()


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, subprocess.SubprocessError) as error:
        sys.exit(f"poteto probe: {error}")
