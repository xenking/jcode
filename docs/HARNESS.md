# Harness details

## Session subprocess environment

Clients can replace the current session's subprocess environment overrides using
`SetSessionEnv` (or the SDK's `setSessionEnv` / `set_session_env` helper). The map
replaces, rather than merges with, previous overrides. An empty map clears it.
Keys must be nonempty and contain neither `=` nor NUL, values cannot contain NUL,
and `JCODE_*` keys are reserved (case-insensitively).

These overrides apply to session-owned Bash subprocess launches, including queued
background launches. Each accepted job keeps its own snapshot. Updating or
clearing a session's map does not alter already accepted jobs or running
processes. Overrides do not mutate the daemon's process environment. They are
not a general override of provider authentication/configuration, shared MCP
servers, or unrelated daemon services.

Forks, transfers, and swarm children receive independent copies of their parent's
current overrides. Subsequent parent updates do not update existing children.
Clearing conversation history carries overrides to the replacement session ID.
Attaching an existing session uses that target's overrides, never the temporary
connecting session's map.
Session teardown removes the live registry entry, not environment copies already
held by running jobs. Independent sessions do not receive another session's map.

**Overrides are ephemeral and are explicitly lost on daemon restart or reload.**
They are not saved in session JSON, journals, reload recovery, or conversation
history. Loading a saved session does not restore them. Clients must reapply
overrides before submitting resumed work, including after reconnecting to a
restarted daemon. Automatic recovery may run without them: this feature adds no
fail-closed recovery barrier and must not be treated as one.

The ACP adapter defaults to `jcode acp --session-env none`, which leaves session
overrides unchanged. `jcode acp --session-env inherit` or
`JCODE_ACP_SESSION_ENV=inherit` replaces the overrides with the ACP process's own
environment after each `session/new`, `session/load`, or `session/resume`
subscription, before the request succeeds. The adapter drops `JCODE_*` keys
case-insensitively and names that are empty or contain `=` or NUL. Environment
names and values must be UTF-8, and values cannot contain NUL. An invalid value,
daemon rejection, or disconnect fails session setup instead of proceeding without
the requested environment. An explicit `--session-env` flag takes precedence over
the environment variable.

ACP automatically reconnects session connections after a daemon restart or
reload, starting a stopped daemon through the normal daemon-start path. It
re-attaches the same session ID and, with `--session-env inherit`, re-applies the
original environment snapshot before sending more work. It also restores a model
selected with `--model`, `session/set_config_option`, or `/model`. Loading or
resuming another session still keeps that session's saved model.

Reconnect attempts use exponential backoff from 100 ms up to 5 seconds, with a
120-second total deadline that includes attachment and environment/model setup.
A turn interrupted by a disconnect or reload returns an ACP error and is not
replayed. If reconnecting times out, waiting requests fail with a reconnect error.
The ACP process stays alive and the next request starts a new reconnect attempt.

For `session/new`, an explicit ACP `--model` selects the session's model even
when the daemon is already running. A bare model uses `--provider-profile` or an
explicit non-`auto` `--provider` as its route. An explicitly routed model such as
`openai:gpt-5.5` keeps its route. The adapter waits for model confirmation and fails
session setup if the daemon rejects the model. `session/load` and `session/resume`
keep the existing session's model instead of applying the ACP launch model.

This is environment isolation, not a security sandbox or secret-redaction system.
The registry does not intentionally log or persist values, but a command can print
its environment into tool output, write it to disk, or transmit it elsewhere.
Subprocesses inherit ordinary daemon environment variables unless overridden,
and OS process inspection or another program running as the same user may expose
secrets. Only supply credentials to trusted commands and trusted child agents.

## Session tool HTTP proxies

`SetSessionEnv` also controls proxy routing for session-owned `webfetch` and all
local `websearch` engines: DuckDuckGo, Bing (HTML and API), SearXNG, custom HTTP
backends, and Kagi. This does not change API-key or endpoint resolution, which
still uses the existing tool configuration and daemon environment.

Recognized keys are `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY`, plus
their lowercase aliases. Each exact key resolves from the session first, then
the daemon environment. Reqwest's uppercase-first alias precedence is applied
after that layering. Consequently a lowercase session alias does not override an
inherited uppercase alias. Scheme-specific proxies take precedence over
`ALL_PROXY`. Empty scheme-specific values fall back to `ALL_PROXY`, while an
empty uppercase alias still masks its lowercase alias. `NO_PROXY` retains
reqwest's host/domain/IP/CIDR/wildcard matching. Setting only `NO_PROXY` preserves
the inherited proxies for other hosts. As in the current reqwest system matcher,
CGI mode (`REQUEST_METHOD` present) disables proxy routing.

No proxy keys, or clearing the session map, restores the original daemon-client
behavior. Otherwise each tool invocation selects immutable effective proxy
settings before making requests. Updates affect only new invocations, not an
in-flight response, redirects, or a search invocation's fallback engines. The
existing user agents, connection/pool settings, fetch timeout overrides, and
custom/Kagi redirect and timeout policies are preserved. Invalid proxy settings
fail closed with a redacted error, never silently retry via daemon credentials.
Reqwest's existing redirect authentication behavior is unchanged: a cross-host
plain-HTTP redirect strips `Proxy-Authorization`, although it keeps the selected
proxy route. HTTPS CONNECT authentication and same-host redirects retain the
selected proxy credentials. Proxies requiring authentication on every redirected
plain-HTTP hop may reject that cross-host hop.

Clients share connection pools by effective proxy configuration and HTTP policy,
not session ID. The process-wide LRU holds at most 32 configurations/policies.
Replacement, inheritance, clearing, and session teardown remove cached clients
whose effective configuration is no longer referenced by any live session.
In-flight client clones survive eviction and cleanup. Proxy cache debug output
and transport errors do not expose proxy credentials or retain unsafe error
source chains. The ephemeral environment registry is still not a sandbox.

Provider/model requests (including provider-native web search) and daemon-internal
updates, telemetry, usage, authentication refresh, and catalogs remain on daemon
configuration. OpenAI's raw WebSocket transport is unchanged and does not gain
HTTP proxy support here. Remote HTTP/SSE MCP clients also remain daemon-scoped.
Stdio MCP servers are shared/pool-scoped processes inheriting daemon environment,
not per-session overrides. Do not use session credentials to configure shared MCP
servers without a separate per-session transport/process isolation design.

## Session context ceilings

The harness API supports a persistent compaction ceiling for each session:

```json
{"v":1,"id":42,"req":"set_context_cap","session_id":"session-id","max_context_tokens":600000}
```

The reply reports both the normalized override and the effective budget:

```json
{"v":1,"reply_to":42,"ev":"context_cap_changed","session_id":"session-id","max_context_tokens":600000,"effective_budget":600000}
```

The effective budget is the smaller of the model's context window and the session
ceiling. If no session ceiling is set, `[compaction] max_context_tokens` supplies
the ceiling instead. A global value of `0` means the model window is uncapped.
A session override replaces the global ceiling, so it can raise or lower it, but
can never exceed the model window.

Pass `0` or `null` to clear an override and return to the current global default.
Overrides are isolated between sessions, saved with the session, and restored on
resume after a daemon restart. Older sessions without this field inherit the
global default. Model changes recalculate the budget without discarding the
override.

Changes requested during a running turn wait for the session lock without blocking
the connection's request reader. Lowering the ceiling does not interrupt the
active request or immediately rewrite the transcript. Existing compaction
thresholds apply to subsequent requests. Use `compact` to request compaction
explicitly. The ceiling is a compaction budget, not an exact outbound token limit.

Rust SDK: `client.set_context_cap(session_id, Some(600_000))`.
TypeScript SDK: `await client.setContextCap(sessionId, 600_000)`.
Both return the effective budget. Live `token_usage` events also carry an optional
`effective_budget` from the daemon, allowing clients to render a context meter
without treating the model's full window as the active compaction budget.

## Account routing

A daemon that advertises the `routing_v1` capability (harness API minor 24) carries
account routing: which credential serves a session, a spending-limit gate that
holds a turn until a person answers, human intents, and a settings view. Clients
that do not know these events ignore them.

Events, all scoped by `session_id`:

```json
{"v":1,"ev":"routing_decision","session_id":"s1","account":"claude:team","reason":"priority 1","warnings":["budget hf-month $45.00 of $50.00"],"pending":"claude:spare"}
{"v":1,"ev":"routing_gate","session_id":"s1","gate_id":"g1","reason":"claude:team reached its 5h limit","options":[{"kind":"switch","account":"claude:spare","detail":"40% used"},{"kind":"wait_until","resets_at":1900000000},{"kind":"ack","account":"claude:team"}]}
{"v":1,"ev":"routing_gate_closed","session_id":"s1","gate_id":"g1"}
```

`routing_decision` names the account for the next turn. `pending` is the account a
warm session moves to at its next cache-cold seam, so a header chip can show the
intended move. `warnings` and `pending` are omitted when empty. A gate holds the
turn until `routing_gate_answer` arrives. `routing_gate_closed` follows every
answer, from any attached client, and also an abandoned gate: close the matching
card.

Requests:

```json
{"v":1,"id":7,"req":"routing_gate_answer","session_id":"s1","gate_id":"g1","choice":{"kind":"switch","account":"claude:spare"}}
{"v":1,"id":8,"req":"routing_intent","session_id":"s1","action":{"action":"hold","account":"claude:team","minutes":90}}
{"v":1,"id":9,"req":"get_routing_view"}
```

`choice` is `switch {account}`, `wait`, `ack {minutes}` or `cancel`. An answer
replies `ok`, or an error when no such gate is waiting, for example because another
client answered first. `action` is `status`, `pin`, `switch`, `hold`, `ack`, `paid`
or `unstick`, with the same arguments as the TUI's `/acct`. An omitted `account`
clears a pin or switch. The reply is `routing_status {text}`, ready to show.

`get_routing_view` needs no attached session. It replies `routing_view {view}` with
the configured mode and paid setting, every account (kind, priority, per-class
priority, model globs, egress, `on_limit`) with its measured usage windows, every
budget with its live `used` next to `limit`, labelled API keys per provider (source,
active, whether a routing account exists, usage windows) and any configuration
problems. A meter that cannot be read is absent from the reply, never zero. The view
holds metadata only: no key or token ever appears.

Rust SDK: `client.answer_routing_gate(session_id, gate_id, choice)`,
`client.routing_intent(session_id, action)` and `client.routing_view()`.
TypeScript SDK: `await client.answerRoutingGate(...)`, `await client.routingIntent(...)`
and `await client.routingView()`. All three fail before sending a frame when the
runtime does not advertise `routing_v1`.

## Token optimization

`agentgrep` is jcode's grep tool, built for agents. Matches include file structure, such as functions and their locations, so an agent can understand the surrounding code without reading an entire file. Results are adaptively trimmed based on what the agent has already seen to reduce repeated context.

See the [agentgrep implementation](../crates/jcode-app-core/src/tool/agentgrep.rs).

## KV cache

Provider-side prompt caching lets repeated prompt prefixes reuse work from earlier requests. Jcode monitors provider-reported cache usage and compares request signatures to detect meaningful cache misses. The monitor runs inside the agent, so clients receive the same classified cache-miss events.

Cache reuse depends on the provider and the request. Monitoring makes misses visible rather than guaranteeing that every request will hit the cache.

See the [KV cache monitor](../crates/jcode-base/src/kv_cache_monitor.rs).

## Todo planning hierarchy

Define plans as **phases → lanes → items**, including plans with only one phase or lane:

```json
{
  "phases": [
    {
      "phase": 1,
      "lanes": [
        {
          "lane": "implementation",
          "items": [
            {
              "name": "Implement the change",
              "description": "Implement and validate the requested behavior",
              "status": "in_progress",
              "priority": "high",
              "confidence": "validated"
            }
          ]
        }
      ]
    }
  ]
}
```

Phases execute in declared order. Lanes within a phase can run in parallel, while items within a lane run in order. Items inherit phase and lane membership from their enclosing containers. Update existing items by `name` rather than resending the hierarchy. Names must be nonblank and globally unique across all phases and lanes. Use `description` for longer context and `rename_to` for an explicit rename, for example `{"update":[{"name":"Implement the change","rename_to":"Implement the feature"}]}`. Internal IDs are generated automatically and preserved when replacing a plan with the same task names. Legacy `id`/`content`/`details` calls remain accepted for compatibility. Flat saved todo lists remain compatible internally with storage, swarm coordination, and rendering.

A lane can optionally replace the default whole-prior-phase barrier with specific
prerequisite task names from strictly earlier phases. For example, add this phase
after the phase above:

```json
{"phase": 2, "lanes": [{"lane": "tests", "after": ["Implement the change"], "items": [
  {"name": "Test the change", "status": "pending", "priority": "high"}
]}]}
```

The tests lane can start as soon as `Implement the change` is successfully completed, even
if an unrelated lane in phase 1 is still running. Omit `after` to retain the
phase barrier. When present, `after` must contain unique, existing task names and
must not be empty. `remove`, `add.after`, and `update.lane_after` also reference
task names. Ambiguous names in legacy saved lists are rejected rather than
selecting an arbitrary task. Saved items
carry this setting as `lane_after`. Cancelled or deleted prerequisites do not
satisfy the dependency. The TUI shows concise `tests: after 1/1 · met`
progress and does not dim a prerequisite-ready lane merely because its phase is
later. This note describes the prerequisite gate, not the lane's lifecycle or
other blockers.

## Cancelling a todo

The active plan uses three statuses: `pending`, `in_progress`, and `completed`.
Cancel work you no longer intend to do with an action rather than another status:

```json
{"cancel":[{"name":"compiler-promotion","reason":"Superseded by publish-built-014"}]}
```

Cancellation removes the item from the plan and records why it was withdrawn.
It does not count as successful completion. If another lane explicitly depends
on that item, redirect its prerequisite to a replacement or explicitly clear
that dependency before cancelling (or in the same edit).

The cancellation history for a session lives at `todos/cancellations/<session-id>.json` under `JCODE_HOME`. It contains the item as it was before cancellation, the reason, and the timestamp.

When space is tight, the collapsed pinned todo view prioritizes rows with live workers before showing `+N more`. This is a display choice only. Expanding the list restores plan order, and scheduling and dependencies are unchanged.

## Pausing a todo

A todo has no "blocked" state. An unfinished todo keeps receiving automatic continuation unless one of these applies:

| Field | Level | Pauses until | Use for |
| --- | --- | --- | --- |
| `waiting_on: {ids, reason}` | todo | the listed background tasks or `await_members` waits finish | work that is already running somewhere else |
| `requires_user_auth: {reason}` | todo | the user authenticates and the field is cleared with `null` | a login, 2FA prompt, or credential the agent has no access to |
| `cannot_complete: "<reason>"` | goal | the reason is cleared with `""` | a goal that is genuinely impossible |

```json
{
  "update": [
    {"name": "Compare benchmark", "waiting_on": {"ids": ["bench-task"], "reason": "Rerun in progress"}},
    {"name": "Deploy to production", "requires_user_auth": {"reason": "Log in to the deploy dashboard"}}
  ],
  "goals": [
    {"group": "migration", "cannot_complete": "Source data was deleted upstream", "closed_feedback_loop": "absent", "feedback_loop": "None possible", "feedback_loop_relevance": "indirect", "feedback_loop_coverage": "narrow", "feedback_loop_traceability": "unmapped"}
  ]
}
```

These are conditions on unfinished work, not lifecycle statuses: keep the todo pending or in progress. A failed command, a retry, an approval, or a decision is not a reason to pause: `requires_user_auth` is only for authentication the agent cannot perform itself. `waiting_on` also accepts a bare list of IDs. Both todo pauses clear automatically when the todo is completed or cancelled.

The TUI shows the waiting reason on the todo row, marks a required login with **✋ Needs your login**, and lists a goal that cannot complete in the footer above the composer. Independent work stays actionable in every case.

The former `external_blocker` and `completely_blocked` item fields were removed. Saved todos that still carry them load as ordinary actionable todos, and a tool call that sends them is rejected with a pointer to the fields above.

## Live worker status in todo lanes

An assigned worker's todo lane shares the opened session's live status renderer: activity spinner, observed input/output tokens, streaming throughput, connection, elapsed time, and current tool intent. Missing telemetry is not guessed from context size or lifetime totals. Waiting and stopped workers show their available reason instead of a live spinner. A todo that needs the user to log in pauses only itself, without hiding independent work in the same lane.

Passive worker status is displayed with the todo lanes rather than in a second list beside the pinned todo band. Focused worker navigation and transcript controls remain available.

## Hill-climbable measurements

A hill-climbable task has a concrete objective and a repeatable way to measure progress. Instead of stopping at an assertion that the work is done, an agent can make a change, measure the result, and iterate against that feedback.

Jcode's todo assessments record the feedback loop, its coverage, how directly it tests the requested outcome, and the evidence for stopping. This distinguishes an implementation change from a validated user workflow.

See the [todo tool implementation](../crates/jcode-app-core/src/tool/todo.rs).
