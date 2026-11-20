import { test } from "node:test";
import assert from "node:assert/strict";
import { isKnownEvent, type ApiEvent } from "../dist/protocol.js";

test("final request usage is a known event and preserves zero and missing counters", () => {
  const completion = {
    ev: "request_usage",
    session_id: "s1",
    request_id: "r1",
    provider: "openai",
    model: "served-model",
    input: 100,
    output: null,
    cache_read_input: 0,
  } satisfies ApiEvent;

  assert.ok(isKnownEvent(completion));
  assert.equal(completion.cache_read_input, 0);
  assert.equal(completion.output, null);
  assert.ok(!("cache_creation_input" in completion));
});
