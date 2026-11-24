import { test } from "node:test";
import assert from "node:assert/strict";
import { JcodeClient, HarnessError, ROUTING_CAPABILITY } from "../dist/index.js";
import { startMockHarness } from "./mock-harness.ts";

test("routing requests carry their documented fields and replies are typed", async (t) => {
  const requests: any[] = [];
  const server = await startMockHarness({
    capabilities: [ROUTING_CAPABILITY],
    onRequest(req, send) {
      requests.push(req);
      switch (req.req) {
        case "routing_gate_answer":
          if (req.gate_id === "gone") {
            send({ v: 1, reply_to: req.id, ev: "error", code: "internal", message: "No routing gate gone is waiting for this session" });
          } else {
            send({ v: 1, reply_to: req.id, ev: "ok" });
          }
          break;
        case "routing_intent":
          send({ v: 1, reply_to: req.id, ev: "routing_status", text: `ran ${req.action.action}` });
          break;
        case "get_routing_view":
          send({
            v: 1,
            reply_to: req.id,
            ev: "routing_view",
            view: {
              mode: "auto",
              paid: "off",
              accounts: [{ id: "claude:team", kind: "subscription", priority: 1, on_limit: "ask", windows: [{ window: "5h", unit: "percent", used: 42.5 }] }],
              budgets: [{ name: "hf-month", scope_kind: "provider", scope: "huggingface", meter: "ledger", window: "month", unit: "usd", limit: 50, action: "block" }],
            },
          });
          break;
      }
    },
  });
  t.after(() => server.close());
  const client = await JcodeClient.connect({ socketPath: server.socketPath });
  t.after(() => client.close());

  await client.answerRoutingGate("s1", "g1", { kind: "ack", minutes: 30 });
  assert.deepEqual(requests.at(-1), { ...requests.at(-1), req: "routing_gate_answer", session_id: "s1", gate_id: "g1", choice: { kind: "ack", minutes: 30 } });

  assert.equal(await client.routingIntent("s1", { action: "hold", account: "claude:team", minutes: 90 }), "ran hold");
  assert.deepEqual(requests.at(-1).action, { action: "hold", account: "claude:team", minutes: 90 });

  const view = await client.routingView();
  assert.equal(requests.at(-1).req, "get_routing_view");
  assert.equal(view.mode, "auto");
  assert.equal(view.accounts?.[0].windows?.[0].used, 42.5);
  assert.equal(view.budgets?.[0].used, undefined, "an unreadable meter stays absent");

  await assert.rejects(client.answerRoutingGate("s1", "gone", { kind: "cancel" }), (error: unknown) => {
    assert.ok(error instanceof HarnessError);
    assert.equal(error.code, "internal");
    return true;
  });
});

test("a runtime without the routing capability is refused before a frame is sent", async (t) => {
  const requests: any[] = [];
  const server = await startMockHarness({ capabilities: ["sessions"], onRequest: (req) => requests.push(req) });
  t.after(() => server.close());
  const client = await JcodeClient.connect({ socketPath: server.socketPath });
  t.after(() => client.close());

  await assert.rejects(client.routingView(), (error: unknown) => error instanceof HarnessError && error.code === "unsupported");
  await assert.rejects(client.routingIntent("s1", { action: "status" }), HarnessError);
  await assert.rejects(client.answerRoutingGate("s1", "g1", { kind: "wait" }), HarnessError);
  assert.deepEqual(requests, []);
});

test("routing events arrive on the event stream", async (t) => {
  const server = await startMockHarness({ capabilities: [ROUTING_CAPABILITY] });
  t.after(() => server.close());
  const client = await JcodeClient.connect({ socketPath: server.socketPath });
  t.after(() => client.close());
  const seen = new Promise<any[]>((resolve) => {
    const events: any[] = [];
    client.on("event", (frame: any) => {
      events.push(frame);
      if (frame.ev === "routing_gate_closed") resolve(events);
    });
  });
  server.broadcast({ v: 1, ev: "routing_decision", session_id: "s1", account: "claude:team", reason: "priority 1", pending: "claude:spare" });
  server.broadcast({ v: 1, ev: "routing_gate", session_id: "s1", gate_id: "g1", reason: "limit", options: [{ kind: "wait_until", resets_at: 5 }] });
  server.broadcast({ v: 1, ev: "routing_gate_closed", session_id: "s1", gate_id: "g1" });
  const events = await seen;
  assert.deepEqual(events.map((e) => e.ev), ["routing_decision", "routing_gate", "routing_gate_closed"]);
  assert.equal(events[0].pending, "claude:spare");
});
