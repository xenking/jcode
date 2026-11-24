# Routing policy: accounts, egress, budgets (plan)

Fork-only. Goal: jcode replaces the work laptop's omp setup (`xenking/pi-o-my-work`)
without losing any routing behaviour, and the features stay general: any user can
describe accounts, proxies and spend limits in config, nothing names the employer.
Work-specific values live in a separate private repo, `xenking/jcode-work`.

## Decisions (owner, 2026-11-24)

| Question | Decision |
| --- | --- |
| Where routing runs | Natively in the jcode daemon. agentgateway is a source of concepts, not a dependency. |
| Limit reached | Configurable per account and budget. Default as omp: ask in an interactive session (own inline view, like `ask`), block with a reason in headless and subagents. Automatic switch is a per-account setting. |
| Egress proxy down | Per proxy: `required = true` fails closed (account unavailable, routing picks the next), `false` goes direct. |
| Non-routing customizations | After routing: typed tools to MCP in `work-connectors`, gates to `pre_tool` hooks, skills copied as they are. |
| Work config | New private repo `jcode-work`. |
| Upstream | Fork-only. Record in `docs/FORK_LEDGER.md`. |
| Config format | `[routing]`, `[egress.*]`, `[[routing.accounts]]`, `[[routing.budgets]]` in `config.toml`. Mutable state in a daemon store. `account-pool.json` is migrated and deleted. |
| Scope of smart spending | Claude subscriptions and credits, OpenAI/Codex credits, Cursor dollar pool, OpenCode Go (several keys), Hugging Face resource group. OpenCode Zen is out. |

## What omp work does today

Read from `pi-o-my-work` at `49fca80`.

| omp piece | Behaviour to keep |
| --- | --- |
| `acct-policy.ts` | Slots `sub`/`credits`; priority per model class (`{ fable = 1, default = 2 }`); model globs per account; adaptive limit from measured burn rate (`threshold`, `reserveFloorPct`, `reserveHorizonMin`); drain a subscription whose weekly window resets within `drainHorizonHours`; credits paced against the calendar-month line with slack; hysteresis on moving in; subagents stop `weeklyFloorSubagents` points earlier; gate at `gateAt` with switch/wait/ack options built from live meters; intents `hold`, `pin`, `switch`, `unstick`, `ack`, `paid auto/off/allow <dur>`; grant probe and login TTL warning; moves only at a cache-cold seam; account chip in the status line. |
| `account-egress` patch | `PI_PROXY_ORGS=<orgId>=<proxy>`: only corp-allowlisted orgs go through tinyproxy over VPN. |
| `acct-policy-fallback` patch | A gated session hands off through the native model fallback chain (opus to sol, sonnet to terra). |
| `codex-gate.ts` | Monthly Codex credit pool, warn/limit, meter may be unreadable (shown as `?`, never zero). |
| `hf-gate.ts` | Local dollar ledger for `huggingface/*`, calendar month, warn $450, limit $500. HF bills the resource group and shows no usage to the member. |
| `models.yml` HF block | `X-HF-Bill-To` header, key from Keychain, per-model cost and compat overrides. |
| `config.d/cursor.yml` | Cursor $600/month pool as the manual post-gate profile. |
| `omp-credits`, `omp-acct` | CLI status and intents from outside a session, SwiftBar output. |
| review-session profile | Per-role models for review panes (orchestrator, subagent cap, consultant). |

## What jcode has

- Multi-account OAuth for Claude and OpenAI only (`auth/account_pool.rs`). One priority per account, failover within a tier on exhaustion (`provider/account_failover.rs`).
- Usage windows for Claude, OpenAI and OpenCode Go (`usage/`, `usage/api_keys.rs`). Local spend per API-key source (`provider_activity.rs`).
- Named OpenAI-compatible providers with `headers`, so the HF bill-to header already works.
- Cross-provider failover (`cross_provider_failover`), swarm allowlists and fallbacks.
- Missing: proxy per provider or account (only global `HTTPS_PROXY`), several keys per API-key provider, budgets and gates, per-model-class priority, model globs per account, key from a command, per-model cost override on named providers.

## Borrowed from agentgateway

- Priority groups: ordered groups, load balancing inside a group (`ai.groups[].providers`).
- Health eviction: N consecutive unhealthy responses evict a backend for a duration, then it returns with a restored score.
- Budgets: USD or tokens, fixed windows, `Audit` or `Block`, charged after the response from reported usage.
- Token buckets keyed by an expression (per account, per model).

## Data model

```toml
[egress.corp]
url = "http://127.0.0.1:8899"
required = true                 # fail closed while the VPN is down
probe = "tcp"                   # health check before use

[routing]
mode = "auto"                   # auto | shadow (log decisions, act on none) | off
paid = "off"                    # default paid intent: auto | off
switch_at = "cold-seam"         # never move a warm session mid-cache
subagent_reserve_pct = 5

[[routing.accounts]]
id = "claude:team"              # provider:label, same key as the account pool
kind = "subscription"           # subscription | credits | metered
priority = { fable = 1, default = 2 }
models = ["claude-*"]
egress = "corp"
on_limit = "switch"             # switch | ask | block
drain_before_reset_h = 24

[[routing.accounts]]
id = "opencode-go:a"            # several keys per API-key provider
kind = "subscription"
priority = 3

[[routing.accounts]]
id = "huggingface"
kind = "metered"
priority = 5
key_command = ["security", "find-generic-password", "-s", "jcode-huggingface", "-w"]

[[routing.budgets]]
scope = "huggingface"           # account id, provider:*, or model glob
meter = "ledger"                # provider (live meter) | ledger (local priced usage)
window = "calendar-month"       # 5h | 7d | calendar-month | rolling:<dur>
unit = "usd"                    # percent | usd | credits | tokens
warn = 450
limit = 500
action = "ask"                  # ask | block | audit
pacing = "none"                 # none | month-line (credits)
```

Types live in `jcode-config-types`. Illegal combinations are rejected at parse:
`pacing = "month-line"` needs `unit` in `usd | credits`, `meter = "provider"` needs a
provider that reports that unit, `egress` must name a defined proxy.

## Engine

A pure crate, `jcode-routing-policy`:

```text
decide(policy, meters, intents, request { model, role, session }, now) -> Decision
Decision = Use { account, reason } | Gate { options, reason } | Block { reason }
```

Ordering, highest first: `pin` (session and children), `switch` (session only),
`hold` (all interactive sessions until expiry), drain-before-reset candidates,
subscriptions by priority for the request's model class below their adaptive limit,
credits below their month line, then credits as last resort if `paid` allows,
then the account's `on_limit`. Unreadable meters never disqualify an account.
Evicted accounts (health or `required` egress down) are skipped.

The daemon owns state: intents, meter samples for burn rate, ledger, health scores.
One process sees every session, so omp's shared `acct-policy.state.json` races go away.
The decision is applied at session start and at cache-cold seams. Warm triggers only
evaluate and report a pending move.

## Phases

Each phase ends in a check that runs without the work laptop.

1. **Types and engine.** Config types with parse-time validation, `jcode-routing-policy` crate, table tests ported from `research/acct-policy/acct-fallback.test.ts` and `acct-policy.test.ts` scenarios. Check: `justrust test -p jcode-routing-policy`.
2. **Egress.** Named proxies, one shared `reqwest::Client` per egress, assignment per provider, named provider and account, `required` fail-closed, TCP probe. Check: loopback CONNECT proxy fixture sees only the assigned account's requests; with the proxy down a required account is skipped.
3. **Several credentials per API-key provider.** `opencode-go`, Hugging Face and named providers get labelled keys like OAuth accounts, `key_command` source, per-model cost and compat overrides. Check: two OpenCode Go fixture keys with different `/usage` payloads both appear in `/account` and `/usage`.
4. **Meters, ledger, budgets.** One meter view over provider windows (Claude, OpenAI, OpenCode, Codex credits, Cursor pool) and the local priced ledger. Budget evaluation with warn/limit/action. Check: HF fixture spend crosses `warn` then `limit` and the verdict changes.
5. **Engine in the daemon.** Account choice for new sessions and seams, failover through the engine instead of tier rotation, gates (inline TUI view, headless block), intents via `/acct` and `jcode acct status|paid|prefer|pin|unstick --json`, account chip in the header. Migrate `account-pool.json` priorities and delete the old path. Check: isolated daemon with fixture providers, `/verify-jcode` drive of the gate view.
6. **Strategies.** Burn-rate adaptive limit, drain before weekly reset, month-line pacing with hysteresis, subagent reserve, health eviction. Check: time-stepped engine tests on recorded meter series.
7. **`jcode-work` repo.** Work config (three Claude accounts, Codex credits, Cursor pool, HF resource group, OpenCode Go keys, oMLX local), review-session profile, skills, rules, agents, installer. Then xdev-bindings as MCP in `work-connectors`, slack-send-gate and review-ask-guard as `pre_tool` hooks, gastown hooks.
8. **Shadow and cutover.** `routing.mode = "shadow"` on the work laptop next to omp: log jcode's decision per turn and compare with acct-policy's. Switch to `auto` after a clean week, then retire the omp overlay.

## Open items

- Cursor pool and Codex credit meters: confirm which endpoints report them in jcode today (omp read Codex credits through `codexbar`).
- Profiles: jcode needs a per-pane config overlay for the review-session profile (omp used `PI_CONFIG_FILES`).
- Burn-rate samples must survive daemon reload.
