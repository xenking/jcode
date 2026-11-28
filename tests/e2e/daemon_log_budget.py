#!/usr/bin/env python3
"""Probe default log levels on a private daemon with a loopback provider.

Usage: python3 tests/e2e/daemon_log_budget.py /absolute/path/to/jcode [--measure-only]
"""

import argparse
from collections import Counter
from contextlib import ExitStack
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile

from idle_unload_daemon import wait_for


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def main(binary, measure_only):
    fake = Path(__file__).resolve().parents[2] / "scripts/fake_openai_server.py"
    with tempfile.TemporaryDirectory(prefix="jlog-", dir=os.environ.get("JCODE_SCRATCH_DIR")) as tmp:
        root = Path(tmp)
        home, jhome, client_home = root / "h", root / "j", root / "c"
        for directory in (home, jhome, client_home):
            directory.mkdir(mode=0o700)
        sock, port = root / "d.sock", root / "port"
        requests = root / "requests.jsonl"
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(home),
            "JCODE_HOME": str(jhome),
            "JCODE_RUNTIME_DIR": str(root),
            "TMPDIR": str(root),
            "JCODE_TEST_SESSION": "1",
            "JCODE_NO_AUTO_UPDATE": "1",
            "JCODE_NO_TELEMETRY": "1",
            "JCODE_PROVIDER": "openai-compatible",
            "JCODE_MODEL": "fake-model",
            "OPENAI_COMPAT_API_KEY": "fixture-only",
            "JCODE_OPENAI_COMPAT_DEFAULT_MODEL": "fake-model",
            "FAKE_LOG": str(requests),
            "FAKE_PORT_FILE": str(port),
            "FAKE_CONTROL_DIR": str(root),
            "FAKE_LABEL": "log-budget",
        }
        config = jhome / "config.toml"
        config.write_text(
            '[session]\nunload_idle_secs = 1\nhibernation_idle_secs = 0\n'
            '[server]\nmobile_push = false\n',
            encoding="utf-8",
        )
        command = [str(binary), "--no-update", "--no-selfdev"]
        with ExitStack() as stack:
            output = stack.enter_context((root / "daemon.log").open("w+"))
            provider = subprocess.Popen([sys.executable, str(fake), "0"], cwd=root, env=env)
            stack.callback(stop, provider)
            wait_for(port.exists, "provider port")
            env["JCODE_OPENAI_COMPAT_API_BASE"] = f"http://127.0.0.1:{port.read_text().strip()}/v1"
            daemon = subprocess.Popen(
                [*command, "serve", "--socket", str(sock)], cwd=root, env=env,
                stdout=output, stderr=output,
            )
            stack.callback(stop, daemon)

            def debug(*args):
                return subprocess.run(
                    [*command, "debug", *args, "--socket", str(sock)],
                    env=dict(env, JCODE_HOME=str(client_home)),
                    cwd=root, capture_output=True, text=True, timeout=30,
                ).stdout

            try:
                wait_for(lambda: sock.exists() and debug("server:info"), "daemon ready")
                log = next((jhome / "logs").glob("jcode-*.log"))
                start = log.stat().st_size
                session = json.loads(debug(f"create_session:{root}"))["session_id"]
                for i in range(100):
                    with socket.socket(socket.AF_UNIX) as peer:
                        peer.settimeout(5)
                        peer.connect(str(sock))
                        peer.sendall(f'{{"type":"ping","id":{i}}}\n'.encode())
                        with peer.makefile("rb") as stream:
                            assert any(
                                event["type"] == "pong" and event["id"] == i
                                for event in map(json.loads, stream)
                            ), f"no pong for {i}"
                for i in range(4):
                    prompt = f"[e2e:verify-write] isolated log sample {i}"
                    reply = debug("message", prompt, "--session", session, "--wait")
                    assert "[log-budget] ack" in reply, reply
                assert (root / "verify-tool.txt").is_file(), "no tool side effect"
                # Force the next subscribe through persisted history rather
                # than reading the already-resident session transcript.
                def evicted():
                    stats = json.loads(debug("server:memory"))["sessions"]
                    return stats["idle_eviction_count"] > 0 and stats["live_count"] == 0

                wait_for(evicted, "idle session eviction")
                with socket.socket(socket.AF_UNIX) as peer:
                    peer.settimeout(5)
                    peer.connect(str(sock))
                    subscribe = {"type": "subscribe", "id": 201, "target_session_id": session}
                    peer.sendall((json.dumps(subscribe) + "\n").encode())
                    with peer.makefile("rb") as stream:
                        history = next(
                            event for event in map(json.loads, stream)
                            if event.get("id") == 201 and event["type"] in ("history", "error")
                        )
                    assert history["type"] == "history" and history["session_id"] == session, history
                with log.open("rb") as reader:
                    reader.seek(start)
                    sample = reader.read()
            except Exception:
                output.seek(0)
                print(output.read(), file=sys.stderr)
                raise

        keys = [b"main accept:", b"session_load:", b"mobile_sync stage=", b"API call starting:",
                b"API call complete", b"API stream opened", b"Prompt prefix estimate:",
                b"messages_for_provider (", b"PROVIDER_CANONICAL_INPUT", b"phase=load_done",
                b"phase=save_done", b"event=TOOL_LIFECYCLE", b"event=AGENT_PROVIDER_STREAM_LIFECYCLE"]
        info = [line for line in sample.splitlines(keepends=True) if b"] [INFO] " in line]
        matches = Counter({key: sum(len(line) for line in info if key in line) for key in keys})
        completions = [line for line in info if keys[-1] in line and b"phase=stream_complete" in line]
        tools = [line for line in info if keys[-2] in line]
        nreq = sum(1 for row in requests.read_text().splitlines() if json.loads(row)["agent"])
        print(f"daemon log: {len(sample)} bytes for 100 pings, {nreq} provider requests and cold reattach")
        print("INFO bytes by template:", {key.decode(): value for key, value in matches.items() if value})
        if not measure_only:
            assert not any(matches[key] for key in keys[:-2]), matches
            assert len(tools) == 4, tools
            assert all(b"phase=done" in line and b"tool=write" in line and len(line) < 250
                       for line in tools), tools
            assert nreq >= 4 and len(completions) == nreq, (nreq, completions)
            assert matches[keys[-1]] == sum(map(len, completions)), matches
            for line in completions:
                for field in (b"provider=", b"model=fake-model", b"elapsed_ms=", b"input=1", b"output=1",
                              b"cache_read=", b"cache_write="):
                    assert field in line, line


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--measure-only", action="store_true")
    args = parser.parse_args()
    main(args.binary.resolve(), args.measure_only)
