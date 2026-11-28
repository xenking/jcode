#!/usr/bin/env python3
"""Prove ACP seat environment survives an isolated daemon restart.

Only the external model is fake. ACP, the daemon, session persistence, and Bash
use the supplied executable. Evidence is retained below JCODE_SCRATCH_DIR.
"""
import argparse
import asyncio
import json
import os
from pathlib import Path
import shutil
import signal
import sys
import uuid

REPO = Path(__file__).resolve().parent.parent


async def wait_for_socket(path, process):
    async def wait():
        while process.returncode is None:
            try:
                _, writer = await asyncio.open_unix_connection(str(path))
                writer.close()
                await writer.wait_closed()
                return
            except (OSError, ConnectionError):
                await asyncio.sleep(0.05)
        raise RuntimeError(f"isolated daemon exited with {process.returncode}")
    await asyncio.wait_for(wait(), 60)


async def daemon_identity(socket):
    reader, writer = await asyncio.open_unix_connection(str(socket))
    try:
        writer.write(b'{"type":"ping","id":1}\n')
        await writer.drain()
        while True:
            line = await reader.readline()
            if not line:
                raise RuntimeError("daemon closed before identifying itself")
            event = json.loads(line)
            if event.get("type") == "pong":
                return event["daemon_identity"]
    finally:
        writer.close()
        await writer.wait_closed()


async def run(binary, evidence, restart):
    evidence.mkdir(mode=0o700, parents=True, exist_ok=False)
    workspace = evidence / "workspace"
    workspace.mkdir()
    home = evidence / "home"
    home.mkdir()
    data = evidence / "data"
    data.mkdir()
    control = evidence / "control"
    control.mkdir()
    socket = evidence / "daemon.sock"
    if len(os.fsencode(socket)) >= 100:
        raise RuntimeError("evidence path is too long for a Unix socket")
    executable = evidence / "jcode"
    shutil.copy2(binary, executable)
    seat_value = "seat-" + uuid.uuid4().hex
    env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(home), "LANG": "en_US.UTF-8", "TMPDIR": str(evidence),
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_CACHE_HOME": str(home / ".cache"),
        "JCODE_HOME": str(data), "JCODE_RUNTIME_DIR": str(evidence),
        "JCODE_SOCKET": str(socket), "JCODE_TEST_SESSION": "1",
        "JCODE_NO_MENUBAR": "1", "JCODE_NO_TELEMETRY": "1",
        "JCODE_TELEMETRY": "off", "JCODE_NO_AUTO_UPDATE": "1",
        "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_TERMINAL_PROMPT": "0", "OPENAI_COMPAT_API_KEY": "synthetic-test-key",
        "JCODE_OPENAI_COMPAT_DEFAULT_MODEL": "fake-model",
    }
    processes = []
    logs = []
    acp = None

    async def spawn(label, args, process_env, stdin=None, stdout=None):
        stderr = (evidence / f"{label}.stderr").open("wb")
        logs.append(stderr)
        if stdout is None:
            stdout = (evidence / f"{label}.stdout").open("wb")
            logs.append(stdout)
        process = await asyncio.create_subprocess_exec(
            *map(str, args), cwd=workspace, env=process_env,
            stdin=stdin, stdout=stdout, stderr=stderr,
        )
        processes.append(process)
        return process

    try:
        provider_env = dict(env, FAKE_LOG=str(evidence / "provider.jsonl"),
                            FAKE_CONTROL_DIR=str(control),
                            FAKE_PORT_FILE=str(evidence / "provider.port"))
        await spawn("provider", [sys.executable, REPO / "scripts/fake_openai_server.py", "0"], provider_env)
        async def provider_port():
            while not (evidence / "provider.port").exists():
                await asyncio.sleep(0.05)
            return (evidence / "provider.port").read_text().strip()
        port = await asyncio.wait_for(provider_port(), 10)
        env["JCODE_OPENAI_COMPAT_API_BASE"] = f"http://127.0.0.1:{int(port)}/v1"
        base = [executable, "--no-update", "--no-selfdev", "--provider", "openai-compatible",
                "--model", "fake-model", "--tools", "bash", "--socket", socket]
        daemon = await spawn("daemon-before", base + ["serve"], env)
        await wait_for_socket(socket, daemon)
        acp = await spawn("acp", base + ["acp", "--session-env", "inherit"],
                          dict(env, FOO_SEAT=seat_value),
                          stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE)
        transcript = (evidence / "acp.jsonl").open("w")
        logs.append(transcript)

        async def rpc(request_id, method, params):
            acp.stdin.write((json.dumps({"jsonrpc": "2.0", "id": request_id,
                                        "method": method, "params": params}) + "\n").encode())
            await acp.stdin.drain()
            messages = []
            async def response():
                while True:
                    line = await acp.stdout.readline()
                    if not line:
                        raise RuntimeError(f"ACP exited with {acp.returncode}")
                    transcript.write(line.decode())
                    transcript.flush()
                    message = json.loads(line)
                    messages.append(message)
                    if message.get("id") == request_id:
                        return message
            reply = await asyncio.wait_for(response(), 130)
            if "error" in reply:
                raise RuntimeError(f"{method} failed: {reply['error']}")
            return reply["result"], messages

        await rpc(1, "initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        session, _ = await rpc(2, "session/new", {"cwd": str(workspace), "mcpServers": []})
        session_id = session["sessionId"]
        proof = {"acp_pid": acp.pid, "session_id": session_id, "seat_value": seat_value,
                 "restart": restart, "serving_before": await daemon_identity(socket), "turns": []}
        for number, label in [(3, "before restart"), (4, "after restart")]:
            result, messages = await rpc(number, "session/prompt", {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "[e2e:verify-env] Run bash echo $FOO_SEAT"}],
            })
            assert result["stopReason"] == "end_turn", result
            assert seat_value in json.dumps(messages), "Bash did not return the seat environment"
            assert acp.returncode is None, "ACP must stay alive"
            proof["turns"].append({"label": label, "stopReason": result["stopReason"], "output": seat_value})
            print(f"{label}: echo $FOO_SEAT -> {seat_value} [end_turn]", flush=True)
            if number == 3:
                proof["killed_daemon_pid"] = daemon.pid
                daemon.kill()
                await daemon.wait()
                if restart == "manual":
                    daemon = await spawn("daemon-after", base + ["serve"], env)
                    await wait_for_socket(socket, daemon)
        proof["serving_after"] = await daemon_identity(socket)
        assert proof["serving_after"]["pid"] != proof["serving_before"]["pid"], "daemon was not replaced"
        (evidence / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")
        print(f"PASS same ACP pid={acp.pid}, session={session_id}; evidence={evidence}", flush=True)
    finally:
        if acp is not None and acp.returncode is None:
            acp.kill()
            await acp.wait()
        try:
            identity = await asyncio.wait_for(daemon_identity(socket), 2)
        except (OSError, ConnectionError, RuntimeError, KeyError, asyncio.TimeoutError):
            identity = None
        if identity is not None:
            Path(identity["binary_path"]).resolve().relative_to(evidence)
            os.kill(identity["pid"], signal.SIGKILL)
        for process in reversed(processes):
            if process.returncode is None:
                process.kill()
            await process.wait()
        for log in logs:
            log.close()
        (evidence / "cleanup.json").write_text(json.dumps({
            "owned_processes_exited": all(p.returncode is not None for p in processes),
            "stopped_daemon_identity": identity,
        }) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--evidence", type=Path)
    parser.add_argument("--restart", choices=["manual", "auto"], default="manual")
    args = parser.parse_args()
    os.umask(0o077)
    scratch = Path(os.environ.get("JCODE_SCRATCH_DIR", Path.home() / ".jcode/scratch"))
    scratch.mkdir(parents=True, exist_ok=True)
    evidence = args.evidence or scratch / ("acp-" + uuid.uuid4().hex[:8])
    asyncio.run(run(args.binary.resolve(strict=True), evidence.resolve(), args.restart))


if __name__ == "__main__":
    main()
