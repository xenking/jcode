#!/usr/bin/env python3
"""Run two real rollout clients with fake build commands and a private submit socket."""

import argparse
import json
import os
from pathlib import Path
import signal
import socketserver
import subprocess
import tempfile
import threading
import time


TIMEOUT = 20


def git(path, *args):
    return subprocess.check_output(["git", "-C", str(path), *args], text=True).strip()


def commit(path, name):
    git(path, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
        "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-qm", name)
    return git(path, "rev-parse", "HEAD")


def until(check):
    deadline = time.monotonic() + TIMEOUT
    while not check():
        assert time.monotonic() < deadline, "fixture timed out"
        time.sleep(0.02)


def executable(path, contents):
    path.write_text(contents)
    path.chmod(0o755)


class Host:
    def __init__(self, root, binary):
        self.root = root
        self.binary = binary
        self.processes = []
        self.submissions = []
        self.messages = []
        self.journal = None
        self.env = dict(os.environ)
        for key in ("JCODE_RUNTIME_TARGET", "JCODE_RUNTIME_RELOAD_ID", "JCODE_RUNTIME_GENERATION"):
            self.env.pop(key, None)
        for directory in ("home", "bin", "projects/jcode", "projects/jcode-desktop", "jcode/rollout"):
            (root / directory).mkdir(parents=True)
        self.env.update(HOME=str(root / "home"), JCODE_HOME=str(root / "jcode"),
                        JCODE_RUNTIME_DIR=str(root), JCODE_SOCKET=str(root / "j.sock"),
                        JCODE_ROLLOUT_PROJECTS=str(root / "projects"), JCODE_TEST_SESSION="1",
                        JCODE_ROLLOUT_APP=str(root / "Jcode.app"),
                        CARGO_TARGET_DIR=str(root / "target"), FIXTURE=str(root),
                        PATH=f"{root / 'bin'}:{os.environ['PATH']}")
        self.repo = root / "projects/jcode"
        self.worktree = root / "jcode/wt/rollout/jcode"
        desktop = root / "projects/jcode-desktop"
        for repo in (self.repo, desktop):
            git(repo, "init", "-q")
            git(repo, "remote", "add", "origin", ".")
        self.old = commit(self.repo, "old")
        self.new = commit(self.repo, "new")
        git(self.repo, "checkout", "-q", "--detach", self.old)
        self.other = commit(self.repo, "divergent")
        self.desktop = commit(desktop, "desktop")
        executable(root / "bin/df", "#!/bin/sh\nprintf 'header\nfixture 0 0 41943040 0 /\n'\n")
        executable(root / "bin/cargo", '''#!/bin/sh
set -eu
sha=$(git rev-parse HEAD)
printf '%s %s\n' "$JCODE_SESSION_ID" "$sha" >> "$FIXTURE/builds"
if [ "$JCODE_SESSION_ID" = owner ]; then
  while [ ! -f "$FIXTURE/release" ]; do sleep 0.02; done
fi
[ "$(git rev-parse HEAD)" = "$sha" ] || exit 9
path="$CARGO_TARGET_DIR/aarch64-apple-darwin/release/jcode"
mkdir -p "$(dirname "$path")"
printf '#!/bin/sh\necho '\\''{"git_hash":"%s"}'\\''\n' "$sha" > "$path"
chmod +x "$path"
''')
        host = self

        class Handler(socketserver.StreamRequestHandler):
            def handle(self):
                request = json.loads(self.rfile.readline())
                command = request.get("command", "")
                if command == "server:rollout_status":
                    result = host.journal
                elif command == "server:turn_activity":
                    result = {"drain_sessions": []}
                elif command.startswith("server:rollout_submit:"):
                    intent = json.loads(command.split(":", 2)[2])
                    host.submissions.append(intent)
                    host.journal = {"schema": 1, "id": f"fixture-{len(host.submissions)}",
                                    "intent": intent, "phase": "submitted", "owner_pid": os.getpid(),
                                    "socket": str(root / "j.sock"), "previous_shared": "abcdef123",
                                    "previous_binary": "/fixture/old", "staged_binary": "/fixture/new",
                                    "incoming_app": "/fixture/incoming", "backup_app": "/fixture/backup",
                                    "backup_ready": False, "desktop_swapped": False, "serving": False,
                                    "mutations": []}
                    host.save_journal()
                    result = host.journal
                elif request["type"] == "comm_message":
                    host.messages.append(request)
                    self.wfile.write(b'{"type":"ack","id":1}\n')
                    return
                else:
                    return
                event = {"type": "debug_response", "id": 1, "ok": True,
                         "output": json.dumps(result)}
                self.wfile.write((json.dumps(event) + "\n").encode())

        self.servers = [socketserver.ThreadingUnixStreamServer(str(root / name), Handler)
                        for name in ("j.sock", "j-debug.sock")]
        for server in self.servers:
            threading.Thread(target=server.serve_forever, daemon=True).start()

    def save_journal(self, phase=None):
        if phase:
            self.journal["phase"] = phase
        path = self.root / "jcode/rollout/journal.json"
        temporary = path.with_suffix(".tmp")
        temporary.write_text(json.dumps(self.journal))
        temporary.replace(path)

    def start(self, sha, session):
        env = dict(self.env, JCODE_SESSION_ID=session or "")
        if session is None:
            env.pop("JCODE_SESSION_ID")
        process = subprocess.Popen([self.binary, "--no-update", "rollout", "--no-desktop",
                                    "--jcode-ref", sha, "--desktop-ref", self.desktop], env=env,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                                   start_new_session=True)
        self.processes.append(process)
        return process

    def builds(self):
        path = self.root / "builds"
        return path.read_text().splitlines() if path.exists() else []

    def release(self):
        (self.root / "release").touch()

    def close(self):
        for process in self.processes:
            # A crashed CLI can leave the fixture cargo shell alive.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate(timeout=TIMEOUT)
        for server in self.servers:
            server.shutdown()
            server.server_close()


def result(process, success):
    text, _ = process.communicate(timeout=TIMEOUT)
    assert (process.returncode == 0) == success, text
    return text


def exercise(host, case):
    owner = host.start(host.old, "owner")
    until(lambda: len(host.builds()) == 1)
    if case == "divergent":
        contender = host.start(host.other, "contender")
        until(lambda: contender.poll() is not None or len(host.builds()) > 1)
        assert git(host.worktree, "rev-parse", "HEAD") == host.old, "second caller changed active build's checkout"
        text = result(contender, False)
        assert all(item in text for item in ("owner", host.old, host.other, host.desktop)), text
        until(lambda: host.messages)
        assert host.messages[0]["delivery"] == "notify" and host.messages[0]["wake"] is False
    elif case in ("same", "newer", "orphan"):
        if case == "orphan":
            owner.kill()
            until(lambda: owner.poll() is not None)
        contender = host.start(host.new if case == "newer" else host.old, None)
        time.sleep(0.7)
        assert host.builds() == [f"owner {host.old}"], "two builds entered preparation concurrently"
        assert not host.submissions, host.submissions
    host.release()
    if case != "orphan":
        assert "fixture-1" in result(owner, True)
    if case == "same":
        assert "fixture-1" in result(contender, True)
        assert len(host.submissions) == 1 and len(host.builds()) == 1
    elif case == "newer":
        time.sleep(0.7)
        assert len(host.builds()) == 1 and contender.poll() is None, "newer changed prep before terminal handoff"
        host.save_journal("completed")
        assert "fixture-2" in result(contender, True)
        assert len(host.builds()) == 2 and len(host.submissions) == 2
    elif case == "orphan":
        assert "fixture-1" in result(contender, True)
        assert len(host.builds()) == 1 and len(host.submissions) == 1
    print(f"PASS {case}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--case", choices=("same", "newer", "divergent", "orphan"), action="append")
    args = parser.parse_args()
    for case in args.case or ("divergent", "same", "newer", "orphan"):
        with tempfile.TemporaryDirectory(prefix="jrm-", dir="/tmp") as tmp:
            host = Host(Path(tmp), args.binary.resolve())
            try:
                exercise(host, case)
            finally:
                host.close()


if __name__ == "__main__":
    main()
