#!/usr/bin/env python3
"""Verify daemon idle unload and reattachment with a loopback fake provider.

Usage: python3 tests/e2e/idle_unload_daemon.py /absolute/path/to/jcode
"""

import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time


def wait_for(check, description, timeout=45):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.2)
    raise AssertionError(f"timed out: {description}")


def main(binary):
    fake_provider = Path(__file__).resolve().parents[2] / "scripts/fake_openai_server.py"
    with tempfile.TemporaryDirectory(prefix="ju-", dir=os.environ.get("JCODE_SCRATCH_DIR")) as tmp:
        root = Path(tmp)
        home, jhome, runtime = (root / name for name in ("home", "jcode", "run"))
        for path in (home, jhome, runtime):
            path.mkdir(mode=0o700)
        daemon_socket = runtime / "d.sock"
        port_file = root / "port"
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(home),
            "JCODE_HOME": str(jhome),
            "JCODE_RUNTIME_DIR": str(runtime),
            "XDG_RUNTIME_DIR": str(runtime),
            "TMPDIR": str(root),
            "JCODE_TEST_SESSION": "1",
            "JCODE_NO_AUTO_UPDATE": "1",
            "JCODE_NO_TELEMETRY": "1",
            "JCODE_PROVIDER": "openai-compatible",
            "JCODE_MODEL": "fake-model",
            "OPENAI_COMPAT_API_KEY": "fixture-only",
            "JCODE_OPENAI_COMPAT_DEFAULT_MODEL": "fake-model",
            "FAKE_LOG": str(root / "provider-requests.jsonl"),
            "FAKE_PORT_FILE": str(port_file),
            "FAKE_LABEL": "idle-unload",
        }
        (jhome / "config.toml").write_text(
            '[session]\nunload_idle_secs = 1\nhibernation_idle_secs = 0\n'
            '[server]\nmobile_push = false\n',
            encoding="utf-8",
        )
        provider = subprocess.Popen([sys.executable, str(fake_provider), "0"], env=env, cwd=root)
        daemon = None
        with (root / "daemon.log").open("w+") as log:
            try:
                wait_for(port_file.exists, "fake provider", timeout=10)
                env["JCODE_OPENAI_COMPAT_API_BASE"] = (
                    f"http://127.0.0.1:{port_file.read_text().strip()}/v1"
                )
                command = [str(binary), "--no-update", "--no-selfdev"]
                daemon = subprocess.Popen(
                    [*command, "serve", "--socket", str(daemon_socket)],
                    env=env, cwd=root, stdout=log, stderr=log,
                )

                def debug(*args, check=True):
                    result = subprocess.run(
                        [*command, "debug", *args, "--socket", str(daemon_socket)],
                        env=env, cwd=root, capture_output=True, text=True, timeout=15,
                    )
                    if result.returncode != 0:
                        if check:
                            raise AssertionError(f"debug {args}: {result.stderr} {result.stdout}")
                        return None
                    return result.stdout

                wait_for(lambda: debug("server:info", check=False), "daemon debug socket")
                session = json.loads(debug(f"create_session:{root}"))["session_id"]
                prompt = "fixture transcript before idle unload"
                assert "[idle-unload] ack" in debug("message", prompt, "--session", session, "--wait")

                def stats():
                    return json.loads(debug("server:memory"))["sessions"]

                assert any(row["session_id"] == session for row in stats()["top_by_json_bytes"])
                def after_eviction():
                    sample = stats()
                    if sample["idle_eviction_count"] < 1:
                        return None
                    assert all(row["session_id"] != session for row in sample["top_by_json_bytes"])
                    assert sample["live_count"] == 0, sample
                    return sample

                unloaded = wait_for(after_eviction, "idle transcript eviction")
                with socket.socket(socket.AF_UNIX) as peer:
                    peer.settimeout(10)
                    peer.connect(str(daemon_socket))
                    peer.sendall((json.dumps({"type": "subscribe", "id": 42,
                                             "target_session_id": session}) + "\n").encode())
                    with peer.makefile("rb") as lines:
                        for line in lines:
                            event = json.loads(line)
                            if event.get("id") != 42:
                                continue
                            if event["type"] == "error":
                                raise AssertionError(event)
                            if event["type"] == "history":
                                assert event["session_id"] == session
                                texts = [message["content"] for message in event["messages"]]
                                assert prompt in texts, texts
                                assert any("[idle-unload] ack" in text for text in texts), texts
                                break
                        else:
                            raise AssertionError("subscribe ended without history")
                        assert any(row["session_id"] == session for row in stats()["top_by_json_bytes"])
                print(f"PASS: idle eviction count {unloaded['idle_eviction_count']}; socket subscribe restored {session}")
            except Exception:
                log.seek(0)
                print(log.read(), file=sys.stderr)
                raise
            finally:
                for process in (daemon, provider):
                    if process is not None and process.poll() is None:
                        process.terminate()
                        try:
                            process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait()


if __name__ == "__main__":
    main(Path(sys.argv[1]).resolve())
