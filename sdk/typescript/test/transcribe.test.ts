import { test } from "node:test";
import assert from "node:assert/strict";
import { JcodeClient, HarnessError, TRANSCRIBE_CAPABILITY, MAX_TRANSCRIBE_WAV_BYTES } from "../dist/index.js";
import { startMockHarness } from "./mock-harness.ts";

test("transcribeAudio sends base64 audio and returns the transcript", async (t) => {
  const requests: any[] = [];
  const server = await startMockHarness({
    capabilities: [TRANSCRIBE_CAPABILITY],
    onRequest(req, send) {
      requests.push(req);
      if (req.req !== "transcribe_audio") return;
      if (req.language === "xx") {
        send({ v: 1, reply_to: req.id, ev: "error", code: "transcription_not_configured", message: "no key" });
      } else {
        send({ v: 1, reply_to: req.id, ev: "audio_transcript", text: "hello there" });
      }
    },
  });
  t.after(() => server.close());
  const client = await JcodeClient.connect({ socketPath: server.socketPath });
  t.after(() => client.close());

  assert.equal(await client.transcribeAudio(new Uint8Array([1, 2, 3, 4]), "en"), "hello there");
  assert.deepEqual(
    { ...requests.at(-1), id: undefined, v: undefined },
    { req: "transcribe_audio", wav: "AQIDBA==", language: "en", id: undefined, v: undefined },
  );
  await assert.rejects(client.transcribeAudio(new Uint8Array([1]), "xx"), (error: unknown) =>
    error instanceof HarnessError && error.code === "transcription_not_configured");
  const before = requests.length;
  await assert.rejects(client.transcribeAudio(new Uint8Array(MAX_TRANSCRIBE_WAV_BYTES + 1)), (error: unknown) =>
    error instanceof HarnessError && error.code === "audio_too_large");
  assert.equal(requests.length, before, "an oversized recording is never sent");
});

test("transcribeAudio refuses before sending when the daemon lacks the capability", async (t) => {
  const requests: any[] = [];
  const server = await startMockHarness({ onRequest: (req) => requests.push(req) });
  t.after(() => server.close());
  const client = await JcodeClient.connect({ socketPath: server.socketPath });
  t.after(() => client.close());
  const before = requests.length;
  await assert.rejects(client.transcribeAudio(new Uint8Array([1])), (error: unknown) =>
    error instanceof HarnessError && error.code === "unsupported");
  assert.equal(requests.length, before);
});
