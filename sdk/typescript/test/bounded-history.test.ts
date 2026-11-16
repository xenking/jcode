import { test } from "node:test";
import assert from "node:assert/strict";
import { JcodeClient } from "../dist/index.js";
import { startMockHarness } from "./mock-harness.ts";

test("bounded history reassembles UTF-8 fragments and lazily fetches images", async (t) => {
  const message = { role: "user", content: "Ж image", tool: null };
  const image = { media_type: "image/png", data: "jcode-lazy:key", label: null, source: { kind: "user_input" }, anchor: { kind: "user_prompt", ordinal: 0 } };
  const row = JSON.stringify({ message, images: [image] });
  const boundary = row.indexOf("Ж");
  const left = row.slice(0, boundary), right = row.slice(boundary);
  const start = Buffer.byteLength(left), total = Buffer.byteLength(row);
  let pages = 0, imageRequests = 0;
  const server = await startMockHarness({ onRequest(request, send) {
    if (request.req === "get_history_page") {
      pages++;
      const first = request.before === null;
      send({ v: 1, reply_to: request.id, ev: "history_page", session_id: "s1", fragments: [{ index: 0, start: first ? start : 0, total, text: first ? right : left }], before: first ? "cursor" : null });
    } else if (request.req === "get_history_image_chunk") {
      imageRequests++;
      const first = request.offset === 0;
      send({ v: 1, reply_to: request.id, ev: "history_image_chunk", session_id: "s1", key: "key", offset: request.offset, total: 3, data: first ? "Ж" : "a" });
    }
  }});
  t.after(() => server.close());
  const client = await JcodeClient.connect({ socketPath: server.socketPath });
  t.after(() => client.close());
  const result = await client.getHistoryPage("s1");
  assert.equal(pages, 2);
  assert.equal(imageRequests, 0);
  assert.equal(result.messages[0].content, "Ж image");
  assert.equal(result.images[0].data, "jcode-lazy:key");
  assert.equal(await client.getHistoryImage("s1", "key"), "Жa");
  assert.equal(imageRequests, 2);
});
