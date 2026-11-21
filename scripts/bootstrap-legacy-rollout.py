#!/usr/bin/env python3
"""Explicit one-time operator migration, never an automatic rollout fallback."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time


def version(binary):
    result = subprocess.run([str(binary), "version", "--json"], check=True, capture_output=True, text=True)
    value = json.loads(result.stdout)
    label = value["git_hash"]
    if not re.fullmatch(r"[0-9a-fA-F]{7,40}", label):
        raise ValueError(f"invalid binary label: {label!r}")
    return label[:9]


def probe(path):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(5)
        connection.connect(str(path))
        if sys.platform == "darwin":
            peer = struct.unpack("i", connection.getsockopt(0, 2, 4))[0]
        else:
            peer = struct.unpack("3i", connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))[0]
        connection.sendall(b'{"type":"ping","id":1}\n')
        with connection.makefile() as reader:
            event = json.loads(reader.readline())
        if event.get("type") != "pong":
            raise ValueError(f"unexpected Ping response: {event}")
        return peer, event.get("daemon_identity")


def sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def atomic_text(path, value):
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    with open(temporary, "w", opener=lambda name, flags: os.open(name, flags, 0o600)) as target:
        target.write(value)
        target.flush()
        os.fsync(target.fileno())
    os.replace(temporary, path)
    sync_directory(path.parent)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--apply", action="store_true", help="explicitly authorize the manual migration")
    parser.add_argument("--daemon-only", action="store_true", help="defer matched Desktop rollout to a later command")
    parser.add_argument("--legacy-cli", type=Path, help="default: JCODE_HOME/builds/versions/483d7848f/jcode")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    label = version(binary)
    home = Path(os.environ.get("JCODE_HOME", str(Path.home() / ".jcode")))
    runtime = Path(os.environ.get("JCODE_RUNTIME_DIR") or os.environ.get("XDG_RUNTIME_DIR") or
                   (os.environ.get("TMPDIR") if sys.platform == "darwin" else "") or
                   str(Path(tempfile.gettempdir()) / f"jcode-{os.geteuid()}"))
    endpoint = Path(os.environ.get("JCODE_SOCKET", str(runtime / "jcode.sock")))
    builds = home / "builds"
    installed = builds / "versions" / label / "jcode"
    legacy = (args.legacy_cli or builds / "versions/483d7848f/jcode").resolve(strict=True)
    reload_command = [str(legacy), "--no-update", "server", "reload", "--force", "--json"]
    rollout_command = [str(installed), "--no-update", "rollout", "--jcode-ref", label]
    previous = {channel: {"target": str((builds / channel / "jcode").resolve(strict=True)),
                          "marker": (builds / f"{channel}-version").read_text()}
                for channel in ("current", "shared-server")}
    probe(endpoint)
    print(json.dumps({"manual_bootstrap": label, "source": str(binary), "immutable_binary": str(installed),
                      "channels": ["current", "shared-server"], "previous": previous,
                      "reload": reload_command, "desktop_rollout": rollout_command,
                      "socket": str(endpoint), "apply": args.apply}, indent=2), flush=True)
    if not args.apply:
        print("Review only. Finish active turns and stop concurrent builds, then rerun from a separate terminal with --apply.")
        return
    if os.environ.get("JCODE_SESSION_ID"):
        raise RuntimeError("Run the manual bootstrap from a separate terminal, not an agent tool. No mutation made.")
    if installed.exists() and version(installed) != label:
        raise RuntimeError("immutable version already exists with a different label")
    migration = home / "rollout"
    migration.mkdir(parents=True, exist_ok=True)
    atomic_text(migration / "manual-bootstrap-previous.json", json.dumps(previous, indent=2))
    installed.parent.mkdir(parents=True, exist_ok=True)
    if not installed.exists():
        temporary = installed.with_name(f".jcode.{os.getpid()}.incoming")
        shutil.copyfile(binary, temporary)
        temporary.chmod(0o755)
        with temporary.open("rb") as source:
            os.fsync(source.fileno())
        if version(temporary) != label:
            raise RuntimeError("copied candidate reports a different label")
        os.link(temporary, installed)
        temporary.unlink()
        sync_directory(installed.parent)
    for channel in ("shared-server", "current"):
        directory = builds / channel
        temporary = directory / f".jcode.{os.getpid()}.incoming"
        temporary.symlink_to(installed)
        os.replace(temporary, directory / "jcode")
        sync_directory(directory)
        atomic_text(builds / f"{channel}-version", label)
    environment = dict(os.environ, JCODE_SOCKET=str(endpoint))
    subprocess.run(reload_command, env=environment, check=True, timeout=120)
    deadline = time.monotonic() + 90
    while True:
        try:
            peer, identity = probe(endpoint)
            if (identity and identity.get("pid") == peer and identity.get("git_hash", "").startswith(label)
                    and Path(identity["binary_path"]).resolve() == installed.resolve()):
                break
        except (OSError, ValueError, KeyError):
            pass
        if time.monotonic() >= deadline:
            raise RuntimeError("legacy reload did not serve the selected binary; previous channels saved in manual-bootstrap-previous.json")
        time.sleep(0.05)
    print(json.dumps({"bootstrap_serving": identity, "peer_pid": peer}), flush=True)
    if args.daemon_only:
        print("Daemon bootstrapped. Desktop is unchanged. Next: " + " ".join(rollout_command))
        return
    subprocess.run(rollout_command, env=environment, check=True)
    deadline = time.monotonic() + 1800
    while time.monotonic() < deadline:
        state = json.loads((migration / "journal.json").read_text())
        if state["phase"] == "completed":
            print(json.dumps({"rollout_complete": state["id"], "backup_app": state["backup_app"],
                              "recovery": state["recovery"]}, indent=2))
            return
        if state["phase"] in ("failed", "rolled_back"):
            raise RuntimeError(f"Desktop rollout did not complete: {state}")
        time.sleep(0.25)
    raise RuntimeError("Desktop rollout still running; inspect jcode rollout --status")


if __name__ == "__main__":
    main()
