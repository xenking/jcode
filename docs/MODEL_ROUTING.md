# Model routing and per-model compaction

How Jcode picks a model for each role, and how compaction thresholds follow
the active model.

## Roles and their knobs

| Role | Config key | Session override | Default when unset |
|---|---|---|---|
| Main agent | `[provider] default_model` / `default_provider` | `/model`, Ctrl+B saves | provider default |
| Swarm workers, `/subagent` | `[agents] swarm_model` (env `JCODE_SWARM_MODEL`) | `/agents swarm <model>` | inherit coordinator |
| Worker fallback chain | `[agents] swarm_model_fallbacks` | none | none |
| Hard worker allowlist | `[agents] spawn_allowed_models` (exact strings) | none | unset = unrestricted |
| Agent-chosen worker model (swarm tool `model` param) | `[agents] swarm_model_selection` + `swarm_model_allowlist` (exact strings) | none | denied |
| Worker effort | `[agents] swarm_effort` | none | `low` |
| Worker effort ceilings by model route | `[agents] swarm_effort_caps` | none | no cap |
| `/review`, autoreview | `[autoreview] model` | `/agents review <model>` | inherit |
| `/judge`, autojudge | `[autojudge] model` | `/agents judge <model>` | inherit |
| Memory extraction | `[agents] memory_model` | `/agents memory <model>` | inherit |
| Session titles | `[agents] title_model` (env `JCODE_TITLE_MODEL`) | none | `claude-haiku-5-5`, then `opencode-go:deepseek-v4.1-flash` |
| Ambient runner | `[ambient] model` | `/agents ambient <model>` | inherit |

`/agents` with no args prints defaults, session overrides and available
models. `/agents global <role> <model>` persists into config. `inherit` or
`coordinator` means "use the spawning session's model".

Resolution for a swarm spawn (`server/spawn_model_selection.rs`):

1. Explicit swarm tool `model` (only if allowed by `swarm_model_selection` /
   `swarm_model_allowlist`, and always inside `spawn_allowed_models`; else the
   call is rejected with the allowed list).
2. Session override (`/agents swarm ...`, persisted as `subagent_model`).
3. `[agents] swarm_model`; if `swarm_model_fallbacks` is non-empty and the
   coordinator's route catalog shows no *available* (logged-in) route for it,
   the first available fallback is used. Unknown catalog keeps `swarm_model`.
4. Otherwise inherit the coordinator's model, provider key and auth route.

### `spawn_allowed_models`: the hard worker boundary

`[agents] spawn_allowed_models` is a worker-only allowlist, enforced in all
three places a worker model can change:

1. **Spawn** - the server rejects any agent-requested worker model outside the
   list, and the swarm tool's `model` field is advertised with exactly those
   entries. No model given means `swarm_model`, never a picker default.
2. **Failover** - worker fallbacks are filtered to the list at spawn, and the
   retry loop re-checks it, so a stale config or an account-router choice can
   never move a worker to fable/astra/another provider's route.
3. **Session restore** - a persisted session model always resolves to a real
   model; the `unknown` placeholder is dropped instead of being saved.

An empty list keeps the legacy behaviour (`swarm_model` plus its configured
fallbacks are trusted). A user-started session is never restricted: only
spawned workers are.

## Route prefixes

A model string may carry a route prefix that pins provider and auth:
`claude:`, `claude-oauth:`, `claude-api:`, `anthropic-api:`, `openai:`,
`openai-oauth:`, `openai-api:`, `copilot:`, `gemini:`, `openrouter:`, and
catalog providers such as `opencode-go:` or `deepinfra:`.
`openai-api:` / `claude-api:` force API-key billing, `*-oauth:` force the
subscription. A bare id uses whatever route the catalog resolves first.

## Recipe: Opus main, cheap workers

```toml
[provider]
default_model = "claude-opus-5-5"

[agents]
# Cheapest logged-in option today. Swap the comment once the key exists:
swarm_model = "openai:gpt-6.1-terra"
# swarm_model = "opencode-go:deepseek-v4-flash"   # needs opencode-go key
# swarm_model = "deepinfra:deepseek-ai/DeepSeek-V4-Flash"  # needs deepinfra key
swarm_model_fallbacks = ["openai:gpt-6.1-terra", "claude:claude-sonnet-5-5"]
swarm_effort = "low"
# Optional: cap workers on this route, including after failover or /effort.
swarm_effort_caps = { "openai:gpt-6.1-sol" = "xhigh" }
memory_model = "claude-haiku-5-5"
title_model = "claude-haiku-5-5"

[autoreview]
model = "claude-haiku-5-5"

[autojudge]
model = "claude-haiku-5-5"

[ambient]
model = "gpt-6.1-terra"
```

Task-type routing (e.g. "tests on luna, refactors on terra") is guidance, not
config: put it in `.jcode/swarm-prompt.md` and enable
`swarm_model_selection = true` with an exact `swarm_model_allowlist` so the
coordinator may pass `model` per spawn:

```toml
[agents]
swarm_model_selection = true
swarm_model_allowlist = ["openai:gpt-6.1-terra", "openai:gpt-6-luna", "claude:claude-sonnet-5-5"]
```

```markdown
<!-- .jcode/swarm-prompt.md -->
- Default: omit `model` (uses agents.swarm_model).
- Mechanical edits, tests, docs: `openai:gpt-6-luna`.
- Cross-crate refactors or tricky concurrency: `claude:claude-sonnet-5-5`.
```

## Per-model / per-provider compaction

`[compaction]` is the global layer. Two optional maps merge over it for the
active session model, field by field: **model beats provider beats global**.
Every `[compaction]` key is accepted. Model keys match the bare id
case-insensitively, a route prefix on the active model is ignored. Provider
keys match the provider name (`claude`, `openai`, `copilot`, ...). Without
overrides behavior is unchanged.

```toml
[compaction]
reactive_threshold = 0.80
reactive_threshold_tokens = 400000

[compaction.providers.openai]
reactive_threshold = 0.75

[compaction.models."claude-opus-5-5"]   # 1M window
reactive_threshold_tokens = 500000
max_context_tokens = 600000

[compaction.models."gpt-6.1-terra"]
max_context_tokens = 1050000
reactive_threshold_tokens = 400000

[compaction.models."gpt-6.1-sol"]
max_context_tokens = 1050000
reactive_threshold_tokens = 400000
```

For OAuth/ChatGPT subscriptions the 400k trigger is a usage-policy choice, not
an API token-pricing cliff. API-key billing doubles above 272k input tokens, so
consider a 250k trigger for that route. The example configures a 1,050,000-token
ceiling, but the active provider catalog can report a different model window.

Overrides re-resolve whenever the session model changes (model switch,
session restore). `/compact mode` choices made at runtime survive re-resolve.
The effective values are logged on every switch:
`compaction effective: provider=... model=... reactive_threshold=... max_context_tokens=... budget=...`.
There is no env override for `[compaction]` keys. Per-session ceiling:
`/context cap <tokens>` still wins over `max_context_tokens`.

## Session titles

Titles are a tiny side call, not a turn, so they use their own chain and never
take the main model:

1. `[agents] title_model` / `JCODE_TITLE_MODEL` when set;
2. `claude-haiku-5-5`;
3. `opencode-go:deepseek-v4.1-flash`.

Each candidate is tried on a fresh provider fork until one answers, so the
title keeps working while OpenAI is paused or the first provider is down.
Generation runs after the first real user turn and is refined once at the end
of the third turn. A manual rename or bookmark label always wins, internal
turns (system reminders, scheduled tasks, poke text, swarm lane assignments)
are never titled, and answers that are not 3-6 words in the user's language
fall back to the previous title.

Existing sessions can be renamed in bulk, dry-run first:

```bash
jcode titles backfill                 # report only
jcode titles backfill --apply         # write
jcode titles backfill --sessions-dir /path/to/copy --limit 20
```
