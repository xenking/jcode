import { test } from "node:test";
import assert from "node:assert/strict";
import { JcodeClient } from "../dist/index.js";
import { startMockHarness } from "./mock-harness.ts";

test("force stop sends the session and awaits its correlated reply", async (t) => {
  const requests: any[] = [];
  const server = await startMockHarness({
    capabilities: ["force_stop_turn_v1"],
    onRequest(request, send) {
      requests.push(request);
      send({ v: 1, reply_to: request.id, ev: "ok" });
    },
  });
  t.after(() => server.close());
  const client = await JcodeClient.connect({ socketPath: server.socketPath });
  t.after(() => client.close());
  await client.forceStopTurn("s1");
  assert.deepEqual(requests.map(({ v, id, ...request }) => request), [
    { req: "force_stop_turn", session_id: "s1" },
  ]);
});
