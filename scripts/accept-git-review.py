#!/usr/bin/env python3
"""Run typed Git review against a local fake provider and keep request/output evidence."""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import tempfile
from threading import Thread


FINDINGS = {"findings": [{"path": "tracked.txt", "range": {"start_line": 1, "end_line": 1},
                          "severity": "high", "message": "Fixture finding"}]}


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, status, body, content_type="application/json"):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.reply(200, b'{"data":[{"id":"fake-model","object":"model"}]}')

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        case = self.path.split("/")[1]
        with (self.server.evidence / "provider.jsonl").open("a") as log:
            log.write(json.dumps({"case": case, "request": request}) + "\n")
        if case == "failed":
            self.reply(400, b'{"error":{"message":"fixture provider rejected request"}}')
            return
        text = json.dumps(FINDINGS) if case == "valid" else "not JSON"
        usage = {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        if not request.get("stream"):
            self.reply(200, json.dumps({"id": "review", "model": "fake-model", "usage": usage,
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": text}}]}).encode())
            return
        chunks = [{"choices": [{"index": 0, "delta": {"role": "assistant", "content": text}}]},
                  {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": usage}]
        body = "".join(f"data: {json.dumps(dict(chunk, id='review', model='fake-model'))}\n\n"
                       for chunk in chunks) + "data: [DONE]\n\n"
        self.reply(200, body.encode(), "text/event-stream")


def exercise(binary, evidence, server, root):
    repo = root / "repo"
    private_home = root / "home"
    repo.mkdir()
    private_home.mkdir()
    env = {key: os.environ[key] for key in ("PATH", "SYSTEMROOT", "PATHEXT") if key in os.environ}
    env.update(HOME=str(private_home), USERPROFILE=str(private_home), JCODE_HOME=str(root / "jcode"),
               JCODE_RUNTIME_DIR=str(root / "runtime"), JCODE_SOCKET=str(root / "r.sock"),
               JCODE_TEST_SESSION="1", JCODE_NO_MENUBAR="1",
               JCODE_NO_TELEMETRY="1", JCODE_TELEMETRY="off", JCODE_NO_AUTO_UPDATE="1", NO_COLOR="1",
               GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=str(root / "empty-gitconfig"),
               GIT_OPTIONAL_LOCKS="0", OPENAI_COMPAT_API_KEY="fixture-only",
               HTTPS_PROXY="http://127.0.0.1:1", HTTP_PROXY="http://127.0.0.1:1",
               NO_PROXY="127.0.0.1", JCODE_OPENAI_COMPAT_DEFAULT_MODEL="fake-model")

    def git(*args):
        return subprocess.check_output(["git", "-c", "user.name=Review Fixture", "-c",
            "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false", *args],
            cwd=repo, env=env, text=True).strip()

    git("init", "-q")
    tracked = repo / "tracked.txt"
    tracked.write_text("original-probe\n")
    (repo / ".gitignore").write_text("ignored.txt\n")
    git("add", ".")
    git("commit", "-qm", "fixture")
    head = git("rev-parse", "HEAD")

    def cli(case, *target, prefix=()):
        socket_args = [] if "--ssh" in target else ["--socket", env["JCODE_SOCKET"]]
        invocation = [str(binary), "--no-update", "--no-selfdev", *socket_args,
                      "--provider", "openai-compatible", "--model", "fake-model", *prefix, "review", *target]
        result = subprocess.run(invocation, cwd=repo, timeout=45, capture_output=True, env=dict(env,
            JCODE_OPENAI_COMPAT_API_BASE=f"http://127.0.0.1:{server.server_port}/{case}/v1"))
        (evidence / f"{case}.stdout").write_bytes(result.stdout)
        (evidence / f"{case}.stderr").write_bytes(result.stderr)
        (evidence / f"{case}.json").write_text(json.dumps({"argv": invocation, "exit": result.returncode}))
        return result

    def provider_calls():
        log = evidence / "provider.jsonl"
        return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []

    clean = cli("clean", "--uncommitted")
    assert clean.returncode == 0, clean.stderr
    assert json.loads(clean.stdout) == {"target": {"kind": "uncommitted", "head": head}, "findings": []}
    unknown = cli("unknown", "--commit", "missing-ref")
    assert unknown.returncode == 1 and not unknown.stdout and b"missing-ref" in unknown.stderr, unknown
    resume = cli("resume", "--uncommitted", prefix=("--resume", "old"))
    ssh = cli("ssh", "--uncommitted", "--ssh", "example.invalid")
    for invalid in (resume, ssh):
        assert invalid.returncode == 1 and not invalid.stdout
        assert b"Git review does not support --resume or --ssh" in invalid.stderr
    assert not provider_calls()

    tracked.write_text("staged-probe\n")
    git("add", "tracked.txt")
    tracked.write_text("unstaged-probe\n")
    (repo / "new file.txt").write_text("untracked-probe\n")
    (repo / "ignored.txt").write_text("ignored-probe\n")
    index = (repo / ".git/index").read_bytes()
    status = git("status", "--porcelain")

    valid = cli("valid", "--uncommitted")
    assert valid.returncode == 0, valid.stderr
    assert json.loads(valid.stdout) == {"target": {"kind": "uncommitted", "head": head}, **FINDINGS}
    calls = provider_calls()
    assert len(calls) == 1, calls
    request = calls[0]["request"]
    assert not request.get("tools"), request
    user = next(message["content"] for message in reversed(request["messages"]) if message["role"] == "user")
    if isinstance(user, list):
        user = "".join(block.get("text", "") for block in user)
    snapshot, _ = json.JSONDecoder().raw_decode(user[user.index('{'):])
    assert snapshot["target"]["head"] == head, snapshot
    assert [patch["kind"] for patch in snapshot["patches"]] == ["staged", "unstaged", "untracked"], snapshot
    assert "+staged-probe" in snapshot["patches"][0]["diff"]
    assert "+unstaged-probe" in snapshot["patches"][1]["diff"]
    assert "+untracked-probe" in snapshot["patches"][2]["diff"]
    assert "ignored-probe" not in user

    for case, diagnostic in [("malformed", b"invalid review JSON"), ("failed", b"Git review failed")]:
        result = cli(case, "--commit", "HEAD")
        assert result.returncode == 1 and not result.stdout and diagnostic in result.stderr, result
    for call in provider_calls()[1:]:
        messages = json.dumps(call["request"]["messages"])
        assert "new file.txt" not in messages and "unstaged-probe" not in messages, call
    assert git("status", "--porcelain") == status
    assert (repo / ".git/index").read_bytes() == index
    assert tracked.read_text() == "unstaged-probe\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    args.evidence.mkdir(parents=True)
    with tempfile.TemporaryDirectory(prefix="jcode-gr-") as temp:
        with ThreadingHTTPServer(("127.0.0.1", 0), Provider) as server:
            server.evidence = args.evidence
            server_thread = Thread(target=server.serve_forever, daemon=True)
            server_thread.start()
            try:
                exercise(args.binary.resolve(), args.evidence, server, Path(temp))
            finally:
                server.shutdown()
                server_thread.join()
    print(f"PASS: isolated Git review CLI; evidence at {args.evidence}")


if __name__ == "__main__":
    main()
