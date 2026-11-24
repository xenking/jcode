# Routing policy: accounts, egress, budgets (plan)

Fork-only. Multi-account routing across providers, per-provider and per-account
egress proxies, account priorities, and usage/spend limits, all described in
`config.toml`. The daemon decides which credential serves each session.

## Design decisions

| Question | Decision |
| --- | --- |
| Where routing runs | Natively in the daemon. One process sees every session, so intents, meter samples and the spend ledger have one owner. |
| Limit reached | Configurable per account and per budget: `switch`, `ask` or `block`. Default: ask in an interactive session through an inline view like `ask`, block with a reason in headless sessions and subagents. |
| Egress proxy down | Per proxy. `required = true` fails closed (the account is unavailable and routing picks the next), `false` goes direct. |
| Config format | `[routing]`, `[egress.*]`, `[[routing.accounts]]`, `[[routing.budgets]]` in `config.toml`. Mutable state lives in a daemon store. `account-pool.json` is migrated and deleted. |
| Providers covered | Claude subscriptions and API credits, OpenAI subscriptions and Codex credits, Cursor dollar pool, OpenCode Go (several keys), Hugging Face Inference Providers billed to an organization, named OpenAI-compatible providers. |

## Behaviour

- **Account kinds.** `subscription` (rate windows such as 5h and 7d), `credits` (a prepaid or monthly grant), `metered` (pay per token).
- **Priority.** A number, or a map by model class (`{ fable = 1, default = 2 }`), so sessions on different model families can prefer different accounts. Lower wins. Accounts in one priority group are load balanced.
- **Model globs.** An account serves only the models it lists, so routing never sends a model to an account that cannot run it.
- **Model-scoped windows.** A per-family weekly window only counts for that family.
- **Adaptive limit.** Leave an account when what is left no longer covers the measured burn rate over a horizon, bounded by a ceiling and a floor. Unknown rate falls back to the ceiling.
- **Drain before reset.** A subscription whose weekly window resets soon, with room the measured burn will not use, goes ahead of priority.
- **Month-line pacing.** A credits account is used while its spend is behind the elapsed fraction of the calendar month, with hysteresis so the boundary does not flap.
- **Subagent reserve.** Subagents leave an account a few points earlier than the interactive session.
- **Paid intent.** `auto` (pacing and last-resort fallback), `off` (paid accounts only by explicit choice), `allow <duration>`.
- **Intents.** `pin` (session and its children), `switch` (this session), `hold` (all interactive sessions until expiry), `unstick`, `ack` (keep spending here for a while).
- **Health eviction.** N consecutive failures evict an account for a duration. A `required` egress that is down evicts its accounts.
- **Unreadable meters never disqualify** and are shown as unknown, never as zero.
- **Cache seams.** Moves happen at session start and at cache-cold seams. Warm triggers only report a pending move.
- **Grant health.** Idle OAuth accounts get a periodic refresh probe, and an approaching login expiry warns ahead of time.

## Data model

```toml
[egress.corp]
url = "http://127.0.0.1:8899"
required = true
probe = "tcp"

[routing]
mode = "auto"                   # auto | shadow (log decisions, act on none) | off
paid = "off"                    # auto | off
subagent_reserve_pct = 5

[[routing.accounts]]
id = "claude:team"              # provider:label
kind = "subscription"
priority = { fable = 1, default = 2 }
models = ["claude-*"]
egress = "corp"
on_limit = "switch"             # switch | ask | block

[[routing.accounts]]
id = "opencode-go:second"
kind = "subscription"
priority = 3

[[routing.accounts]]
id = "huggingface"
kind = "metered"
priority = 5

[[routing.budgets]]
name = "hf-month"               # stable; ledger usage is recorded under it
provider = "huggingface"        # exactly one of account, provider, model (glob)
meter = "ledger"                # provider (live meter) | ledger (local priced usage)
window = "calendar-month"       # 30m | 5h | 7d | 2w | calendar-month
unit = "usd"                    # percent | usd | credits | tokens
warn = 90
limit = 100
action = "ask"                  # audit | switch | ask | block
```

Parse-time validation reports every problem at once and never breaks loading the
rest of the config: unknown egress or account, duplicate ids or budget names, a
budget with zero or several scopes, a ledger budget in percent or credits, percent
above 100, `warn` above `limit`.

The effective action at a limit is the strictest of the account's `on_limit` and
the budget's `action`. `ack` clears `ask` limits for a while, never `block`.
Phase 6 adds `drain_before_reset_h`, `pacing = "month-line"`, hysteresis and
eviction settings. Phase 3 adds `key_command`.

## Engine

A pure crate, `jcode-routing-policy`:

```text
decide(policy, meters, intents, request { model, role, session }, now) -> Decision
Decision = Use { account, reason } | Gate { options, reason } | Block { reason }
```

Order: `pin`, `switch`, `hold`, drain candidates, subscriptions by priority for the
request's model class below their adaptive limit, credits behind their month line,
credits as last resort if `paid` allows, then the account's `on_limit`. Evicted
accounts are skipped.

## What exists today

- Multi-account OAuth for Claude and OpenAI (`auth/account_pool.rs`), one priority per account, failover within a tier (`provider/account_failover.rs`).
- Usage windows for Claude, OpenAI and OpenCode Go (`usage/`). Local spend per API-key source (`provider_activity.rs`).
- Named OpenAI-compatible providers with custom `headers`.
- Missing: per-provider and per-account proxy, several keys per API-key provider, budgets and gates, per-model-class priority, model globs, key from a command, per-model cost overrides on named providers.

## Phases

Each phase ends in a check that runs on fixtures.

1. **Types and engine.** Done: `crates/jcode-routing-policy` (raw config, validated `Policy`, pure `decide`), `[routing]` and `[egress.*]` on `Config`. Check: `justrust test -p jcode-routing-policy`.
2. **Egress.** Named proxies, one shared client per egress, assignment per provider and account, fail-closed, probe. Check: a loopback CONNECT proxy fixture sees only the assigned account's requests, and a required account is skipped while the proxy is down.
3. **Several credentials per API-key provider.** Labelled keys for OpenCode Go, Hugging Face and named providers, `key_command`, per-model cost and compat overrides. Check: two OpenCode Go fixture keys with different `/usage` payloads both appear in `/account` and `/usage`.
4. **Meters, ledger, budgets.** One meter view over provider windows (Claude, OpenAI, OpenCode, Codex credits, Cursor pool) and the local priced ledger. Check: fixture spend crosses `warn`, then `limit`, and the verdict changes.
5. **Engine in the daemon.** Account choice at session start and seams, failover through the engine, gate view in the TUI and headless block, `/acct` and `jcode acct status|paid|prefer|pin|unstick --json`, account chip in the header, `account-pool.json` migration. Check: isolated daemon with fixture providers and a driven gate view.
6. **Strategies.** Adaptive limit, drain, month-line pacing, subagent reserve, health eviction. Check: time-stepped engine tests on recorded meter series.
7. **Shadow rollout.** `routing.mode = "shadow"` logs the decision per turn without acting, then `auto`.

## Open items

- Which endpoints report the Cursor pool and Codex credits.
- A per-pane config overlay, so one pane can run a different role-model profile.
- Burn-rate samples must survive daemon reload.
