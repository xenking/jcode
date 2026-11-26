#!/usr/bin/env python3
"""Isolated acceptance: a swarm worker's MCP approval reaches the root client.

Usage: scripts/accept-mcp-swarm.py <jcode-binary> [--keep DIR]

A private daemon (own JCODE_HOME, runtime dir and socket) with the fixture MCP
server of accept-mcp-typed.py and a scripted provider. The root session is a
protocol client. Its model spawns one headless worker; the worker's model makes
gated writes through codemode. The worker has no client, so the daemon must show
each card on the root client, labelled with the worker, and the root client's
answers (Allow once, Deny, Always every tool on this server) decide the calls.
Proved by the fixture's write log, not by messages alone.
"""
import argparse
import importlib.util
import json
import pathlib
import sys
import tempfile
from http.server import BaseHTTPRequestHandler

HERE = pathlib.Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("accept_mcp_typed", HERE / "accept-mcp-typed.py")
T = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(T)

WORKER_SCRIPT = """
const out = {};
for (const n of [1, 2, 3]) {
  try { await tools.mcp__fixture__write_item({ n }); out['write' + n] = 'ran'; }
  catch (e) { out['write' + n] = String(e.message).slice(0, 40); }
}
try { await tools.mcp__fixture__bare({ n: 4 }); out.bare = 'ran'; } catch (e) { out.bare = 'refused'; }
return out;
"""


def role_provider(self):
    """The root's model spawns one worker; the worker's model runs WORKER_SCRIPT."""
    request = json.loads(self.rfile.read(int(self.headers["content-length"])))
    messages = request.get("messages", [])

    def text(message):
        content = message.get("content") or ""
        if isinstance(content, list):
            content = "\n".join(p.get("text", "") for p in content if isinstance(p, dict))
        return content

    users = [i for i, m in enumerate(messages) if m.get("role") == "user"]
    # A worker is forked from the root's history; its own task is the last user message.
    last_user = text(messages[users[-1]]) if users else ""
    since = messages[users[-1] + 1:] if users else messages
    called = any(m.get("tool_calls") for m in since if m.get("role") == "assistant")
    last = messages[-1] if messages else {}
    if request.get("tools") and not called and last.get("role") != "tool":
        if "WORKER-TASK" in last_user:
            name, args = "codemode", {"intent": "gated writes", "code": WORKER_SCRIPT}
        else:
            name, args = "swarm", {"action": "spawn", "label": "gated-writer", "spawn_mode": "headless",
                                   "prompt": "WORKER-TASK: make the gated writes."}
        call = {"index": 0, "id": "call-" + name, "type": "function", "function": {"name": name, "arguments": json.dumps(args)}}
        delta, finish = {"role": "assistant", "tool_calls": [call]}, "tool_calls"
    else:
        delta, finish = {"role": "assistant", "content": "RESULT: " + text(last)[:2000]}, "stop"
    self.send_response(200)
    self.send_header("content-type", "text/event-stream")
    self.end_headers()
    for choice in [{"index": 0, "delta": delta}, {"index": 0, "delta": {}, "finish_reason": finish}]:
        chunk = {"id": "fake", "object": "chat.completion.chunk", "model": "fake-model", "choices": [choice]}
        self.wfile.write(b"data: " + json.dumps(chunk).encode() + b"\n\n")
    self.wfile.write(b"data: [DONE]\n\n")
    self.wfile.flush()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binary")
    parser.add_argument("--keep", help="directory that keeps the daemon log and the cards")
    args = parser.parse_args()
    T.Provider.do_POST = role_provider
    with tempfile.TemporaryDirectory(prefix="jcode-mcp-swarm-", dir="/tmp") as temp:
        root = pathlib.Path(temp).resolve()
        run = T.Run(args.binary, root, "swarm", T.approval_servers(root, "swarm"), "")
        # Long enough for the cards to be answered, short enough to fail fast.
        run.env["JCODE_MCP_APPROVAL_ATTACH_WAIT_SECS"] = "5"
        try:
            client = None
            client, _ = T.daemon_session(run)
            client.send(type="message", id=2, content="delegate the gated writes")
            answers = ["Allow once", "Deny", "Always allow every tool on this server"]
            cards = []
            for index, label in enumerate(answers, start=1):
                card = client.until(lambda e: e.get("type") == "ask_user_request"
                                    and e.get("request_id", "").startswith("ask-mcp-approval-"), f"worker card {index}")
                question = card["questions"][0]
                cards.append(card)
                T.check(card["request_id"].startswith("ask-mcp-approval-worker-"), f"card {index} is a worker approval", card["request_id"])
                T.check(question["question"].startswith("Worker \"") and '"write-item"' in question["question"]
                        and '"fixture"' in question["question"] and f'"n":{index}' in question["question"],
                        f"card {index} names the worker, server, tool and arguments", question["question"])
                T.check([o["label"] for o in question["options"]]
                        == ["Deny", "Allow once", "Always allow this tool", "Always allow every tool on this server"],
                        f"card {index} offers the four options", question["options"])
                client.send(type="ask_user_response", id=100 + index, request_id=card["request_id"],
                            response={"answers": [{"selected": [label]}]})
            T.wait_for(lambda: len(run.write_lines()) >= 3, "the worker's writes", 60, lambda: "\n".join(run.write_lines()))
            T.check(run.write_lines() == ['write-item:{"n": 1}', 'write-item:{"n": 3}', 'bare:{"n": 4}'],
                    "Allow once and Always ran their calls, Deny ran nothing, the next tool was not asked", run.write_lines())
            T.check(len(cards) == 3, "exactly three cards were shown", len(cards))
            saved = json.loads((run.root / "jcode" / "mcp.json").read_text())["servers"]["fixture"]["approve_free"]
            T.check(saved == ["*"], "Always every tool is saved in mcp.json", saved)
            logs = sorted((run.root / "jcode" / "logs").glob("*.log"))
            log = "\n".join(path.read_text() for path in logs)
            for outcome in ("allowed_once", "denied", "allowed_always_server"):
                T.check(any(f"MCP_APPROVAL outcome={outcome} session=" in line and " routed_to=" in line
                            and " server=fixture tool=write-item" in line for line in log.splitlines()),
                        f"daemon log has {outcome} with session and routed_to")
            if args.keep:
                keep = pathlib.Path(args.keep)
                keep.mkdir(parents=True, exist_ok=True)
                (keep / "daemon.log").write_text(log)
                (keep / "cards.json").write_text(json.dumps(cards, indent=2))
            client.close()
        except BaseException:
            # The temp dir is removed on exit: show why before it goes.
            logs = sorted((run.root / "jcode" / "logs").glob("*.log"))
            lines = logs[-1].read_text().splitlines() if logs else []
            print("--- jcode log tail ---\n" + "\n".join(lines[-60:]), file=sys.stderr)
            print("--- client events ---\n" + "\n".join(str(e)[:200] for e in (client.seen if client else [])[-20:]), file=sys.stderr)
            raise
        finally:
            run.stop()
    print("mcp swarm approval acceptance passed")


if __name__ == "__main__":
    sys.exit(main())
