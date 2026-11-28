#!/usr/bin/env python3
"""Exercise process lifetimes against a real Jcode binary in private homes."""

import argparse
import json
import os
from pathlib import Path
import select
import shutil
import socket
import subprocess
import tempfile
import time
import unittest


class Lifecycle(unittest.TestCase):
    def setUp(self):
        root = tempfile.TemporaryDirectory(prefix="jlife-", dir="/tmp")
        self.addCleanup(root.cleanup)
        self.root = Path(root.name)
        self.env = os.environ.copy()
        for key in ("JCODE_SERVER_SCOPE", "JCODE_TEMP_SERVER", "JCODE_SERVER_OWNER_PID",
                    "JCODE_SERVER_OWNER_FD", "JCODE_SOCKET", "JCODE_COMPANION_SOCKET",
                    "JCODE_READY_FD"):
            self.env.pop(key, None)
        self.env.update(
            JCODE_HOME=str(self.root / "home"),
            JCODE_RUNTIME_DIR=str(self.root / "rt"),
            JCODE_TEST_SESSION="1",
            JCODE_NO_MENUBAR="1",
            JCODE_COMPANION_AUTOSTART="0",
            JCODE_DEFERRED_AUTH_BOOTSTRAP="1",
            JCODE_TEMP_SERVER_IDLE_SECS="1",
        )
        (self.root / "home").mkdir()
        (self.root / "rt").mkdir()
        self.sock = self.root / "private.sock"

    def run_cli(self, *args, binary=None, **kwargs):
        env = kwargs.pop("env", self.env)
        stderr = self.root / "stderr.log"
        with stderr.open("ab") as log:
            child = subprocess.Popen(
                [str(binary or BINARY), "--no-update", "--no-selfdev", *args],
                cwd=self.root, env=env, stderr=log, **kwargs,
            )
        self.addCleanup(self.reap_child, child)
        return child

    @staticmethod
    def reap_child(child):
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
        for pipe in (child.stdin, child.stdout, child.stderr):
            if pipe is not None:
                pipe.close()

    def eventually(self, predicate, timeout=10):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if predicate():
                return
            time.sleep(0.05)
        log = self.root / "stderr.log"
        self.fail(f"Condition still false after {timeout}s; stderr: {log.read_text()[-4000:]}")

    def connect(self, path=None):
        connection = socket.socket(socket.AF_UNIX)
        try:
            connection.connect(str(path or self.sock))
        except OSError:
            connection.close()
            raise
        return connection

    def test_private_daemon_exits_after_last_client(self):
        daemon = self.run_cli("serve", "--socket", str(self.sock))
        self.eventually(self.sock.exists, timeout=15)
        with self.connect():
            # A real ACP-like attach is unnecessary to prove the listener won't
            # retire a connection while reading its first request.
            time.sleep(2)
            self.assertIsNone(daemon.poll())
        start = time.monotonic()
        self.eventually(lambda: daemon.poll() is not None, timeout=13)
        self.assertFalse(self.sock.exists())
        self.assertFalse(self.sock.with_name("private-debug.sock").exists())
        print(f"private serve: exited {time.monotonic() - start:.2f}s after last client")

    def test_private_daemon_can_be_explicitly_persistent(self):
        daemon = self.run_cli("serve", "--persistent-server", "--socket", str(self.sock))
        self.eventually(self.sock.exists, timeout=15)
        time.sleep(2)
        self.assertIsNone(daemon.poll())
        with self.connect():
            pass

    def test_private_runtime_default_socket_exits_in_test_mode(self):
        daemon = self.run_cli("serve", env=dict(self.env, JCODE_DEBUG_CONTROL="1"))
        path = self.root / "rt" / "jcode.sock"
        self.eventually(path.exists, timeout=15)
        start = time.monotonic()
        self.eventually(lambda: daemon.poll() is not None, timeout=3)
        self.assertFalse(path.exists())
        print(f"test runtime default serve: exited {time.monotonic() - start:.2f}s after bind")

    def start_acp(self):
        acp = self.run_cli("acp", "--socket", str(self.sock), stdin=subprocess.PIPE,
                           stdout=subprocess.PIPE,
                           env=dict(self.env, JCODE_TEMP_SERVER_IDLE_SECS="1800"))
        acp.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "session/new",
                                    "params": {"cwd": str(self.root), "mcpServers": []}}).encode() + b"\n")
        acp.stdin.flush()
        self.eventually(self.sock.exists, timeout=15)
        pids = subprocess.check_output(["pgrep", "-P", str(acp.pid)], text=True).split()
        self.assertEqual(len(pids), 1)
        pid = int(pids[0])
        self.addCleanup(self.reap_acp_daemon, acp, pid)
        pending = b""
        end = time.monotonic() + 10
        while time.monotonic() < end:
            ready, _, _ = select.select([acp.stdout], [], [], max(0, end - time.monotonic()))
            if not ready:
                break
            chunk = os.read(acp.stdout.fileno(), 65536)
            if not chunk:
                break
            *lines, pending = (pending + chunk).split(b"\n")
            for line in lines:
                message = json.loads(line)
                if message.get("id") == 1:
                    self.assertIn("result", message)
                    return acp, pid
        self.fail("ACP did not complete session/new")

    def reap_acp_daemon(self, acp, pid):
        self.reap_child(acp)
        self.reap_daemon(pid)

    def test_private_acp_daemon_exits_when_its_parent_goes_away(self):
        acp, pid = self.start_acp()
        start = time.monotonic()
        acp.stdin.close()
        acp.wait(timeout=10)
        self.eventually(lambda: not self.pid_exists(pid), timeout=3)
        self.assertFalse(self.sock.exists())
        print(f"ACP child: exited {time.monotonic() - start:.2f}s after parent EOF")

    def test_private_acp_daemon_exits_when_its_parent_crashes(self):
        acp, pid = self.start_acp()
        start = time.monotonic()
        acp.kill()
        acp.wait(timeout=5)
        self.eventually(lambda: not self.pid_exists(pid), timeout=3)
        self.assertFalse(self.sock.exists())
        print(f"ACP child: exited {time.monotonic() - start:.2f}s after parent SIGKILL")

    def test_acp_reaps_its_daemon_that_exits_first(self):
        acp, pid = self.start_acp()
        os.kill(pid, 15)
        self.eventually(lambda: not self.pid_exists(pid), timeout=5)
        self.assertIsNone(acp.poll())

    @staticmethod
    def pid_exists(pid):
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False

    def reap_daemon(self, pid):
        if self.pid_exists(pid):
            try:
                os.kill(pid, 15)
            except ProcessLookupError:
                pass
            for _ in range(100):
                if not self.pid_exists(pid):
                    return
                time.sleep(0.05)

    def test_companion_keeps_an_unresponsive_live_listener(self):
        self.env["JCODE_COMPANION_SOCKET"] = str(self.sock)
        with socket.socket(socket.AF_UNIX) as owner:
            owner.bind(str(self.sock))
            owner.listen()
            inode = self.sock.stat().st_ino
            contender = self.run_cli("companion", "--no-voice", "--no-browser-updates")
            self.eventually(lambda: contender.poll() is not None, timeout=3)
            self.assertEqual(self.sock.stat().st_ino, inode)

    def test_companion_uses_one_listener_across_builds(self):
        # Both builds share an isolated home but not a daemon runtime dir.
        # Before the fix each has its own companion socket in its runtime dir.
        socket_name = "jcode-companion.sock"
        expected = self.root / "home" / socket_name
        old_path = self.root / "rt" / socket_name
        first = self.run_cli("companion", "--no-voice", "--no-browser-updates")
        self.eventually(lambda: expected.exists() or old_path.exists(), timeout=15)
        other = self.root / "other-build"
        try:
            os.link(BINARY, other)
        except OSError:
            shutil.copy2(BINARY, other)
        other_rt = self.root / "other-rt"
        other_rt.mkdir()
        second = self.run_cli("companion", "--no-voice", "--no-browser-updates", binary=other,
                              env=dict(self.env, JCODE_RUNTIME_DIR=str(other_rt)))
        self.eventually(lambda: second.poll() is not None, timeout=5)
        self.assertIsNone(first.poll())
        self.assertFalse((other_rt / socket_name).exists())
        with self.connect(expected):
            pass


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=lambda path: Path(path).resolve())
    args, remaining = parser.parse_known_args()
    BINARY = args.binary
    unittest.main(argv=[__file__, *remaining])
