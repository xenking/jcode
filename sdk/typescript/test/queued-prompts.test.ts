import { test } from "node:test";
import assert from "node:assert/strict";
import { JcodeClient, HarnessError, isKnownEvent } from "../dist/index.js";
import { startMockHarness } from "./mock-harness.ts";

test("queue methods retain modes, images, reminder and correlated replies", async (t) => {
  const requests: any[] = [];
  const prompt = { client_id: "q1", content: "look", images: [["image/png", "YWJj"]] as [string, string][], mode: "after_yield" as const };
  const server = await startMockHarness({
    onRequest(request, send) {
      requests.push(request);
      if (request.req === "get_queued_prompts") {
        send({ v: 1, reply_to: request.id, ev: "queued_prompts", session_id: "s1", prompts: [prompt] });
      } else if (request.req === "remove_queued_prompt") {
        send({ v: 1, reply_to: request.id, ev: "queued_prompt_removed", session_id: "s1", prompt_id: request.prompt_id, outcome: "in_flight" });
      } else if (request.req === "interrupt_queued_prompt") {
        send({ v: 1, reply_to: request.id, ev: "error", code: "not_found", message: "missing" });
      } else {
        send({ v: 1, reply_to: request.id, ev: "ok" });
      }
    },
  });
  t.after(() => server.close());
  const client = await JcodeClient.connect({ socketPath: server.socketPath });
  t.after(() => client.close());
  await client.queuePromptWithReminder("s1", prompt, "hidden");
  await client.queuePrompt("s1", { client_id: "q2", content: "next", mode: "steer", images: [] });
  await client.setQueuedPromptMode("s1", "q1", "steer");
  assert.deepEqual(await client.getQueuedPrompts("s1"), [prompt]);
  assert.equal(await client.removeQueuedPrompt("s1", "panel", "q1"), "in_flight");
  await client.reorderQueuedPrompts("s1", "panel", ["q2", "q1"]);
  await assert.rejects(() => client.interruptQueuedPrompt("s1", "missing"), (error: unknown) => error instanceof HarnessError && error.code === "not_found");
  const wire = requests.map(({ v, id, ...request }) => request);
  assert.deepEqual(wire, [
    { req: "queue_prompt", session_id: "s1", ...prompt, system_reminder: "hidden" },
    { req: "queue_prompt", session_id: "s1", client_id: "q2", content: "next", mode: "steer" },
    { req: "set_queued_prompt_mode", session_id: "s1", client_id: "q1", mode: "steer" },
    { req: "get_queued_prompts", session_id: "s1" },
    { req: "remove_queued_prompt", session_id: "s1", client_id: "panel", prompt_id: "q1" },
    { req: "reorder_queued_prompts", session_id: "s1", client_id: "panel", order: ["q2", "q1"] },
    { req: "interrupt_queued_prompt", session_id: "s1", client_id: "missing" },
  ]);
  assert.ok(isKnownEvent({ ev: "queued_prompts", session_id: "s1", prompts: [prompt] }));
});
