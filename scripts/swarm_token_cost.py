#!/usr/bin/env python3
"""Measure actual Chat Completions wire for synthetic, read-only swarm work.

Run: uv run --no-project --with tiktoken==0.12.0 python scripts/swarm_token_cost.py /path/to/jcode
Only a loopback fake provider is used. It sends scripted outputs, not model inference.
"""

import argparse
from collections import defaultdict
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
from statistics import median
import subprocess
import tempfile
import threading

import tiktoken

MODEL = "swarm-cost-fixture"
PROMPT = "Read the synthetic swarm fixture and report the requested fields."
INTENT = "Read swarm fixture"
DONE = "Fixture verified."
ACTIONS = ("list", "status", "plan_status")
PATTERNS = {
    "list": r"(session_[\w-]+) \(you\)",
    "status": r"Lifecycle: (\w+)",
    "plan_status": r"Items: (\d+)",
}
TIMING = re.compile(r"^\[tool timing: [^\n]+?\] ?")
RECEIPT = re.compile(r"^  #(\d+) swarm (\w+) \d+ms (\d+)B", re.M)
SID = re.compile(r"session_[\w-]+")


def compact(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def without_timing(text):
    return TIMING.sub("", text, count=1)


def projected(text, action):
    if action == "summary":
        assert "requires one `to_session` ID" in text, text
        return "missing-target"
    return re.search(PATTERNS[action], text).group(1)


@dataclass
class Probe:
    shape: str
    size: int
    variant: str
    requests: list = field(default_factory=list)
    outputs: list = field(default_factory=list)
    failures: list = field(default_factory=list)
    result_bytes: int = 0
    fixture_session: str = ""

    @property
    def actions(self):
        if self.shape == "chain":
            return ["list"] + ["status"] * (self.size - 1)
        if self.shape == "fault":
            return ["list", "summary", "plan_status"]
        return [ACTIONS[i % 3] for i in range(self.size)]

    def swarm_call(self, action, tool_schema, prior=None):
        args = {"action": action, "intent": INTENT}
        if prior:
            args["to_session"] = SID.search(prior).group()
        if self.variant == "native-null":
            args = dict.fromkeys(tool_schema["parameters"]["properties"]) | args
        return "swarm", args

    def code_call(self):
        call = f'const read=(action,args={{}})=>tools.swarm({{action,intent:{compact(INTENT)},...args}});'
        if self.shape == "chain":
            run = ('const a=[await read("list")];'
                   f'for(let i=1;i<{self.size};i++) a.push(await read("status",'
                   '{to_session:a.at(-1).match(/session_[\\w-]+/)[0]}));')
            pick = 'return a.map((x,i)=>i?x.match(/Lifecycle: (\\w+)/)[1]:x.match(/session_[\\w-]+/)[0]);'
        elif self.shape == "fault":
            run = ('const ops=["list","summary","plan_status"];'
                   'const a=(await Promise.allSettled(ops.map(x=>read(x))))'
                   '.map(r=>r.status==="fulfilled"?r.value:String(r.reason));')
            pick = ('if(!a[1].includes("requires one `to_session` ID")) throw Error(a[1]);'
                    'return [a[0].match(/session_[\\w-]+/)[0],"missing-target",a[2].match(/Items: (\\d+)/)[1]];')
        elif self.size == 1:
            call = ""
            run = f'const a=[await tools.swarm({compact({"action": "list", "intent": INTENT})})];'
            pick = 'return [a[0].match(/session_[\\w-]+/)[0]];'
        else:
            literal = compact(self.actions)
            loop = f'Array.from({{length:{self.size}}},(_,i)=>{compact(ACTIONS)}[i%3])'
            ops = min((literal, loop), key=len)
            run = f'const a=await Promise.all({ops}.map(x=>read(x)));'
            pick = (f'const p={compact(list(PATTERNS.values()))};'
                    'return a.map((x,i)=>x.match(p[i%3])[1]);')
        select = pick if self.variant == "project" else 'return a;'
        return "codemode", {"code": call + run + select, "intent": INTENT}

    def complete(self, request, wire_bytes):
        self.requests.append((request, wire_bytes))
        tools = {tool["function"]["name"]: tool["function"] for tool in request["tools"]}
        assert set(tools) == {"swarm", "codemode"}, set(tools)
        history = {msg["tool_call_id"]: without_timing(msg["content"])
                   for msg in request["messages"] if msg["role"] == "tool"}
        issued = {call["id"]: call["function"] for output in self.outputs
                  for call in output.get("tool_calls", [])}
        echoed = {call["id"]: call["function"] for msg in request["messages"]
                  for call in msg.get("tool_calls", [])}
        assert issued.keys() == echoed.keys() == history.keys()
        for call_id, sent in issued.items():
            actual = echoed[call_id]
            assert sent["name"] == actual["name"]
            assert json.loads(sent["arguments"]) == json.loads(actual["arguments"])
        if not history and self.variant in {"passthrough", "project"}:
            calls = [self.code_call()]
        elif self.variant.startswith("native") and len(history) < self.size:
            remaining = self.actions[len(history):]
            if self.shape == "chain":
                prior = history.get(f"call-cost-{len(history) - 1}")
                calls = [self.swarm_call(remaining[0], tools["swarm"], prior)]
            else:
                assert not history, "the independent native arm must batch into one model turn"
                calls = [self.swarm_call(action, tools["swarm"]) for action in remaining]
        else:
            self.verify(history)
            delta = {"role": "assistant", "content": DONE}
            self.outputs.append(delta)
            return delta, "stop"

        start = len(history)
        calls = [{"id": f"call-cost-{start + i}", "type": "function",
                  "function": {"name": name, "arguments": compact(args)}}
                 for i, (name, args) in enumerate(calls)]
        delta = {"role": "assistant", "tool_calls": calls}
        self.outputs.append(delta)
        return delta | {"tool_calls": [c | {"index": i} for i, c in enumerate(calls)]}, "tool_calls"

    def verify(self, history):
        expected_failures = int(self.shape == "fault")
        if self.variant.startswith("native"):
            assert len(history) == self.size
            values = [history[f"call-cost-{i}"] for i in range(self.size)]
            self.result_bytes = sum(len(v.encode()) for v in values)
            observed_failures = sum("requires one `to_session` ID" in v for v in values)
            values = [projected(v, op) for v, op in zip(values, self.actions)]
        else:
            assert len(history) == 1
            report = history["call-cost-0"]
            body, _, _ = report.partition("\ncalls:\n")
            assert body.startswith("result:\n"), report
            values = json.loads(body.removeprefix("result:\n"))
            receipts = RECEIPT.findall(report)
            assert len(receipts) == self.size, report
            assert all(status in {"ok", "failed"} for _, status, _ in receipts), report
            observed_failures = sum(status == "failed" for _, status, _ in receipts)
            self.result_bytes = sum(int(size) for _, _, size in receipts)
            if self.variant == "passthrough":
                assert sorted(int(size) for _, status, size in receipts if status == "ok") == sorted(
                    len(v.encode()) for v, op in zip(values, self.actions) if op != "summary")
                values = [projected(v, op) for v, op in zip(values, self.actions)]
        assert observed_failures == expected_failures
        assert len(values) == self.size
        for value, op in zip(values, self.actions):
            if op == "list":
                assert SID.fullmatch(value), value
                assert not self.fixture_session or value == self.fixture_session
                self.fixture_session = value
            else:
                assert value == {"status": "ready", "plan_status": "0", "summary": "missing-target"}[op], values

    def measure(self, tokens):
        prompt_parts = [{key: request[key] for key in ("messages", "tools")}
                        for request, _ in self.requests]
        input_counts = [tokens(parts) for parts in prompt_parts]
        tool_turns = len(self.outputs) - 1
        return (len(input_counts), tool_turns, sum(input_counts),
                sum(count - input_counts[0] for count in input_counts),
                sum(tokens(output) for output in self.outputs), input_counts[-1],
                sum(wire for _, wire in self.requests), self.result_bytes)


class Provider(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        body = compact({"data": [{"id": MODEL, "object": "model"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        body = self.rfile.read(int(self.headers["content-length"]))
        try:
            assert self.path == "/v1/chat/completions", self.path
            delta, finish = self.server.probe.complete(json.loads(body), len(body))
        except Exception as error:
            self.server.probe.failures.append(error)
            self.send_error(500, "fixture verification failure")
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for choice in [{"index": 0, "delta": delta}, {"index": 0, "delta": {}, "finish_reason": finish}]:
            chunk = {"id": "chatcmpl-fixture", "object": "chat.completion.chunk", "model": MODEL,
                     "choices": [choice]}
            self.wfile.write(b"data: " + compact(chunk).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")


def run(binary, server, probe):
    server.probe = probe
    with tempfile.TemporaryDirectory(prefix="swarm-cost-", dir="/tmp") as temp:
        root = Path(temp).resolve()
        home, runtime, jcode = (root / name for name in ("h", "r", "j"))
        for directory in (home, runtime, jcode):
            directory.mkdir()
        socket = str(runtime / "j.sock")
        env = {
            "PATH": "/usr/bin:/bin", "HOME": str(home), "USER": "fixture", "TERM": "xterm-256color",
            "TMPDIR": str(runtime), "XDG_RUNTIME_DIR": str(runtime), "JCODE_HOME": str(jcode),
            "JCODE_RUNTIME_DIR": str(runtime), "JCODE_SOCKET": socket, "JCODE_TEST_SESSION": "1",
            "JCODE_OPENAI_COMPAT_API_BASE": f"http://127.0.0.1:{server.server_port}/v1",
            "OPENAI_COMPAT_API_KEY": "fixture-only", "JCODE_OPENAI_COMPAT_DEFAULT_MODEL": MODEL,
            "JCODE_NO_TELEMETRY": "1", "JCODE_TELEMETRY": "off", "JCODE_NO_AUTO_UPDATE": "1",
            "JCODE_CODEMODE": "1",
        }
        command = [binary, "run", "--no-update", "--no-selfdev", "--provider", "openai-compatible",
                   "--model", MODEL, "--tools", "swarm,codemode", "--socket", socket, "--json", PROMPT]
        result = subprocess.run(command, env=env, cwd=root, capture_output=True, text=True, timeout=35)
        if probe.failures:
            raise probe.failures[0]
        assert result.returncode == 0, result.stderr
        finished = json.loads(result.stdout)
        assert finished["text"] == DONE, finished["text"]
        assert finished["session_id"] == probe.fixture_session
        expected_turns = probe.size + 1 if probe.shape == "chain" and probe.variant == "native" else 2
        assert len(probe.requests) == expected_turns, len(probe.requests)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=lambda value: str(Path(value).resolve(strict=True)))
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--sizes", type=int, nargs="+", default=[1, 2, 3, 4, 8, 16])
    args = parser.parse_args()
    if args.repeats < 1 or min(args.sizes) < 1 or max(args.sizes) > 16:
        parser.error("expected repeats >= 1 and sizes in 1..16")
    encoding = tiktoken.get_encoding("o200k_base")
    def tokens(value):
        return len(encoding.encode_ordinary(compact(value)))
    print(subprocess.check_output([args.binary, "--version"], text=True).strip())
    variants = ("native", "passthrough", "project")
    cases = [("independent", n, variants + ("native-null",)) for n in args.sizes]
    cases += [("chain", n, variants) for n in args.sizes if n > 1]
    cases += [("fault", 3, variants)]
    results = defaultdict(list)
    first_schema = None
    with ThreadingHTTPServer(("127.0.0.1", 0), Provider) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            for shape, n, arms in cases:
                for repeat in range(args.repeats):
                    start = repeat % len(arms)
                    for variant in arms[start:] + arms[:start]:
                        probe = Probe(shape, n, variant)
                        run(args.binary, server, probe)
                        schema = probe.requests[0][0]["tools"]
                        if first_schema is None:
                            first_schema = schema
                            for tool in schema:
                                fn = tool["function"]
                                params = fn["parameters"]
                                print("schema", fn["name"], "tokens", tokens(tool), "required",
                                      params["required"], "properties", len(params["properties"]),
                                      "strict", fn.get("strict"))
                        assert all(req["tools"] == first_schema for req, _ in probe.requests)
                        results[shape, n, variant].append(probe.measure(tokens))
        finally:
            server.shutdown()
            thread.join()
    print("shape n arm requests tool-turns input incr-input output final-input request-bytes result-bytes")
    for shape, n, arms in cases:
        for variant in arms:
            columns = zip(*results[shape, n, variant])
            numbers = [str(median(col)) if min(col) == max(col)
                       else f"{median(col)}[{min(col)}..{max(col)}]" for col in columns]
            print(shape, n, variant, *numbers)
    print("verified", sum(map(len, results.values())), "runs,",
          args.repeats * len(variants), "expected nested/direct failures, no unexpected failures")
    for value in ("agent ready ", "session:task/7fc9de3f\n"):
        text = (value * (200184 // len(value) + 1))[:200184]
        print("illustration: same byte count", len(text.encode()), "text tokens",
              len(encoding.encode_ordinary(text)))


if __name__ == "__main__":
    main()
