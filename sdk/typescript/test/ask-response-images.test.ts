import { test } from "node:test";
import assert from "node:assert/strict";
import { JcodeClient } from "../dist/index.js";
import { startMockHarness } from "./mock-harness.ts";

test("ask images are never sent to a server without the capability", async () => {
  for (const supported of [false, true]) {
    const requests: any[] = [];
    const server = await startMockHarness({
      capabilities: supported ? ["ask_response_images"] : [],
      onRequest: (request, send) => {
        requests.push(request);
        send({ v: 1, reply_to: request.id, ev: "ok" });
      },
    });
    const client = await JcodeClient.connect({ socketPath: server.socketPath });
    try {
      const response = { answers: [{ selected: ["B"], other_images: [["image/png", "YWJj"]] as [string, string][], comment_images: { B: [["image/jpeg", "ZGVm"]] as [string, string][] } }] };
      if (supported) {
        await client.respondToAsk("s", "r", response);
        assert.deepEqual(requests[0].response, response);
      } else {
        await assert.rejects(client.respondToAsk("s", "r", response), { code: "unsupported" });
        await assert.rejects(client.respondToAsk("s", "r", { answers: [{ selected: ["B"], comment_images: { B: [["image/jpeg", "ZGVm"]] } }] }), { code: "unsupported" });
        await assert.rejects(client.request({ req: "ask_response", session_id: "s", request_id: "r", response }), { code: "unsupported" });
        assert.equal(requests.length, 0);
        await client.respondToAsk("s", "r", { answers: [{ selected: ["B"], other: "text only" }] });
        assert.equal(requests.length, 1);
        assert.equal(requests[0].response.answers[0].other, "text only");
      }
    } finally {
      client.close();
      await server.close();
    }
  }
});
