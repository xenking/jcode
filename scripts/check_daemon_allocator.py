#!/usr/bin/env python3
"""Smoke-test the allocator of a freshly built macOS or Linux daemon."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time

from profile_server_memory import debug_cmd, isolated_env


def check(binary: Path, root: Path) -> None:
    for name in ("home", "run", "tmp", "work"):
        (root / name).mkdir()
    env = isolated_env(root, {"HOME": str(root / "home")})
    socket = root / "run" / "jcode.sock"
    debug_socket = root / "run" / "jcode-debug.sock"
    log_path = root / "daemon.log"
    with log_path.open("wb") as log:
        daemon = subprocess.Popen(
            [str(binary), "--no-update", "--no-selfdev", "--socket", str(socket), "serve"],
            cwd=root / "work", env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
        )
        try:
            deadline = time.monotonic() + 30
            while not debug_socket.is_socket():
                if daemon.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError(f"daemon did not start:\n{log_path.read_text()}")
                time.sleep(0.05)

            def debug(command: str) -> dict:
                return json.loads(debug_cmd(debug_socket, command, None, 10))

            allocator = debug("server:memory")["process"]["allocator"]
            assert allocator["name"] == "jemalloc" and allocator["stats_available"], allocator
            assert allocator["stats"]["allocated_bytes"] > 0, allocator
            tuning = allocator["tuning"]
            assert tuning["dirty_decay_ms"] == tuning["muzzy_decay_ms"] == 1000, tuning
            assert allocator["profiling"] == {"available": True, "enabled": True, "active": False}, allocator
            try:
                assert debug("allocator:profile:on")["allocator"]["profiling"]["active"] is True
                heap_path = root / "daemon.heap"
                debug(f"allocator:profile:dump {heap_path}")
                heap = heap_path.read_text()
                assert heap.startswith("heap_v2/") and "MAPPED_LIBRARIES:" in heap, heap[:300]
                assert str(binary) in heap.split("MAPPED_LIBRARIES:", 1)[1], heap[-1000:]
            finally:
                assert debug("allocator:profile:off")["allocator"]["profiling"]["active"] is False
        finally:
            daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=5)


if __name__ == "__main__":
    # Keep Unix socket paths short even on hosts with a long default TMPDIR.
    with tempfile.TemporaryDirectory(prefix="jc-alloc-", dir="/tmp") as directory:
        check(Path(sys.argv[1]).resolve(strict=True), Path(directory))
    print("Daemon uses jemalloc with stats, configured decay and opt-in heap sampling.")
