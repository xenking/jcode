# Model routing and per-model compaction

How Jcode picks a model for each role, and how compaction thresholds follow
the active model.

## Roles and their knobs

| Role | Config key | Session override | Default when unset |
|---|---|---|---|
| Main agent | `[provider] default_model` / `default_provider` | `/model`, Ctrl+B saves | provider default |
| Swarm workers, `/subagent` | `[agents] swarm_model` (env `JCODE_SWARM_MODEL`) | `/agents swarm <model>` | inherit coordinator |
| Worker fallback chain | `[agents] swarm_model_fallbacks` | none | none |
| Agent-chosen worker model (swarm tool `model` param) | `[agents] swarm_model_selection` + `swarm_model_allowlist` (exact strings) | none | denied |
| Worker effort | `[agents] swarm_effort` | none | `low` |
| `/review`, autoreview | `[autoreview] model` | `/agents review <model>` | inherit |
| `/judge`, autojudge | `[autojudge] model` | `/agents judge <model>` | inherit |
| Memory extraction | `[agents] memory_model` | `/agents memory <model>` | inherit |
| Ambient runner | `[ambient] model` | `/agents ambient <model>` | inherit |

`/agents` with no args prints defaults, session overrides and available
models. `/agents global <role> <model>` persists into config. `inherit` or
`coordinator` means "use the spawning session's model".

Resolution for a swarm spawn (`server/spawn_model_selection.rs`):

1. Explicit swarm tool `model` (only if allowed by `swarm_model_selection` +
   allowlist, else the call is rejected).
2. Session override (`/agents swarm ...`, persisted as `subagent_model`).
3. `[agents] swarm_model`; if `swarm_model_fallbacks` is non-empty and the
   coordinator's route catalog shows no *available* (logged-in) route for it,
   the first available fallback is used. Unknown catalog keeps `swarm_model`.
4. Otherwise inherit the coordinator's model, provider key and auth route.

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
memory_model = "claude-haiku-5-5"

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
consider a 250k trigger for that route. The model window is 1,050,000 tokens.

Overrides re-resolve whenever the session model changes (model switch,
session restore). `/compact mode` choices made at runtime survive re-resolve.
The effective values are logged on every switch:
`compaction effective: provider=... model=... reactive_threshold=... max_context_tokens=... budget=...`.
There is no env override for `[compaction]` keys. Per-session ceiling:
`/context cap <tokens>` still wins over `max_context_tokens`.
