#!/usr/bin/env python3
"""Accept daemon-owned rollout against two real CLI generations, never user runtime.

Build two binaries with different JCODE_BUILD_GIT_HASH labels using justrust, then:
  scripts/accept-rollout-daemon.py --old /scratch/old-jcode --new /scratch/new-jcode
All host paths, daemon sockets, processes and Applications bundles are private.
"""

import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time


def wait_for(description, condition, timeout=90):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            value = condition()
            if value:
                return value
        except (OSError, ValueError, KeyError) as error:
            last = error
        time.sleep(0.02)
    raise AssertionError(f"timeout waiting for {description}: {last}")


def request(path, command):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(15)
        connection.connect(str(path))
        connection.sendall((json.dumps(command) + "\n").encode())
        with connection.makefile("r") as reader:
            while line := reader.readline():
                event = json.loads(line)
                if event.get("type") != "ack":
                    return event
    raise AssertionError("daemon disconnected without response")


def version(binary, root):
    env = dict(os.environ, HOME=str(root / "home"), JCODE_HOME=str(root / "jcode"),
               JCODE_RUNTIME_DIR=str(root / "run"), JCODE_ROLLOUT_APP=str(root / "Applications/Jcode.app"))
    for key in ("JCODE_RUNTIME_TARGET", "JCODE_RUNTIME_RELOAD_ID", "JCODE_RUNTIME_GENERATION"):
        env.pop(key, None)
    result = subprocess.run([str(binary), "version", "--json"], env=env, check=True, capture_output=True, text=True)
    return json.loads(result.stdout)["git_hash"][:9]


DESKTOP_C = r'''
#include <sys/wait.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "--version") == 0) {
        puts("Jcode Desktop v0.0.0 (7654321)"); return 0;
    }
    FILE *launches = fopen(getenv("ROLLOUT_DESKTOP_LAUNCHES"), "a");
    if (!launches) return 2;
    fprintf(launches, "%d\n", getpid()); fclose(launches);
    char bridge[4096]; snprintf(bridge, sizeof(bridge), "%s/Contents/MacOS/jcode", getenv("JCODE_ROLLOUT_APP"));
    pid_t child = fork();
    if (child == 0) { execl(bridge, bridge, "api-bridge", NULL); _exit(3); }
    waitpid(child, NULL, 0); return 0;
}
'''


class Host:
    def __init__(self, root, old, new, pause_backup=False):
        self.root = root
        self.old = old
        self.new = new
        self.old_label = version(old, root)
        self.new_label = version(new, root)
        assert self.old_label != self.new_label, "acceptance needs different real generation labels"
        self.home = root / "home"
        self.jcode_home = root / "jcode"
        self.runtime = root / "run"
        self.app = root / "Applications/Jcode.app"
        self.package = root / "package/Jcode.app"
        self.socket = self.runtime / "jcode.sock"
        self.debug = self.runtime / "jcode-debug.sock"
        self.journal = self.jcode_home / "rollout/journal.json"
        self.launches = root / "desktop-launches"
        self.children = []
        for directory in (self.home, self.jcode_home, self.runtime, self.app, self.package / "Contents/MacOS"):
            directory.mkdir(parents=True)
        (self.app / "old-bundle").write_text("previous bundle must never disappear")
        builds = self.jcode_home / "builds"
        previous = builds / "versions" / self.old_label / "jcode"
        previous.parent.mkdir(parents=True)
        shutil.copy2(old, previous)
        for channel in ("current", "shared-server"):
            directory = builds / channel
            directory.mkdir()
            (directory / "jcode").symlink_to(previous)
        (builds / "current-version").write_text(self.old_label)
        (builds / "shared-server-version").write_text(self.old_label)
        shutil.copy2(new, self.package / "Contents/MacOS/jcode")
        (self.package / "Contents/Info.plist").write_text("<plist><dict><key>SUPublicEDKey</key><string>fixture-key</string></dict></plist>")
        source = root / "desktop.c"
        source.write_text(DESKTOP_C)
        subprocess.run(["cc", str(source), "-o", str(self.package / "Contents/MacOS/jcode-desktop")], check=True)
        self.env = dict(os.environ)
        for key in ("JCODE_TEST_SESSION", "JCODE_READY_FD", "JCODE_SESSION_ID", "JCODE_RELOAD_AUTH_STATUS",
                    "JCODE_RUNTIME_TARGET", "JCODE_RUNTIME_RELOAD_ID", "JCODE_RUNTIME_GENERATION"):
            self.env.pop(key, None)
        self.env.update(HOME=str(self.home), JCODE_HOME=str(self.jcode_home), JCODE_RUNTIME_DIR=str(self.runtime),
                        JCODE_SOCKET=str(self.socket), JCODE_API_SOCKET=str(self.runtime / "jcode-api.sock"),
                        JCODE_ROLLOUT_APP=str(self.app), JCODE_DEFERRED_AUTH_BOOTSTRAP="1",
                        ROLLOUT_DESKTOP_LAUNCHES=str(self.launches))
        if pause_backup:
            commands = root / "commands"
            commands.mkdir()
            trap = commands / "ditto"
            trap.write_text("#!/bin/sh\n/usr/bin/ditto \"$@\" || exit $?\ncase \"$2\" in *.backup.app)\nif [ ! -e \"$ROLLOUT_BACKUP_PAUSE\" ]; then\n echo copied > \"$ROLLOUT_BACKUP_PAUSE\"\n while [ ! -e \"$ROLLOUT_BACKUP_RELEASE\" ]; do sleep 0.05; done\nfi\n;; esac\n")
            trap.chmod(0o755)
            self.env.update(PATH=f"{commands}:{self.env['PATH']}", ROLLOUT_BACKUP_PAUSE=str(root / "backup-paused"), ROLLOUT_BACKUP_RELEASE=str(root / "backup-release"))

    def start(self, binary=None):
        log = open(self.root / f"daemon-{len(self.children)}.log", "wb")
        child = subprocess.Popen([str(binary or self.old), "--no-update", "--provider", "anthropic", "serve", "--socket", str(self.socket)],
                                 env=self.env, stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
        self.children.append(child)
        wait_for("isolated daemon Pong", lambda: request(self.socket, {"type": "ping", "id": 1}).get("type") == "pong")
        return child

    def intent(self, binary=None, label=None):
        return dict(cli=label or self.new_label, desktop="7654321", binary=str(binary or self.new), bundle=str(self.package), app=str(self.app), now=True,
                    drain_timeout_secs=30, triggering_session=None)

    def submit(self, intent=None):
        event = request(self.debug, dict(type="debug_command", id=7, command="server:rollout_submit:" + json.dumps(intent or self.intent())))
        assert event.get("ok"), event
        return json.loads(event["output"])

    def state(self):
        return json.loads(self.journal.read_text())

    def completed(self):
        state = self.state()
        if state["phase"] in ("failed", "rolled_back"):
            raise AssertionError(f"rollout failed: {state}")
        return state if state["phase"] == "completed" else None

    def stop(self):
        for child in self.children:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
        if self.launches.exists():
            for value in self.launches.read_text().splitlines():
                try:
                    os.kill(int(value), signal.SIGKILL)
                except ProcessLookupError:
                    pass


def caller_death_and_exec(root, old, new):
    host = Host(root, old, new)
    try:
        daemon = host.start()
        caller_script = root / "caller.py"
        caller_script.write_text("import json,socket,sys,time\ns=socket.socket(socket.AF_UNIX);s.connect(sys.argv[1]);s.sendall((json.dumps({'type':'debug_command','id':1,'command':'server:rollout_submit:'+sys.argv[2]})+'\\n').encode());e=json.loads(s.makefile().readline());assert e.get('ok'),e;open(sys.argv[3],'w').write(e['output']);time.sleep(120)\n")
        ack = root / "caller-ack"
        caller = subprocess.Popen([sys.executable, str(caller_script), str(host.debug), json.dumps(host.intent()), str(ack)], env=host.env)
        wait_for("durable submission acknowledgement", ack.exists)
        caller.kill()
        caller.wait()
        state = wait_for("rollout completion after caller SIGKILL and real exec", host.completed)
        assert state["owner_pid"] == daemon.pid, "exec should replace, not fork the serving daemon"
        assert state["serving"] and state["desktop_swapped"]
        assert (Path(state["backup_app"]) / "old-bundle").exists()
        assert not (host.app / "old-bundle").exists()
        assert host.launches.read_text().count("\n") == 1, "startup reconciliation duplicated Desktop launch"
        execs = [m for m in state["mutations"] if m["name"] == "exec"]
        assert len(execs) == 1 and execs[0]["result"] == "new generation serving", execs
        status = subprocess.run([str(new), "--no-update", "rollout", "--status"], env=host.env, check=True, capture_output=True, text=True)
        assert json.loads(status.stdout)["id"] == state["id"]
        return {"caller_sigkill": "passed", "real_exec_resume": "passed", "rollout_id": state["id"]}
    finally:
        host.stop()


def backup_crash(root, old, new):
    host = Host(root, old, new, pause_backup=True)
    try:
        daemon = host.start()
        host.submit()
        wait_for("copy completed before backup/swap crash", (root / "backup-paused").exists)
        os.killpg(daemon.pid, signal.SIGKILL)
        daemon.wait()
        assert (host.app / "old-bundle").read_text() == "previous bundle must never disappear"
        assert any((root / "Applications").glob(".Jcode.*.backup.app/old-bundle"))
        (root / "backup-release").touch()
        host.start(new)
        state = wait_for("startup replay after backup crash", host.completed)
        assert state["desktop_swapped"] and Path(state["backup_app"]).exists()
        return {"backup_swap_crash": "passed", "rollout_id": state["id"]}
    finally:
        host.stop()


def dead_generation(root, old, new):
    host = Host(root, old, new)
    try:
        daemon = host.start()
        bad = root / "bad-jcode"
        bad.write_text("#!/bin/sh\nif [ \"$1\" = version ]; then echo '{\"git_hash\":\"c33333333\",\"version\":\"fixture\"}'; exit 0; fi\nexit 73\n")
        bad.chmod(0o755)
        intent = host.intent(binary=bad, label="c33333333")
        intent["bundle"] = None
        host.submit(intent)
        daemon.wait(timeout=90)
        assert host.state()["phase"] == "exec_pending", host.state()
        result = subprocess.run([str(old), "--no-update", "--provider", "anthropic", "server", "start"], env=host.env, capture_output=True, text=True, timeout=120)
        assert result.returncode == 0, (result.stdout, result.stderr)
        pong = wait_for("previous generation respawn", lambda: request(host.socket, {"type": "ping", "id": 1}))
        identity = pong.get("daemon_identity") or {}
        pid = identity.get("pid")
        if pid:
            # This PID is observed only on our private socket, never the user's.
            host.children.append(ObservedChild(pid))
        state = host.state()
        assert state["phase"] == "rolled_back", state
        link = (host.jcode_home / "builds/shared-server/jcode").resolve()
        assert link.parent.name == host.old_label, link
        assert identity.get("git_hash", "").startswith(host.old_label), identity
        assert (host.app / "old-bundle").exists()
        return {"unserved_generation_fallback": "passed", "rollout_id": state["id"]}
    finally:
        host.stop()


class ObservedChild:
    def __init__(self, pid):
        self.pid = pid

    def wait(self, timeout=5):
        try:
            os.kill(self.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--old", type=Path, required=True)
    parser.add_argument("--new", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    scratch = Path(os.environ.get("JCODE_SCRATCH_DIR", Path.home() / ".jcode/scratch"))
    scratch.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="rollout-accept-", dir=scratch))
    results = {"root": str(root), "old": str(args.old), "new": str(args.new), "checks": []}
    for name, check in (("caller-exec", caller_death_and_exec), ("backup-crash", backup_crash), ("dead-generation", dead_generation)):
        directory = root / name
        directory.mkdir()
        result = check(directory, args.old.resolve(), args.new.resolve())
        results["checks"].append(result)
        print(json.dumps(result), flush=True)
    if args.output:
        args.output.write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
