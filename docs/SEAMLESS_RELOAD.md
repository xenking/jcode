# Seamless reload

A daemon reload or restart must continue an interrupted session as if nothing
happened. It must not send a prompt "from the user".

## Contract

- **No synthetic user message.** A reload continuation is part of the assistant
  turn. If a tool was running, the daemon persists a `tool_result` for that
  `tool_use` saying the restart interrupted it, then continues the same turn. If
  generation was cut off mid-stream, the turn continues from the assistant's own
  partial text. If the last user prompt was never answered, the turn simply
  runs.
- **Prompt cache is preserved.** The resumed provider request is the
  pre-restart request plus at most that one `tool_result`. No message is
  reordered, removed, or inserted before it, so the provider's cached prefix
  still matches. `crates/jcode-app-core/src/agent/reload_resume_tests.rs`
  asserts this on the neutral provider request.
- **Idle sessions are not touched.** A finished turn has no recovery intent and
  is never re-prompted.
- **Ready headless workers keep their address.** The daemon checkpoints swarm
  ownership before shutdown. A ready worker with a live coordinator in its
  swarm retains its ID, report and lane, and loads its agent on the next wake
  or retry. Unowned idle workers still enter terminal-member collection.
- **Interrupted headless workers remain busy.** A saved running worker still
  appears running while its continuation is being admitted. The coordinator
  observes the actual result, not an intermediate crash or idle status.
- **Clients show a status, not a transcript row.** TUI and Desktop show a
  transient "reconnected"/"resumed" status line. The display-only
  `[generation interrupted - server reloading]` marker is stripped from history
  rendering and from the resumed request.
- **System wakes are not resumes.** An idle swarm worker with an open todo lane
  has no interrupted assistant turn. Its recovery record is marked `wake` and
  keeps the hidden reminder prompt; only real turn resumptions are message-free.

## What was lost before

| State | Before | Now |
| --- | --- | --- |
| Interrupted turn | A user-role `[auto] …` message was appended (visible as a prompt) | Tool result or assistant continuation; no user text |
| Queued prompts | The recovery agent had an empty queue and its turn overwrote the durable store | The recovery agent restores the persisted queue before the turn |
| Pending `ask` | Replayed, but admission could race | Record stays `Claimed`; the answer is the receipt |
| Mid-stream text | Marker appended and left in the prompt and transcript | Marker stripped before resume and in rendering |
| Prompt cache | A user message changed the prefix every reload | Prefix + one tool result |

## Design

- `Agent::reload_resume_plan(durable_intent)` classifies the transcript:
  pending tools, unanswered ask, truncated assistant text, unanswered user
  prompt, or nothing.
- `Agent::resume_interrupted_turn_mpsc` appends idempotent interrupted
  `tool_result`s (never for `ask`), strips the marker, and runs the normal turn
  loop.
- `RecoveryOwner` admits the record (`admit_resume`) only for continuable
  plans, and closes a record with `Blocked` when the transcript has nothing to
  resume.
- Anthropic sends a trailing assistant message as a prefill continuation. Models
  that reject prefill get one retry with the minimal continuation user turn at
  the transport layer, never in the transcript.

## Evidence

`cargo test -p jcode-app-core agent::reload_resume_tests` covers reload
mid-tool (prefix + tool result only), mid-stream, queued prompts, and idle.
