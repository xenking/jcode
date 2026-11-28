#!/usr/bin/env python3
"""Isolated acceptance for typed MCP results and per-call approval in codemode.

Usage: scripts/accept-mcp-typed.py <jcode-binary> [--tui] [--keep DIR]

Every run gets its own JCODE_HOME, runtime dir and socket. A scripted
OpenAI-compatible provider makes the model turns, so no account or network is
needed. Fixture MCP servers (stdio, HTTP and a 150-tool catalog) append a line
to a log whenever a write really runs, so "refused" and "denied" are proved by
the log, not by an error message alone.

Headless `jcode run` cases check:
  * outputSchema becomes a `Promise<...Output>` declaration and the script gets
    structuredContent, over stdio and over HTTP
  * `approval = "annotations"` lets a read-only tool run and refuses a write
    when nobody can be asked, with no side effect
  * `approve_free` exempts exactly the named tool
  * a 150-tool catalog adds no tool to the provider's tool list, yet every tool
    is reachable and typed from a script

Daemon cases drive the real protocol as a client would:
  * a gated write asks once per call with the tool and arguments
  * a background request while the prompt is open is refused (moved = false)
  * Allow once runs that one call, Deny and a free-text reply do not
  * the finished codemode row carries one receipt per call, including the
    denied one
  * "Always allow this tool" is saved to mcp.json, the next call in the same
    daemon run is not asked, `jcode mcp approvals` lists it, and revoking it
    makes the daemon ask again

`--tui` adds the terminal path: a real TUI in a private tmux server answers the
same prompts with the keyboard and must show one row per nested call.
"""
import argparse
import json
import os
import pathlib
import re
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

OBJ = {"type": "object", "properties": {"n": {"type": "integer"}}}

STDIO_SERVER = r'''
import json, os, sys
LOG = os.environ["WRITE_LOG"]
OBJ = {"type": "object", "properties": {"n": {"type": "integer"}}}
TOOLS = [
    {"name": "read-item", "description": "Read an item", "inputSchema": OBJ,
     "outputSchema": {"type": "object", "required": ["value"],
                      "properties": {"value": {"type": "string"}, "via": {"type": "string"}}},
     "annotations": {"readOnlyHint": True}},
    {"name": "write-item", "description": "Write an item", "inputSchema": OBJ,
     "annotations": {"readOnlyHint": False, "destructiveHint": True}},
    {"name": "bare", "description": "Writes and declares nothing", "inputSchema": OBJ},
]
for line in sys.stdin:
    req = json.loads(line)
    method = req.get("method")
    if method == "shutdown":
        break
    if "id" not in req:
        continue
    reply = {"jsonrpc": "2.0", "id": req["id"]}
    if method == "initialize":
        reply["result"] = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                           "serverInfo": {"name": "fixture", "version": "1"}}
    elif method == "tools/list":
        reply["result"] = {"tools": TOOLS}
    elif method == "tools/call":
        p = req["params"]; name = p["name"]; args = p.get("arguments") or {}
        if name == "read-item":
            value = "v%s" % args.get("n", 0)
            reply["result"] = {"content": [{"type": "text", "text": json.dumps({"value": value, "via": "text"})}],
                               "structuredContent": {"value": value, "via": "structured"}, "isError": False}
        else:
            with open(LOG, "a") as f:
                f.write("%s:%s\n" % (name, json.dumps(args, sort_keys=True)))
            reply["result"] = {"content": [{"type": "text", "text": json.dumps({"wrote": name})}], "isError": False}
    else:
        reply["error"] = {"code": -32601, "message": "no"}
    print(json.dumps(reply), flush=True)
'''

BIG_SERVER = r'''
import json, os, sys
TOOLS = []
for i in range(150):
    if i < 40:
        TOOLS.append({"name": "read_%03d" % i, "description": "Read record kind %d." % i,
                      "inputSchema": {"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}},
                      "outputSchema": {"type": "object", "required": ["title"],
                                       "properties": {"title": {"type": "string"}, "via": {"type": "string"}}},
                      "annotations": {"readOnlyHint": True}})
    else:
        TOOLS.append({"name": "write_%03d" % i, "description": "Change record kind %d." % i,
                      "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}},
                      "annotations": {"readOnlyHint": False}})
for line in sys.stdin:
    req = json.loads(line); method = req.get("method")
    if method == "shutdown":
        break
    if "id" not in req:
        continue
    reply = {"jsonrpc": "2.0", "id": req["id"]}
    if method == "initialize":
        reply["result"] = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                           "serverInfo": {"name": "big", "version": "1"}}
    elif method == "tools/list":
        reply["result"] = {"tools": TOOLS}
    elif method == "tools/call":
        args = req["params"].get("arguments") or {}
        reply["result"] = {"content": [{"type": "text", "text": json.dumps({"title": "T", "via": "text"})}],
                           "structuredContent": {"title": "T-" + args.get("id", "?"), "via": "structured"}, "isError": False}
    else:
        reply["error"] = {"code": -32601, "message": "no"}
    print(json.dumps(reply), flush=True)
'''

SCRIPT_STDIO = """
const decl = types('mcp__fixture__read_item');
const typed = await tools.mcp__fixture__read_item({ n: 7 });
let write = 'ran';
try { await tools.mcp__fixture__write_item({ n: 1 }); } catch (e) { write = String(e.message).slice(0, 200); }
let bare = 'ran';
try { await tools.mcp__fixture__bare({}); } catch (e) { bare = String(e.message).slice(0, 200); }
return { declaresOutput: decl.includes('Promise<McpFixtureReadItemOutput>'), via: typed.via, value: typed.value, write, bare };
"""

SCRIPT_HTTP = """
const typed = await tools.mcp__httpfx__read_item({ n: 1 });
let write = 'ran';
try { await tools.mcp__httpfx__write_item({ n: 2 }); } catch (e) { write = String(e.message).slice(0, 200); }
return { via: typed.via, declaresOutput: types('read_item').includes('Promise<McpHttpfxReadItemOutput>'), write };
"""

SCRIPT_BIG = """
const listed = listTools('mcp__big').length;
const typed = await tools.mcp__big__read_007({ id: 'x1' });
return { listed, declaresOutput: types('read_007').includes('Promise<McpBigRead007Output>'), via: typed.via, title: typed.title };
"""

SCRIPT_APPROVAL = """
const typed = await tools.mcp__fixture__read_item({ n: 7 });
const out = { value: typed.value };
for (const n of [1, 2, 3]) {
  try { await tools.mcp__fixture__write_item({ n }); out['write' + n] = 'ran'; }
  catch (e) { out['write' + n] = String(e.message).slice(0, 70); }
}
return out;
"""


class Provider(BaseHTTPRequestHandler):
    """First turn calls codemode with `script`, the next echoes the tool result."""

    script = ""
    requests = []

    def log_message(self, *args):
        pass

    def reply(self, value):
        data = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.reply({"data": [{"id": "fake-model", "object": "model"}]})

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["content-length"])))
        messages = request.get("messages", [])
        tools = [t.get("function", {}).get("name") for t in request.get("tools", [])]
        type(self).requests.append({"tools": tools, "bytes": len(json.dumps(request))})
        last = messages[-1] if messages else {}
        if request.get("tools") and last.get("role") != "tool":
            call = {"index": 0, "id": "call-cm", "type": "function", "function": {
                "name": "codemode", "arguments": json.dumps({"intent": "verify typed mcp", "code": self.script})}}
            delta, finish = {"role": "assistant", "tool_calls": [call]}, "tool_calls"
        else:
            content = last.get("content") or ""
            if isinstance(content, list):
                content = "\n".join(p.get("text", "") for p in content if isinstance(p, dict))
            delta, finish = {"role": "assistant", "content": "RESULT: " + content[:4000]}, "stop"
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        for choice in [{"index": 0, "delta": delta}, {"index": 0, "delta": {}, "finish_reason": finish}]:
            chunk = {"id": "fake", "object": "chat.completion.chunk", "model": "fake-model", "choices": [choice]}
            self.wfile.write(b"data: " + json.dumps(chunk).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def start_provider(script):
    handler = type("ScriptedProvider", (Provider,), {"script": script, "requests": []})
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, handler


class HttpMcp(BaseHTTPRequestHandler):
    log = None

    def log_message(self, *args):
        pass

    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        rid = req.get("id")
        if rid is None:
            self.send_response(202)
            self.send_header("content-length", "0")
            self.end_headers()
            return
        reply = {"jsonrpc": "2.0", "id": rid}
        method = req.get("method")
        if method == "initialize":
            reply["result"] = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                               "serverInfo": {"name": "httpfx", "version": "1"}}
        elif method == "tools/list":
            reply["result"] = {"tools": [
                {"name": "read-item", "description": "Read", "inputSchema": OBJ,
                 "annotations": {"readOnlyHint": True},
                 "outputSchema": {"type": "object", "required": ["value"],
                                  "properties": {"value": {"type": "string"}, "via": {"type": "string"}}}},
                {"name": "write-item", "description": "Write", "inputSchema": OBJ,
                 "annotations": {"readOnlyHint": False}}]}
        elif method == "tools/call":
            if req["params"]["name"] == "read-item":
                reply["result"] = {"content": [{"type": "text", "text": '{"value":"h","via":"text"}'}],
                                   "structuredContent": {"value": "h", "via": "structured"}}
            else:
                with open(self.log, "a") as f:
                    f.write("write\n")
                reply["result"] = {"content": [{"type": "text", "text": "ok"}]}
        else:
            reply["error"] = {"code": -32601, "message": "no"}
        data = json.dumps(reply).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class Run:
    """One private JCODE_HOME, runtime dir and socket."""

    def __init__(self, binary, root, name, servers, script):
        self.binary = str(pathlib.Path(binary).resolve())
        self.root = root / name
        self.name = name
        for part in ("home", "run", "jcode", "work"):
            (self.root / part).mkdir(parents=True)
        self.writes = self.root / "writes.log"
        self.provider, self.handler = start_provider(script)
        self.sock = str(self.root / "run" / "j.sock")
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(self.root / "home"), "USER": "test",
            "TERM": "xterm-256color", "TMPDIR": str(self.root / "run"), "XDG_RUNTIME_DIR": str(self.root / "run"),
            "JCODE_HOME": str(self.root / "jcode"), "JCODE_RUNTIME_DIR": str(self.root / "run"),
            "JCODE_SOCKET": self.sock, "JCODE_TEST_SESSION": "1", "JCODE_NO_MENUBAR": "1",
            "JCODE_DISABLE_CLAUDE_MCP": "1", "WRITE_LOG": str(self.writes),
            "JCODE_MCP_APPROVAL_ATTACH_WAIT_SECS": "0",
            "JCODE_OPENAI_COMPAT_API_BASE": f"http://127.0.0.1:{self.provider.server_port}/v1",
            "OPENAI_COMPAT_API_KEY": "fixture-only", "JCODE_OPENAI_COMPAT_DEFAULT_MODEL": "fake-model",
            "JCODE_NO_TELEMETRY": "1", "JCODE_TELEMETRY": "off", "JCODE_NO_AUTO_UPDATE": "1",
        }
        (self.root / "jcode" / "mcp.json").write_text(json.dumps({"servers": servers}))
        self.base = [self.binary, "--no-update", "--no-selfdev", "--provider", "openai-compatible", "--model", "fake-model"]
        self.procs = []

    def cli(self, *args, timeout=90):
        result = subprocess.run(self.base + list(args), env=self.env, cwd=self.root / "work",
                                capture_output=True, text=True, timeout=timeout)
        return result

    def run_prompt(self):
        result = self.cli("--socket", self.sock, "run", "go")
        assert result.returncode == 0, (result.stdout, result.stderr)
        match = re.search(r"result:\s*(\{.*?\})\s*\n\s*(?:calls:|\[\d+ tool calls?)", result.stdout, re.S)
        assert match, f"no script result in output:\n{result.stdout}"
        return json.loads(match.group(1)), result.stdout

    def write_lines(self):
        return self.writes.read_text().splitlines() if self.writes.exists() else []

    def spawn(self, *args):
        proc = subprocess.Popen(self.base + list(args), env=self.env, cwd=self.root / "work",
                                stdout=open(self.root / "daemon.log", "ab"), stderr=subprocess.STDOUT)
        self.procs.append(proc)
        return proc

    def stop(self):
        for proc in self.procs:
            proc.terminate()
        for proc in self.procs:
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
        self.provider.shutdown()


class Client:
    def __init__(self, path):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(30)
        self.socket.connect(path)
        self.reader = self.socket.makefile("rb")
        self.seen = []

    def send(self, **request):
        self.socket.sendall(json.dumps(request).encode() + b"\n")

    def until(self, predicate, what):
        while True:
            line = self.reader.readline()
            if not line:
                raise AssertionError(f"daemon disconnected while waiting for {what}")
            event = json.loads(line)
            self.seen.append(event)
            if predicate(event):
                return event

    def close(self):
        self.reader.close()
        self.socket.close()


def check(condition, message, evidence=None):
    if not condition:
        raise AssertionError(f"{message}\n{evidence if evidence is not None else ''}")
    print(f"  ok  {message}")


def stdio_server(root):
    path = root / "mcp_fixture.py"
    path.write_text(STDIO_SERVER)
    return {"command": "python3", "args": ["-I", "-S", "-u", str(path)], "shared": False, "timeout_secs": 20}


def case_stdio_typed_and_gated(binary, root):
    print("stdio server: typed result, read free, write refused headless")
    run = Run(binary, root, "stdio", {"fixture": {**stdio_server(root), "env": {"WRITE_LOG": str(root / "stdio" / "writes.log")},
                                                   "approval": "annotations"}}, SCRIPT_STDIO)
    try:
        listing = run.cli("mcp", "test", "fixture").stdout
        check("read-item  (read-only, typed result)" in listing, "mcp test marks the typed read-only tool", listing)
        check("2 of 3 tools ask a human first" in listing, "mcp test counts what will ask", listing)
        result, _ = run.run_prompt()
        check(result["declaresOutput"], "outputSchema is declared as Promise<McpFixtureReadItemOutput>", result)
        check(result["via"] == "structured" and result["value"] == "v7", "script receives structuredContent", result)
        check("no attached client" in result["write"], "annotated write is refused with no one to ask", result)
        check("no attached client" in result["bare"], "unannotated tool counts as a write", result)
        check(run.write_lines() == [], "no write reached the server", run.write_lines())
    finally:
        run.stop()


def case_approve_free(binary, root):
    print("approve_free: exempts exactly the named tool")
    run = Run(binary, root, "free", {"fixture": {**stdio_server(root), "env": {"WRITE_LOG": str(root / "free" / "writes.log")},
                                                  "approval": "annotations", "approve_free": ["write-item"]}}, SCRIPT_STDIO)
    try:
        result, _ = run.run_prompt()
        check(result["write"] == "ran", "the listed write runs unattended", result)
        check("no attached client" in result["bare"], "an unlisted write is still refused", result)
        check(run.write_lines() == ['write-item:{"n": 1}'], "exactly one write reached the server", run.write_lines())
    finally:
        run.stop()


def case_http_typed(binary, root):
    print("http server: typed result and the same gate")
    log = root / "http-writes.log"
    HttpMcp.log = str(log)
    server = ThreadingHTTPServer(("127.0.0.1", 0), HttpMcp)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    run = Run(binary, root, "http", {"httpfx": {"type": "http", "url": f"http://127.0.0.1:{server.server_port}/mcp",
                                                 "approval": "annotations"}}, SCRIPT_HTTP)
    try:
        result, _ = run.run_prompt()
        check(result["declaresOutput"] and result["via"] == "structured", "http result is typed and structured", result)
        check("no attached client" in result["write"], "http write is refused headless", result)
        check(not log.exists(), "no http write reached the server")
    finally:
        run.stop()
        server.shutdown()


def case_deferred_catalog(binary, root):
    print("150-tool catalog: nothing added to the tool list, all reachable")
    big = root / "mcp_big.py"
    big.write_text(BIG_SERVER)
    servers = {"big": {"command": "python3", "args": ["-I", "-S", "-u", str(big)], "shared": False,
                       "timeout_secs": 20, "approval": "annotations"}}
    with_big = Run(binary, root, "big", servers, SCRIPT_BIG)
    without = Run(binary, root, "nobig", {}, "return { ok: true };")
    try:
        result, _ = with_big.run_prompt()
        without.run_prompt()
        check(result["listed"] == 150, "listTools sees all 150 tools", result)
        check(result["declaresOutput"] and result["via"] == "structured", "a deferred tool is typed and callable", result)
        first = with_big.handler.requests[0]
        baseline = without.handler.requests[0]
        check(not any((name or "").startswith("mcp__") for name in first["tools"]), "no MCP tool is in the provider's tool list")
        check(first["tools"] == baseline["tools"], "tool list equals the run without the server")
        catalog = {}
        exec(BIG_SERVER.split("for line in sys.stdin")[0], catalog)
        schemas = sum(len(json.dumps(tool)) for tool in catalog["TOOLS"])
        delta = first["bytes"] - baseline["bytes"]
        check(delta < schemas / 4, f"first request grows by {delta} bytes, the schemas alone are {schemas}")
    finally:
        with_big.stop()
        without.stop()


def approval_servers(root, name):
    return {"fixture": {**stdio_server(root), "env": {"WRITE_LOG": str(root / name / "writes.log")}, "approval": "annotations"}}


def daemon_session(run):
    run.spawn("serve", "--socket", run.sock)
    for _ in range(100):
        if os.path.exists(run.sock):
            break
        time.sleep(0.1)
    client = Client(run.sock)
    client.send(type="subscribe", id=1, working_dir=str(run.root / "work"), continue_on_disconnect=True)
    session = client.until(lambda e: e.get("type") == "session", "session")["session_id"]
    client.until(lambda e: e.get("type") == "done" and e.get("id") == 1, "subscribe done")
    return client, session


def case_daemon_approval(binary, root):
    print("daemon: ask once per call, background refused, allow / deny / free text")
    run = Run(binary, root, "approval", approval_servers(root, "approval"), SCRIPT_APPROVAL)
    try:
        client, _ = daemon_session(run)
        client.send(type="message", id=2, content="call the mcp tools")
        answers = ["Allow once", "Deny", None]
        for index, label in enumerate(answers, start=1):
            request = client.until(lambda e: e.get("type") == "ask_user_request", f"prompt {index}")
            question = request["questions"][0]
            check(request["request_id"].startswith("ask-mcp-approval-"), f"prompt {index} is an MCP approval request")
            check(question.get("header") == "MCP approval" and f'"write-item"' in question["question"]
                  and f'"n":{index}' in question["question"], f"prompt {index} names the tool and arguments", question["question"])
            if index == 1:
                client.send(type="background_tool", id=50)
                moved = client.until(lambda e: e.get("type") == "background_tool_result" and e.get("id") == 50, "background result")
                check(moved["moved"] is False, "a background request is refused while the prompt is open", moved)
            response = ({"answers": [{"selected": [label]}]} if label
                        else {"answers": [{"selected": [], "other": "sure, go ahead"}]})
            client.send(type="ask_user_response", id=100 + index, request_id=request["request_id"], response=response)
        done = client.until(lambda e: e.get("type") == "tool_done" and e.get("name") == "codemode", "codemode result")
        check("moved to background" not in done["output"], "the run stayed in the foreground", done["output"][:200])
        calls = done.get("codemode_calls") or []
        check([c["tool"] for c in calls] == ["mcp__fixture__read_item"] + ["mcp__fixture__write_item"] * 3,
              "the receipt lists one row per call, refused ones included", calls)
        check([c["status"] for c in calls] == ["success", "success", "error", "error"], "statuses: read, allowed, denied, free text", calls)
        check(calls[2].get("diagnostic") == "Denied by the user" and calls[3].get("diagnostic") == "Denied by the user",
              "denied calls carry a safe category, not the raw message", calls)
        check(run.write_lines() == ['write-item:{"n": 1}'], "only the allowed call reached the server", run.write_lines())
        client.close()
    finally:
        run.stop()


def case_daemon_always_allow(binary, root):
    print("daemon: always allow saves to mcp.json, applies at once, shows in `mcp approvals`, revoke asks again")
    run = Run(binary, root, "always", approval_servers(root, "always"), SCRIPT_APPROVAL)
    try:
        client, _ = daemon_session(run)
        client.send(type="message", id=2, content="call the mcp tools")
        request = client.until(lambda e: e.get("type") == "ask_user_request", "first prompt")
        labels = [o["label"] for o in request["questions"][0]["options"]]
        check(labels == ["Deny", "Allow once", "Always allow this tool", "Always allow every tool on this server"],
              "the card offers the four options, one line each", labels)
        client.send(type="ask_user_response", id=101, request_id=request["request_id"],
                    response={"answers": [{"selected": ["Always allow this tool"]}]})
        done = client.until(lambda e: e.get("type") == "tool_done" and e.get("name") == "codemode", "codemode result")
        asked = [e for e in client.seen if e.get("type") == "ask_user_request"]
        check(len(asked) == 1, "writes 2 and 3 of the same run were not asked", len(asked))
        check(run.write_lines() == ['write-item:{"n": 1}', 'write-item:{"n": 2}', 'write-item:{"n": 3}'],
              "all three writes reached the server", run.write_lines())
        saved = json.loads((run.root / "jcode" / "mcp.json").read_text())
        check(saved["servers"]["fixture"]["approve_free"] == ["write-item"], "approve_free saved in mcp.json", saved)
        listing = run.cli("mcp", "approvals", "--json")
        rows = {row["name"]: row for row in json.loads(listing.stdout)}
        check(rows["fixture"]["approve_free"] == ["write-item"] and rows["fixture"]["approval"] == "annotations",
              "`jcode mcp approvals --json` lists it", listing.stdout)
        text = run.cli("mcp", "approvals").stdout
        check("always allowed: write-item" in text, "`jcode mcp approvals` lists it", text)
        revoked = run.cli("mcp", "approvals", "revoke", "fixture", "write-item")
        check(revoked.returncode == 0 and "mcp.json" in revoked.stdout, "revoke succeeds", (revoked.stdout, revoked.stderr))
        client.send(type="message", id=3, content="call the mcp tools again")
        request = client.until(lambda e: e.get("type") == "ask_user_request" and e is not asked[0], "prompt after revoke")
        check('"write-item"' in request["questions"][0]["question"], "revoke makes the same daemon ask again")
        denied = 0
        while True:
            client.send(type="ask_user_response", id=102 + denied, request_id=request["request_id"],
                        response={"answers": [{"selected": ["Deny"]}]})
            denied += 1
            event = client.until(lambda e: e is not done and (e.get("type") == "ask_user_request" or (
                e.get("type") == "tool_done" and e.get("name") == "codemode")), "next prompt or result")
            if event.get("type") == "tool_done":
                break
            request = event
        check(denied == 3, "each of the three writes asked again after the revoke", denied)
        check(len(run.write_lines()) == 3, "no write ran after the revoke", run.write_lines())
        logs = "".join(path.read_text(errors="replace") for path in (run.root / "jcode" / "logs").glob("*.log"))
        for outcome in ("asked", "allowed_always_tool", "denied"):
            check(f"MCP_APPROVAL outcome={outcome} session=" in logs and "server=fixture tool=write-item" in logs,
                  f"daemon log names session, server and tool ({outcome})")
        client.close()
    finally:
        run.stop()


def tmux(run, *args):
    return subprocess.run(["tmux", "-S", str(run.root / "tmux.sock"), *args], capture_output=True, text=True).stdout


def wait_for(fn, what, timeout=45, show=None):
    deadline = time.time() + timeout
    while time.time() < deadline:
        value = fn()
        if value:
            return value
        time.sleep(0.4)
    raise AssertionError(f"timed out waiting for {what}\n{show() if show else ''}")


def case_tui(binary, root, evidence):
    print("tui: keyboard approval, one row per nested call")
    run = Run(binary, root, "tui", approval_servers(root, "tui"), SCRIPT_APPROVAL)
    try:
        run.spawn("serve", "--socket", run.sock)
        wait_for(lambda: os.path.exists(run.sock), "daemon socket", 15)
        keep = ("HOME", "OPENAI_COMPAT_API_KEY", "WRITE_LOG", "PATH")
        env = [f"{k}={v}" for k, v in run.env.items() if k.startswith("JCODE") or k in keep]
        client_cmd = shlex.join(["env", *env, *run.base, "--fresh-spawn", "--socket", run.sock])
        subprocess.run(["tmux", "-S", str(run.root / "tmux.sock"), "new-session", "-d", "-x", "130", "-y", "44",
                        "-c", str(run.root / "work"), client_cmd], check=True)

        def screen():
            return tmux(run, "capture-pane", "-p", "-t", "0")

        if wait_for(lambda: "Welcome to jcode" in screen() or "Ready to use" in screen(), "first screen", 30, screen) and "Welcome to jcode" in screen():
            tmux(run, "send-keys", "-t", "0", "Right", "Enter")
            time.sleep(2)
        tmux(run, "send-keys", "-t", "0", "call the mcp tools", "Enter")
        for index, keys in enumerate([("Down", "Enter"), ("Enter",), ("Enter",)], start=1):
            wait_for(lambda: "MCP approval" in screen() and f'"n":{index}' in screen(), f"prompt {index}", 45, screen)
            frame = screen()
            (evidence / f"tui-prompt-{index}.txt").write_text(frame)
            check("Deny" in frame and "Allow once" in frame, f"prompt {index} offers Deny and Allow once")
            if index == 1:
                tmux(run, "send-keys", "-t", "0", "M-b")
                time.sleep(1)
                check("MCP approval" in screen() and "RESULT" not in screen(), "Alt+B leaves the prompt open and answerable")
            tmux(run, "send-keys", "-t", "0", *keys)
        final = wait_for(lambda: screen() if "calls: #" in screen() else None, "final transcript", 45, screen)
        (evidence / "tui-final.txt").write_text(final)
        rows = [line for line in final.splitlines() if "mcp__fixture__" in line and ("✓" in line or "✗" in line)]
        check(len(rows) == 4, "one row per nested call under the codemode row", rows)
        check(sum("✗" in row for row in rows) == 2 and sum("✓" in row for row in rows) == 2, "two ran, two refused", rows)
        check("Denied by the user" in final, "the refused rows say why")
        check(run.write_lines() == ['write-item:{"n": 1}'], "only the allowed call reached the server", run.write_lines())
    finally:
        subprocess.run(["tmux", "-S", str(run.root / "tmux.sock"), "kill-server"], capture_output=True)
        run.stop()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binary")
    parser.add_argument("--tui", action="store_true", help="also drive a real TUI in a private tmux server")
    parser.add_argument("--keep", help="directory that keeps TUI captures after the run")
    args = parser.parse_args()
    if not os.access(args.binary, os.X_OK):
        sys.exit(f"not an executable: {args.binary}")
    with tempfile.TemporaryDirectory(prefix="jcode-mcp-typed-") as temp:
        root = pathlib.Path(temp).resolve()
        evidence = pathlib.Path(args.keep).resolve() if args.keep else root / "evidence"
        evidence.mkdir(parents=True, exist_ok=True)
        cases = [case_stdio_typed_and_gated, case_approve_free, case_http_typed, case_deferred_catalog, case_daemon_approval,
                 case_daemon_always_allow]
        for case in cases:
            case(args.binary, root)
        if args.tui:
            if not shutil.which("tmux"):
                sys.exit("--tui needs tmux")
            case_tui(args.binary, root, evidence)
    print("mcp typed acceptance passed")


if __name__ == "__main__":
    main()
