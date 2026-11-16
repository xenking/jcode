# One-shot CLI

A user submits a prompt with `jcode run`, receives the assistant response and a meaningful exit code, then can inspect the saved conversation.

## Sub-features

- CLI-PLAIN: streamed human-readable output.
- CLI-JSON: one machine-readable result.
- CLI-NDJSON: incremental machine-readable events.
- CLI-SAVE: prompt/reply persist in the run's own session directory.

## How to get to it (user POV)

From a shell: `jcode --no-update --socket <private>/app.sock run 'prompt'`, optionally adding `run --json` or `run --ndjson`. The helper records its exact command and explicitly selects the fake provider/model.

## Driving it with verify-jcode

Preconditions: Launch and doctor passed. No prior conversation is needed.

- **Plain:** `"$V" drive --evidence "$E" cli --mode plain`. Require exit 0, exact fake ack in stdout, fresh HTTP request containing the generated token and a saved session containing that prompt.
- **JSON:** `"$V" drive --evidence "$E" cli --mode json`. Same assertions plus valid parsed JSON result with the exact ack and session ID in the drive receipt.
- **Events:** `"$V" drive --evidence "$E" cli --mode ndjson`. The helper parses every stdout line, requires start/done with the same session ID and the exact completed text. Inspect the event sequence for changes to streaming, not just the presence of the ack.
- **Proof:** retain `drive-*-cli.{stdout,stderr,command.json,json}`, copied `drive-*-session_*.json`, `provider.jsonl` and `actions.jsonl`.

Plain stdout includes the model ack followed by `[Tokens] upload: 1 download: 1` for this fake fixture. The helper checks both, not a substring that could hide extra output.

## Gotchas

- Current `run` hosts its agent in-process and only starts a temporary server when tools need one. A private socket alone does not isolate history: both CLI and daemon also need private JCODE_HOME and JCODE_TEST_SESSION.
- An empty data directory must work. A PATH binary or a listening shared daemon does not prove the build under review.
- Each mode is its own entry point. The warm JSON turn during launch does not cover plain or NDJSON.
- This fixture does not test real vendor authentication, rate limits, cancellation or failure exits. A matching ack is insufficient evidence for changes to those paths.
