#!/usr/bin/env python3
"""Check steady-state idle load on a local macOS or Linux Jcode binary.

Run `python3 scripts/check_idle_load.py target/selfdev/jcode` (60 seconds).
One Desktop-like client uses the actual API bridge: hello, session attachment,
list_sessions every 3s, and the bridge's own background-task watch. No prompts,
user configuration, or credentials are used. This measures that workload, not
an entire Desktop install or idle sessions with real OAuth accounts.
The daemon and bridge share the isolated log directory, so log bytes include
both writers. Only the daemon's main accept marker counts as a connection.
An enabled ambient runner with a future wake also exercises idle safety sweeps.

Initial baseline (November 22, 2026, macOS, d41186c43): 50 accepts/min,
14,160 log bytes/min, 0.06 daemon CPU s/min and no state rewrites. On the
archived pre-fix build 1ced95d9c the same workload rewrote each safety queue
file 41 times. Other known per-file ceilings start at the live audit counts,
including usage files not expected to be exercised without real logins.

To ratchet scripts/idle_load_budget.json, run the same 60s check before and after
an idle fix, repeating on the CI platform before removing headroom. Lower only
the improved limits. Prefer a zero budget for a path known to never write at
idle. Per-path globs are first-match; unlisted paths still have a limit. Fast
--seconds runs are for debugging only: rates are sensitive to single events and
Linux ps reports whole CPU seconds. File polling counts observed mtime/inode
changes, not individual I/O syscalls; several writes between samples can coalesce.
"""

import argparse
from collections import Counter
from contextlib import contextmanager
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from fnmatch import fnmatchcase
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import math
import os
from pathlib import Path
import re
import signal
import socket
import stat
import subprocess
import sys
import tempfile
from threading import Thread
import time


BUDGET_FILE = Path(__file__).with_name("idle_load_budget.json")
ACCEPT = "[TIMING] main accept: connection accepted"
LIST_INTERVAL = 3.0
SAMPLE_INTERVAL = 0.25
START_TIMEOUT = 30
API_TIMEOUT = 10


def parse_cpu_time(text):
    match = re.fullmatch(r"(?:(\d+)-)?(?:(\d+):)?(\d+):(\d+(?:\.\d+)?)", text.strip())
    if not match:
        raise ValueError(f"unexpected ps CPU time: {text!r}")
    days, hours, minutes, seconds = match.groups(default="0")
    return float(days) * 86400 + float(hours) * 3600 + float(minutes) * 60 + float(seconds)


def daemon_cpu(pid):
    result = subprocess.check_output(["ps", "-p", str(pid), "-o", "time="],
                                     text=True, timeout=5, env={"LC_ALL": "C"})
    return parse_cpu_time(result)


def state_snapshot(home):
    files = {}
    for directory, dirs, names in os.walk(home):
        directory = Path(directory)
        if directory == home and "logs" in dirs:
            dirs.remove("logs")  # Count log volume separately, not appends as state rewrites.
        for name in names:
            path = directory / name
            try:
                info = path.lstat()
            except FileNotFoundError:
                continue  # A state writer can rename a temp file between walk and lstat.
            if stat.S_ISREG(info.st_mode):
                files[path.relative_to(home).as_posix()] = (info.st_ino, info.st_mtime_ns, info.st_size)
    return files


def rewritten_paths(before, after):
    return sorted(path for path, state in after.items() if before.get(path) != state)


def log_snapshot(paths):
    files = {}
    for path in paths:
        info = path.stat()
        files[path] = (info.st_dev, info.st_ino, info.st_size)
    return files


@dataclass
class LogStats:
    accepts: int = 0
    bytes: int = 0
    templates: Counter = field(default_factory=Counter)


def log_delta(start, end):
    stats = LogStats()
    if missing := start.keys() - end.keys():
        raise RuntimeError(f"log disappeared during measurement: {min(missing)}")
    for path, (device, inode, size) in end.items():
        old_device, old_inode, offset = start.get(path, (device, inode, 0))
        if (device, inode) != (old_device, old_inode) or size < offset:
            raise RuntimeError(f"log replaced or truncated during measurement: {path}")
        with path.open("rb") as stream:
            stream.seek(offset)
            while remaining := size - stream.tell():
                line = stream.readline(remaining)
                if not line:
                    raise RuntimeError(f"log truncated while reading: {path}")
                template = re.sub(rb"^\[\d{4}-\d{2}-\d{2} [^\]]+\] ", b"", line.rstrip()).decode(errors="replace")
                if template == f"[INFO] {ACCEPT}":
                    stats.accepts += 1
                # Fold session IDs, durations and sizes to show the repeated offender.
                template = re.sub(r"\bsession_\w+_\d{6,}[\w-]*", "<session>", template)
                template = re.sub(r"\b\d+(?:\.\d+)?", "#", template)
                stats.templates[template[:160]] += len(line)
                stats.bytes += len(line)
    return stats


@dataclass
class Row:
    name: str
    unit: str
    actual: float
    limit: float
    rule: str = ""

    @property
    def exceeded(self):
        return self.actual > self.limit


def load_budget(path):
    budget = json.loads(path.read_text())
    try:
        limits = [budget[key] for key in ("accepted_connections_per_minute", "log_bytes_per_minute",
                                         "daemon_cpu_seconds_per_minute", "unlisted_path_rewrites_per_minute")]
        limits.extend(budget["rewrites_per_minute"].values())
        valid = all(type(limit) in (int, float) and math.isfinite(limit) and limit >= 0 for limit in limits)
    except (KeyError, TypeError, AttributeError) as error:
        raise ValueError(f"invalid idle budget {path}: {error}") from error
    if not valid:
        raise ValueError(f"invalid idle budget {path}: limits must be finite and nonnegative")
    return budget


def budget_rows(budget, elapsed, accepts, log_bytes, cpu_seconds, rewrites):
    rate = 60 / elapsed
    rows = [
        Row(ACCEPT, "accepts/min", accepts * rate, budget["accepted_connections_per_minute"]),
        Row("logs/ + daemon stdout/stderr", "bytes/min", log_bytes * rate, budget["log_bytes_per_minute"]),
        Row("daemon", "CPU s/min", cpu_seconds * rate, budget["daemon_cpu_seconds_per_minute"]),
    ]
    patterns = budget["rewrites_per_minute"]
    for path, count in sorted(rewrites.items()):
        rule = next((pattern for pattern in patterns if fnmatchcase(path, pattern)), None)
        limit = patterns[rule] if rule is not None else budget["unlisted_path_rewrites_per_minute"]
        rows.append(Row(path, "rewrites/min", count * rate, limit, rule or "unlisted"))
    return rows


class ApiClient:
    def __init__(self, peer):
        self.peer = peer
        self.buffer = bytearray()
        self.request_id = 0

    def receive(self, until):
        while (now := time.monotonic()) < until:
            if (end := self.buffer.find(b"\n")) >= 0:
                line = self.buffer[:end]
                del self.buffer[:end + 1]
                return json.loads(line)
            self.peer.settimeout(until - now)
            try:
                chunk = self.peer.recv(65536)
            except socket.timeout:
                return None
            if not chunk:
                raise RuntimeError("API bridge closed the idle client connection")
            self.buffer.extend(chunk)
        return None

    def request(self, kind, expected, **fields):
        self.request_id += 1
        self.peer.settimeout(API_TIMEOUT)
        request = {"v": 1, "id": self.request_id, "req": kind, **fields}
        self.peer.sendall(json.dumps(request).encode() + b"\n")
        until = time.monotonic() + API_TIMEOUT
        while (frame := self.receive(until)) is not None:
            if frame.get("reply_to") == self.request_id:
                if frame.get("ev") != expected:
                    raise RuntimeError(f"{kind}: expected {expected!r}, received {frame}")
                return frame
        raise RuntimeError(f"API bridge did not reply to {kind!r}")

    def drain(self, until):
        while self.receive(until) is not None:
            pass


class Catalog(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        if self.path != "/v1/models":
            self.send_error(404)
            return
        body = b'{"data":[{"id":"idle-fixture","object":"model"}]}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@contextmanager
def local_catalog():
    with ThreadingHTTPServer(("127.0.0.1", 0), Catalog) as server:
        thread = Thread(target=server.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True)
        thread.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}/v1"
        finally:
            server.shutdown()
            thread.join()


@contextmanager
def child_process(command, root, env, console):
    with console.open("wb") as output:
        process = subprocess.Popen(command, cwd=root, env=env, stdin=subprocess.DEVNULL,
                                   stdout=output, stderr=output, start_new_session=True)
        try:
            yield process
        finally:
            if process.poll() is None:
                signal_child_group(process, signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    signal_child_group(process, signal.SIGKILL)
                    process.wait(timeout=5)


def signal_child_group(process, sig):
    try:
        os.killpg(process.pid, sig)
    except ProcessLookupError:
        pass  # The private child group can exit between poll and killpg.


def wait_for_socket(path, process):
    deadline = time.monotonic() + START_TIMEOUT
    while not path.is_socket():
        if process.poll() is not None or time.monotonic() >= deadline:
            raise RuntimeError(f"socket {path.name} did not appear (child exit: {process.poll()})")
        time.sleep(0.05)


def measure(client, daemon, home, console, session, seconds):
    def logs():
        return log_snapshot([console, *home.glob("logs/*.log")])

    def list_sessions():
        frame = client.request("list_sessions", "sessions", limit=100)
        if session not in {row.get("session_id") for row in frame["sessions"]}:
            raise RuntimeError("idle fixture session is missing from list_sessions")

    client.request("watch_background_tasks", "background_tasks", session_id=session)
    list_sessions()
    client.drain(time.monotonic() + LIST_INTERVAL)  # Finish attachment/startup work outside the window.
    log_start = logs()
    if not log_delta({}, log_start).accepts:
        raise RuntimeError(f"startup opened daemon connections but no {ACCEPT!r} marker was logged")
    state = state_snapshot(home)
    cpu_start = daemon_cpu(daemon.pid)
    start = time.monotonic()
    end = start + seconds
    next_list = start
    next_sample = start + SAMPLE_INTERVAL
    rewrites = Counter()
    polls = 0
    while (now := time.monotonic()) < end:
        if now >= next_sample:
            current = state_snapshot(home)
            rewrites.update(rewritten_paths(state, current))
            state = current
            next_sample = now + SAMPLE_INTERVAL
        if now >= next_list:
            list_sessions()
            polls += 1
            next_list = now + LIST_INTERVAL
        client.drain(min(next_list, next_sample, end))
    finished = time.monotonic()
    if polls < max(1, math.floor(seconds / LIST_INTERVAL)):
        raise RuntimeError(f"only {polls} list_sessions replies, below the requested idle polling cadence")
    rewrites.update(rewritten_paths(state, state_snapshot(home)))
    log_end = logs()
    cpu_seconds = daemon_cpu(daemon.pid) - cpu_start
    return finished - start, log_delta(log_start, log_end), cpu_seconds, rewrites, polls


def run(binary, root, seconds):
    home, runtime = root / "jcode", root / "run"
    for directory in (home, runtime, root / "os-home"):
        directory.mkdir()
    # Match a running-but-not-due ambient scheduler without generating any model work.
    (home / "config.toml").write_text("[ambient]\nenabled = true\npause_on_active_session = false\n")
    (home / "ambient").mkdir()
    next_wake = datetime.now(timezone.utc) + timedelta(seconds=seconds + 3600)
    (home / "ambient/state.json").write_text(json.dumps({
        "status": {"Scheduled": {"next_wake": next_wake.isoformat()}}, "total_cycles": 0,
    }))
    (home / "safety").mkdir()
    (home / "safety/queue.json").write_text("[]\n")
    daemon_socket, api_socket = runtime / "d.sock", runtime / "a.sock"
    daemon_console, bridge_console = root / "daemon.stdout", root / "bridge.stdout"
    command = [str(binary), "--no-update", "--no-selfdev", "--socket", str(daemon_socket)]
    with local_catalog() as base:
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(root / "os-home"),
            "TMPDIR": str(root), "XDG_RUNTIME_DIR": str(runtime),
            "JCODE_HOME": str(home), "JCODE_RUNTIME_DIR": str(runtime), "JCODE_TEST_SESSION": "1",
            "JCODE_TEMP_SERVER": "1", "JCODE_SERVER_OWNER_PID": str(os.getpid()),
            "JCODE_NO_TELEMETRY": "1", "JCODE_NON_INTERACTIVE": "1",
            "JCODE_PROVIDER": "openai-compatible", "JCODE_MODEL": "idle-fixture",
            "JCODE_OPENAI_COMPAT_DEFAULT_MODEL": "idle-fixture",
            "JCODE_OPENAI_COMPAT_API_BASE": base, "OPENAI_COMPAT_API_KEY": "fixture-only",
        }
        try:
            with child_process([*command, "serve"], root, env, daemon_console) as daemon:
                wait_for_socket(daemon_socket, daemon)
                with child_process([*command, "api-bridge", "--api-socket", str(api_socket)],
                                   root, env, bridge_console) as bridge:
                    wait_for_socket(api_socket, bridge)
                    with socket.socket(socket.AF_UNIX) as peer:
                        peer.settimeout(API_TIMEOUT)
                        peer.connect(str(api_socket))
                        client = ApiClient(peer)
                        client.request("hello", "hello_ok", min_version=1, max_version=1,
                                       client_name="jcode-desktop-idle-budget")
                        created = client.request("create_session", "attached", working_dir=str(root))
                        session = created["session"]["session_id"]
                        client.request("rename_session", "ok", title="Idle load fixture")
                        attached = client.request("attach_session", "attached", session_id=session)
                        if attached["session"]["session_id"] != session:
                            raise RuntimeError("API bridge attached a different session")
                        return measure(client, daemon, home, daemon_console, session, seconds)
        except Exception:
            for path in sorted(home.glob("logs/*.log")) + [daemon_console, bridge_console]:
                if path.is_file():
                    with path.open("rb") as stream:
                        stream.seek(max(0, path.stat().st_size - 2048))
                        text = stream.read().decode(errors="replace").strip()
                    if text:
                        print(f"{path.name} (tail):\n{text}", file=sys.stderr)
            raise


def show_report(rows, elapsed, logs, polls):
    print(f"Idle window: {elapsed:.2f}s, {polls} list_sessions replies; one attached API client with task watch.")
    if not any(row.unit == "rewrites/min" for row in rows):
        print("No state-file rewrites observed.")
    print(f"{'Status':<7} {'Observed':>12} {'Budget':>12} {'Unit':<16} Path or template (rule)")
    for row in rows:
        rule = f" ({row.rule})" if row.rule and row.rule != row.name else ""
        print(f"{'FAIL' if row.exceeded else 'OK':<7} {row.actual:12.2f} {row.limit:12.2f} "
              f"{row.unit:<16} {row.name}{rule}")
    if logs.templates:
        print("Largest log templates (bytes/min):")
        for template, size in logs.templates.most_common(8):
            print(f"  {size * 60 / elapsed:10.1f}  {template}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("binary", type=Path, help="built jcode binary (not a launcher)")
    parser.add_argument("--seconds", type=float, default=60, help="timed idle interval (default: 60)")
    parser.add_argument("--budget", type=Path, default=BUDGET_FILE, help="override budget JSON for verification")
    args = parser.parse_args()
    if not math.isfinite(args.seconds) or args.seconds <= 0:
        parser.error("--seconds must be positive and finite")
    if sys.platform not in ("darwin", "linux"):
        parser.error("the gate requires macOS or Linux")
    try:
        budget = load_budget(args.budget)
        # A short root avoids AF_UNIX path length failures with macOS TMPDIR.
        with tempfile.TemporaryDirectory(prefix="jc-idle-", dir="/tmp") as directory:
            elapsed, logs, cpu, rewrites, polls = run(args.binary.resolve(strict=True), Path(directory), args.seconds)
        rows = budget_rows(budget, elapsed, logs.accepts, logs.bytes, cpu, rewrites)
        show_report(rows, elapsed, logs, polls)
        return int(any(row.exceeded for row in rows))
    except (OSError, RuntimeError, ValueError, OverflowError, KeyError, subprocess.SubprocessError) as error:
        print(f"Idle load check could not measure the workload: {error}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        return 130


def interrupt(_signum, _frame):
    raise KeyboardInterrupt


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupt)
    sys.exit(main())
