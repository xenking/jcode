# Account routing

The one design document for how Jcode chooses which login (account) serves a
session, when it leaves it, and what it says about it. It is derived from the
code and replaces the earlier usage-limit, routing-policy and API-key-account
notes. Where a statement is about work still landing, it sits in a block marked
**Intended (in progress)**; adjust those once the code lands.

Code owners: `crates/jcode-routing-policy` (pure engine and config types),
`crates/jcode-routing-meters` (meters to readings), `crates/jcode-base/src/routing_service`
(the daemon's service), `crates/jcode-base/src/routing_view.rs` and
`routing_meters.rs`, `crates/jcode-app-core/src/server/execution/routing`
(the turn path), `src/cli/routing.rs` (`jcode routing`),
`crates/jcode-session-types/src/routing_acct.rs` (`/acct`),
`crates/jcode-config-types/src/runtime/provider.rs` (`[provider]` failover keys).

Other routing docs: [ACCOUNT_ROUTING.md](ACCOUNT_ROUTING.md) is the original
account-pool and plan-priority note (kept as written; this document wins on any
conflict). [MODEL_ROUTING.md](MODEL_ROUTING.md) is about which *model*
each role (workers, review, titles, compaction) uses, not which account.
[prompt-cache-keepalive.md](prompt-cache-keepalive.md) covers keepalive pings.

## 1. Two layers, and which one is on

| Layer | Switch | What it is |
| --- | --- | --- |
| Account pool (legacy) | always present | `account-pool.json`: membership, order and priority of logins. Used for session start, same-provider failover and cross-provider failover when the routing policy is not in charge. |
| Routing policy | `[routing] mode` | `[[routing.accounts]]`: kinds, priorities, limits, pacing, egress, budgets. A pure engine decides per turn; a daemon service applies it. |

`[routing] mode` (default **`off`**):

- `off`: the policy is ignored and costs nothing. The account pool behaves as before.
- `shadow`: every decision is computed and logged (decisions log, daemon log) but nothing is applied. The pool still rotates accounts.
- `auto`: decisions are applied. For a provider that has a `[[routing.accounts]]` entry serving the session's model the policy owns account choice and the pool rotation stays out of the way. A provider the policy does not describe stays on the pool.

Routing is **inactive for a turn** (the pool behaviour applies) when the mode is
`off`, when `[routing]` fails validation (a warning is logged; `jcode routing
status` lists every problem), or when the session's provider has no routing
account that serves its model. Routing never blocks a provider it does not
describe.

Routing only chooses **between accounts of one provider** (`anthropic` and
`claude` are the same family). Moving to another provider is a failover
concern, see section 7. API-key providers can hold several labelled keys (section 9).

## 2. Accounts

An account id is `provider:label` for a labelled login (`claude:claude-fox`,
`openai:openai-otter`, `opencode-go:second`) or a bare `provider` for a single
credential. An unlabelled `claude` or `openai` id is the API-key route of that
provider. Ids accept ASCII letters, digits and `- _ . @`.

### Kinds

| Kind | Meaning | Paid? |
| --- | --- | --- |
| `subscription` (default) | Rate windows (5h, 7d, model-scoped) that reset on their own. | no |
| `credits` | A prepaid or monthly grant spent in money or credits, with its own reset date. | yes |
| `metered` | Pay per token. No reset, nothing is lost when unused. | yes |

`credits` and `metered` are "paid": the automatic path uses them only when paid
routing is allowed (section 4).

### Priority

Lower number wins. `priority = 2` is flat. `priority = { fable = 1, default = 2 }`
is per model class: the class of the request's model is `opus`, `sonnet`,
`haiku`, `fable` or `spark` when the model id contains it, otherwise the
lowercase words of the model id joined by `-`. A class missing from the map
falls back to `default`; with no `default` the account sorts after every ranked
one. Priority is the only thing that places a paid account behind a
subscription: there is no ranking by kind, so give credits and metered accounts
a higher number than the subscriptions they back up.

### Model globs

`models = ["claude-*"]` limits an account to model ids matching a `*`-glob. An
empty list serves every model. If no account serves the requested model the
decision is a block ("no routing account serves ...").

### Priority in the legacy pool

Without routing accounts, priority comes from `account-pool.json` (`priorities`
map keyed `provider:label`, zero-based, lower first). When a key has no override
it is derived from locally stored plan metadata, never from the network:

| Account | Default priority |
| --- | --- |
| Claude `max`, `max_5x`, `max_20x`; OpenAI `pro`, `promax`, `enterprise`, `edu` | 0 (shown P1) |
| Standard and unknown plans | 1 (P2) |
| Free plans, API keys (`*-api`, key-only providers) | 2 (P3) |

Pool membership: OAuth and device logins and the Jcode subscription join by
default, API keys and `antigravity` stay manual, `excluded` beats `members`.
In `/account` select a login and press `[` (higher priority), `]` (lower) or
`Ctrl+R` (back to the plan-based default); rows are dimmed by priority, with no
numeric badge. Rotation cycles within a priority tier, starting after the current account, and
ranks a fresh healthy usage report ahead of unknown quota inside a tier.
Missing Claude plan metadata is backfilled at startup through the profile
endpoint, at most two requests at a time and at most once per account per
fifteen minutes per process. The TUI cycles saved OAuth accounts with
`Ctrl+Alt+J` / `Ctrl+Alt+K`. `jcode routing import-pool` seeds
`[[routing.accounts]]` from this file (priority, and `metered` for API keys)
without changing the mode or the pool.

## 3. What is measured

The engine never touches the network. Each decision reads a **snapshot** built
from caches (`routing_meters.rs`):

- Claude and OpenAI readings come from the per-account usage caches that `/usage`
  and the account pickers fill. OpenCode (Zen/Go) and Cursor readings are pushed
  by the code that fetches them and expire after 15 minutes. Spend ledger
  budgets read the local ledger (`spend-ledger-v1.sqlite3`), which only contains
  spend jcode itself priced.
- The snapshot is shared for 10 seconds. While routing is not `off`, the daemon
  refreshes the caches every 150 seconds; the first decision of a daemon's life
  waits up to 4 seconds for a first fetch.
- A **reading** is a window (`5h`, `7d`, `30m`, `2w`, `month`, `month@15`), a unit
  (`percent`, `usd`, `credits`, `tokens`), used, the provider's cap for non-percent
  units, the reset time and an optional model class (a window that only counts for
  one family). Windows whose length a label does not state are never given a guessed
  length. **An unreadable meter is never a zero and never disqualifies an
  account**; it shows as unknown.
- Reset grants (earned weekly or Fable resets) come from cached metadata
  only, and only unexpired, non-imported ones count.
- Per-account state learned over time (`routing-health.json`): a ring of meter
  samples per account (one sample per minute at most, kept six hours, history
  restarts when a window resets) from which the burn rate in points per hour is
  measured, and each account's consecutive-failure count and eviction deadline.
  It survives daemon reloads.
- Egress availability (`EgressDown` when a `required` proxy is unreachable) is
  layered on top, see section 8.

## 4. How an account is chosen

`decide(policy, snapshot, intents, request, now)` is a pure function. The
request carries the model, its class, the role (`interactive`, `subagent`,
`headless`), the current account, the parent session's account and a cache
context. In order:

1. **Human intents** (section 5): `pin`, then `switch`, then `hold` (interactive
   sessions only). The named account is used if it is ready and under its
   limits. A pinned account that is unavailable (evicted, egress down) **blocks**
   the turn; an unavailable switch or hold is skipped. A chosen account that
   reached a limit with `on_limit = block` blocks; otherwise it opens a **gate**
   (a person chose it, so spending past its limit is a question, not a switch).
2. **Paid allowed?** The `paid` intent first, else `[routing] paid`
   (default `auto`). With paid off, `credits` and `metered` accounts are removed
   from the automatic candidates. Pins, switches and holds can still name them.
   Candidates are the accounts that are ready and not at a limit.
3. **Rank candidates** by, in order:
   1. *room that would be lost*: accounts whose every pool is behind its pace line
      (section 6) first, soonest reset first. This is the only thing that beats
      priority, because the alternative is throwing quota away;
   2. *priority* for the request's model class;
   3. *price* λ (lower first, see section 6);
   4. the *current* account;
   5. lighter *load* (highest percent reading, known before unknown);
   6. *config order*.
4. **Child start**: a subagent's first turn starts on its parent's account if
   that is allowed and ready, so both share the cached prompt prefix.
5. **At the wall**: if the current account reached a limit, first try to redeem
   a reset grant that would expire unused; then its `on_limit` applies: `block`
   blocks, `ask` opens a gate, `switch` falls through to ranking.
6. **Use the best candidate.** If it is not the current account and the cache is
   warm, the session stays while its account can still serve and the better
   account becomes `pending`; it leaves a warm cache only when the account is
   hard-blocked (see "Cache seams"). At a cold seam the move is free. With no candidate left the decision is a gate (interactive
   sessions) or a **block** (subagents, headless), with the reached limits as the
   reason; if every account is `block` the decision is a block.

The reason string of a decision names the cause, for example `priority 2`,
`behind pace, room would be lost at the reset`, `a reset grant would expire
unused`, `claude:a outranked by claude:b (priority 1)`, `pinned to claude:a`.

### Limits (when an account counts as spent)

For each percent window the account is spent when
`used >= ceiling - hysteresis`, where

- `ceiling = adaptive_limit - subagent_reserve_pct` (subagents only; interactive: 0);
- `adaptive_limit = limit_pct`, or when the burn rate is measurable
  `limit_pct - clamp(burn_pts_per_hour * reserve_horizon_min/60, reserve_floor_pct, limit_pct)`.
  An unmeasured burn never penalizes an account;
- `hysteresis_pct` applies only to accounts other than the current one: moving
  onto an account needs that much more room than staying needs.

Non-percent readings are spent at the provider's own cap. A matching
`[[routing.budgets]]` replaces the default ceiling of a window with the same
length and unit (provider meter) or adds a ledger pool. A budget at its limit
adds a hit with its `action`; at `warn` it only adds a warning. The effective
action is the strictest of the account's `on_limit` and the budget's `action`
(`audit` records only, `switch` moves on, `ask` gates, `block` refuses). `ack`
clears `ask`/`switch` limits on an account for a while, never `block`.

### Cache seams and the option table

A session is at a **seam**: `start` (no history), `cold` (cache TTL expired,
right after a compaction or a model switch, or after a failed turn) or `warm`.
A cold seam moves for free. A warm session **stays on an account that can still
serve** and leaves only when that account is hard-blocked, or when a person pins,
switches or holds. A merely better account (higher priority, behind its pace
line, past the soft `limit_pct` ceiling) becomes `pending` and is taken at the
next start or cold seam. Hard-blocked means a percent window at 99% or more, a
provider cap reached, a `block` budget, an evicted account or a down egress
proxy.

A forced move costs a cache rewrite on the target, so it takes the cheaper of
two ways out. With `C` the cached context tokens, `S` the compacted size (a
quarter of `C` unless measured), `write`/`read` the provider's cache factors and
`λ` the prices of section 6:

| Option | Cost |
| --- | --- |
| Move | `C*write*λnew` |
| Compact, then move | `C*read*λcur + S*5*λcur + S*write*λnew` |
| Redeem a reset on the current account | free when the grant would otherwise expire unused; the cache survives |

A plain move wins a tie. Compaction is an option only when it can run: the
context is at least the manual-compaction floor (10% of the budget) and holds
more messages than the turns compaction keeps. Otherwise the forced move is a
plain move. When a planned compaction does not start the move still happens
(it was forced anyway); that is logged at info, and the move carries a warning
only if a compaction that started failed. Cache factors: Anthropic 5-minute writes 1.25x, 1-hour
writes 2x, reads 0.1x (the tier follows the session's cache TTL, 5m when at most
300 s); other providers write 1x and read at the catalog's cached-input ratio,
1x when unknown.

## 5. Intents: how a person steers

Intents live in `JCODE_HOME/routing-state.json` (atomic writes, expired entries
pruned). They may only name accounts listed in `[[routing.accounts]]`. They take
effect on the session's next turn decision, and only when routing is not `off`.

| Intent | Scope | Default span | Effect |
| --- | --- | --- | --- |
| `pin <id>` / `off` | the session and children spawned later | until cleared | always that account; blocks if it is unavailable |
| `switch <id>` / `off` | this session only | until cleared | move there; beats hold and ranking |
| `hold <id> [dur]` / `off` | all interactive sessions | 4h | prefer that account |
| `ack <id> [dur]` | the account | 30m (gate answer: up to 24h) | keep spending past `ask` limits |
| `paid auto\|off\|<dur>` | global | until changed | whether the automatic path may use paid accounts. A duration allows until then, **then the stored intent becomes `off`** (not the config value) |
| `unstick` | session + global | n/a | drops this session's switch, the hold, and every eviction (daemon only; `jcode routing unstick` drops the hold) |

Durations: `30m`, `2h`, `1h30m`, `1d`.

### Gates

A gate holds a turn at a reached limit until a person answers. Only interactive
sessions get one; subagents and headless runs block with the reason. Options
offered: `Switch` to a usable account, `CompactSwitch` (compact on the warm
account first, when that is cheaper), `RedeemReset` (a grant clears the reached
windows), `Wait` until the window resets (held at most 12 h), `Ack` on
the current account (unless it is `block`), and cancel. A gate nobody answers
fails the turn after 30 minutes; a client that disconnects fails it at once. A
turn re-decides at most four times after gate answers.

## 6. Pace-based selection

Every pool whose unused room is lost at its reset (a subscription window, a
credit grant or a ledger budget with a reset date) is *paced*: it should be
fully spent by its own reset, like a linear line from 0 % at the window start to
100 % at the reset. A pool with no known reset time is never behind pace.

A pool is **behind pace** (room would be lost) when it still has room and

- with a measured burn: `used + burn * hours_to_reset < capacity * (1 + slack)`
  (the measured burn will not use the room before the reset);
- without a burn: `used/capacity < 1 - left_share + slack`, where
  `left_share = (resets_at - now) / window_length` (clamped to 0..1; a month
  counts as 30 days).

`slack = pacing_slack_pct/100` and applies **only to the current account**: it
keeps the current account behind pace a little longer so the boundary does not
flap. An account is *behind pace* only when all of its pools are; its soonest
reset orders such accounts. The reason shown is
`behind pace, room would be lost at the reset` (or `a reset grant would expire
unused` when a grant makes the room free). Accounts that are ahead of pace fall
back to priority.

### Prices (λ)

λ is what one unit of an account's quota is worth now:

- an expiring pool: scarcity = projected demand until reset /
  room left (demand is `burn * hours_to_reset`, or without a burn
  `capacity * left_share`). Unknown reset: the used share. No room: infinite.
  Multiple pools: the largest;
- `credits`/`metered` with no pool reading: the account `weight`;
- an unmeasured subscription: 1.

### Economics rules 1-7

The code and its tests refer to these by number (`economics_tests.rs`,
`scripts/mutate_routing_economics.py` mutation-checks one assertion per rule).

1. A cold seam moves for free: session start, an expired cache TTL, right after a
   compaction, after a model switch, after a failed turn.
2. A warm session moves only when its account is hard-blocked, or when a person
   pins or switches.
3. A short throttle retries on the same account (section 7, item 2). Exhausted
   quota or a broken credential evicts the account and moves at once.
4. At the wall, in order: redeem a reset that would expire unused (the cache
   survives); a later-usable grant is offered at the gate next to `ack`, a switch,
   compact-then-switch and wait. Interactive sessions see the gate; subagents and
   headless runs block.
5. Reset grants are never wasted: the sweep redeems one ten minutes before expiry
   if that restores used quota, and until then routing counts it as free room so
   load drains there first.
6. Keepalive pings follow the ping-versus-rebuild rule in the Keepalive subsection.
7. Children start on their parent's account when it can serve them.

### Reset grants

A grant lists the windows it clears and its expiry. Rules:

1. at the wall, a grant that would expire unused is redeemed and the session
   stays (the cache survives); otherwise it is offered at a gate;
2. every ~150 s, in `auto`, grants due within **10 minutes** are redeemed if
   that restores used quota on a window they clear (the sweep);
3. a grant is claimed once per daemon, whatever the outcome, using the provider's
   own claim code and its cross-agent lock. Nothing here buys credits.

### Keepalive

Prompt-cache pings run only while a session waits on swarm workers. A ping costs
`C * read * λcur`; pinging continues while the pings expected until the wait ends
cost less than a rebuild. See
[prompt-cache-keepalive.md](prompt-cache-keepalive.md).

## 7. Failover

Several mechanisms exist; the first that applies owns a failed turn.

1. **Routing policy** (`mode = auto`, provider covered): on a limit or
   credential error the failure is fed to the health tracker and the turn is
   decided again as a cold seam, up to **3 distinct re-decisions per turn**.
   - A *definitive exhaustion* (usage/credit messages such as `usage limit
     reached`, `insufficient_quota`) or an exhausted account limit evicts the
     account at once; anything else counts toward `evict_after` consecutive
     failures (default 3). An eviction lasts `evict_for` (default `30m`; 30 min
     if `[routing]` is invalid). A success clears the streak but never shortens an
     eviction. `unstick` clears evictions.
   - If no other account can serve, the turn fails with the original error plus
     `routing has no other account to move to` (or the block reason). Routing
     does **not** fall through to another provider.
   - The provider is told to hand a limit to routing at once (fail fast) only
     while another account can serve; otherwise it keeps its own same-account
     retries.
2. **Provider recovery on the same account**: bounded retries. On Anthropic an
   ambiguous 429 / rate-limit retries on the current account (at most the usual
   three total attempts) when there is no `Retry-After` or it is at most 5
   seconds, if `anthropic_retry_recoverable_rate_limits` is on. Explicit
   exhaustion, auth failures and long delays go to account routing at once.
3. **Same-provider account failover** (the pool; `same_provider_account_failover`,
   default `true`): when routing is not in charge, another pool member of the same
   provider is tried, cycling within a priority tier. Metered API keys are not pool
   members by default, so nobody is billed silently.
4. **Cross-provider failover** (`cross_provider_failover`): the same prompt is
   resent on another provider's pool-approved route.
   - Modes: `countdown` or `manual` (aliases `off`, `false`, `disabled`, `none`
     mean manual; `auto`, `automatic` mean countdown). **The default in code is
     `countdown`**: the TUI shows `Switching to X in 3s. Esc to cancel.` and
     resends; `manual` only shows a notice. The owner confirmed this design; do
     not change the default without a new decision.
   - `exhausted_usage_immediate_failover` (default `true`): a *definitive*
     exhaustion skips the countdown; ambiguous failures keep it. `manual` always wins.
   - Desktop and SDK clients that opt in (`supports_server_failover`) get the same
     switch performed by the server, with the same pool rules (at most 3 switches,
     source and target must be pool members, `countdown` mode required). Server-run
     turns (spawned workers, wake, reload resume) always apply it.
   - While routing owns the session's provider the client never runs its own
     rotation; the user only gets the manual suggestion.
   - Nothing enrolls accounts, enables API billing, buys credits or bypasses the pool.
     If no approved route remains Jcode reports the failure.
5. **Excluded models**: `failover_excluded_models` (default `["opus"]`): case-
   insensitive substrings of the model id, `*` wildcard, matched against the model
   (never the provider name). Applies to automatic model, account, provider and
   worker failover. Explicit `/model` or `spawn model=...` still work; if nothing
   eligible remains the original turn fails. `[]` disables the exclusion.
   An auth failure may rotate credentials but never silently picks a different model.
6. **Worker model fallbacks** (`[agents] swarm_model_fallbacks`) run after the
   account router and respect the worker allowlist; see [MODEL_ROUTING.md](MODEL_ROUTING.md).

Earned resets are separate from routing: `openai_auto_banked_reset` (default
`true`), `anthropic_auto_fable_reset` (`false`), `auto_reset_only_when_no_routes`
(`true`), `auto_reset_trigger` (`blocked_wait_over`), `auto_reset_min_wait_minutes`
(10).

## 8. Egress

`[egress.<name>] url = "http://..."` (also `https://`, `socks5://`, `socks5h://`),
`required = true` (default). `[[routing.accounts]] egress = "<name>"` sends every
request of that credential (inference, token refresh, usage meters) through the
proxy, whatever the routing mode. A bare provider entry (`claude`) applies to its
logins; the exact id wins. A proxy is probed (750 ms timeout, cached 5 s).
`required = true` and unreachable: the account is unavailable (`EgressDown`) and
routing picks another, and no request leaves directly. `required = false`: requests
go direct with a warning.
## 9. API-key accounts

API-key providers hold several labelled keys the way Claude and OpenAI logins
hold several accounts: two OpenCode Go subscriptions, Hugging Face keys billed
to different organizations, several keys on one named OpenAI-compatible
provider. The ids are the routing account ids of section 2: `opencode-go` is the
unlabelled key (existing setups keep working), `opencode-go:second` a labelled
one. Providers are the built-in OpenAI-compatible profiles (`opencode`,
`opencode-go`, `huggingface`, `deepseek` and the rest of the login catalog) and
`[providers.<name>]` entries of type `openai-compatible` or `openrouter`. The
generic `openai-compatible` profile and the Muse subscription take no labels.

```sh
jcode login opencode-go --account second                  # paste or pipe the key
jcode auth keys add gateway team --api-key-env TEAM_KEY   # named provider
jcode auth keys list [PROVIDER] [--json]                  # * marks the key requests use
jcode auth keys use opencode-go second                    # from the next call on
jcode auth keys use opencode-go                           # back to the unlabelled key
jcode auth keys remove opencode-go second
```

In the TUI: `/account <provider> switch <label>`, `/account <provider> remove
<label>`, `/account list`. Keys are stored in `api-key-accounts.json` next to the
other credential files (mode 0600) and are never printed. The first labelled key
of a provider with no unlabelled key becomes active on its own; a labelled key
never replaces the unlabelled one.

**In routing and usage.** A key account takes part in routing only when listed in
`[[routing.accounts]]` (typically `kind = "metered"`, or `subscription` for a
plan such as OpenCode Go); a saved key not listed is shown as "not routed" by
`jcode routing status`. `/usage` shows every key as its own entry with its own
windows, for example `OpenCode Go (API key) [second]`. OpenCode and Cursor windows
reach routing as pushed readings (section 3). Metered spend is recorded in the
local ledger under the exact account id of the key in use (`opencode-go:second`
for a labelled key, the original `openai-compatible:opencode-go` entry for the
unlabelled one), and ledger budgets (section 4) count it.

**`key_command`** for a Keychain or password manager: an argv array with no
shell, whose trimmed stdout is the key. Cached in memory until the config
reloads, never written to disk or the log. A command that fails, times out or
prints nothing gives an error naming the account and program; Jcode never falls
back to another key. Values beginning with `!` are plain text and never executed.

```toml
[providers.gateway]
base_url = "https://gateway.example/v1"
key_command = ["security", "find-generic-password", "-s", "gateway", "-w"]
key_command_timeout_secs = 15        # default 15, clamped to 1..=120

[api_key_profiles.opencode-go.keys.vault]
key_command = ["op", "read", "op://Private/opencode/credential"]
```

**Headers and base URL.** `[api_key_profiles.<id>] headers` (and
`[providers.<name>] headers`, which also applies to OpenAI-compatible requests)
are sent with every request; `[api_key_profiles.<id>.keys.<label>] headers`
overrides them per key (`X-HF-Bill-To` for per-organization billing). An invalid
header name or value stops the request with an error naming the header, not its
value. `[api_key_profiles.<id>] base_url` points a built-in profile at a proxy.

**Price overrides.** `[api_key_profiles.<id>.costs."<model or * glob>"]` and
`[providers.<name>.costs."*"]` take `input`, `output`, `cache_read`, `cache_write`
(USD per 1M tokens); the most specific match wins and omitted categories use the
catalog price. They drive the cost shown in the TUI and the spend ledger.

## 10. What is logged where

| Where | What | When |
| --- | --- | --- |
| Chat transcript | One row `Provider: old -> new (cause)` (role `setting_change`), plus Model, Reasoning effort, Mode rows, from `setting_log` | Only when the session's provider/account/model actually changes; the request-boundary net re-checks every turn and writes nothing if nothing changed. The routing table is not a transcript item (see the Intended block in section 12) |
| Wire events | `RoutingDecision {account, reason, warnings, pending}` (drives the account chip), `RoutingGate`/`RoutingGateClosed`, `RoutingStatus {text}`, `RoutingView` | every decided turn / gate / on request |
| Decisions log | `JCODE_HOME/routing-decisions.jsonl` (rotated to `.1` at 8 MiB): `at, time, mode, session, model, role, seam, current, decision, account, reason, warnings, pending, applied` | one line per decision in `shadow` and `auto`; `decision` is `stay`, `move`, `pending`, `compact`, `redeem`, `gate` or `block`; `applied` is true only for an applied `move` |
| Daemon log | `routing[auto\|shadow]: session=.. model=.. current=.. <decision> <account> (<reason>)`; `routing: ...` warnings for invalid config, failed binds, failed or unconfirmed reset claims | same moments |
| `routing-state.json` | intents | on change |
| `routing-health.json` | burn rings, failure counts, evictions | throttled (30 s) or at once on a failure |
| `spend-ledger-v1.sqlite3` | one row per billed API-key call | per call |

The routing table (`status` text) is for **on-demand inspection only** and is not
written into the chat.

## 11. Configuration reference

Everything below is read from `config.toml`; defaults are from the `Default`
impls (`crates/jcode-routing-policy/src/config.rs`,
`crates/jcode-config-types/src/runtime/provider.rs`). An invalid `[routing]`
disables routing for the turn and is reported, it never breaks loading the rest
of the config. Retired keys still load and are named as notes.

### `[routing]`

| Key | Default | Meaning / validation |
| --- | --- | --- |
| `mode` | `"off"` | `off`, `shadow`, `auto` |
| `paid` | `"auto"` | `auto`, `off`: may the automatic path use credits/metered accounts |
| `limit_pct` | `95` | percent at which a window counts as spent (ceiling). > 0 and <= 100 |
| `subagent_reserve_pct` | `5` | subagents leave an account this many points earlier. 0..100 |
| `reserve_floor_pct` | `0` | minimum headroom the adaptive limit keeps when burn is measurable. 0..=`limit_pct` |
| `reserve_horizon_min` | `60` | minutes of projected burn the adaptive limit reserves. > 0 |
| `hysteresis_pct` | `0` | extra room a non-current account needs. 0..100 |
| `evict_after` | `3` | consecutive failures that evict an account. >= 1 |
| `evict_for` | `"30m"` | eviction length (`30m`, `5h`, `7d`, `2w`) |
| `accounts`, `budgets` | empty | see below |

### `[[routing.accounts]]`

| Key | Default | Meaning / validation |
| --- | --- | --- |
| `id` | required | `provider:label` or `provider`; unique |
| `icon` | unset | emoji shown first wherever the account is named; unset uses the animal of the label, else a stable hash pick (`jcode_session_types::account_icon`) |
| `kind` | `"subscription"` | `subscription`, `credits`, `metered` |
| `priority` | `1` | integer, or a map by model class with optional `default`; empty map is an error |
| `models` | `[]` (all) | `*`-globs of model ids this account serves |
| `egress` | unset | name of an `[egress.*]` entry that must exist |
| `on_limit` | `"switch"` | `switch`, `ask`, `block` |
| `pacing_slack_pct` | `0` | slack above the pace line for the current account. 0..100 |
| `weight` | `1.0` | multiplies the price of credits/metered accounts. > 0 |
| `drain_before_reset_h`, `drain_limit_pct`, `pacing` | retired | ignored: pools behind pace drain first on their own |

### `[[routing.budgets]]`

Exactly one of `account` (must be a routed account), `provider`, `model` (glob) sets
the scope. `name` unique and required. `meter` `provider` (default) or `ledger`;
`window` (required) one of `30m`, `5h`, `7d`, `2w`, `calendar-month`, `month@15`
(day 1..=28); `unit` `percent` (default), `usd`, `credits`, `tokens`; `warn`
(unset); `limit` (required, > 0, percent <= 100); `action` `ask` (default),
`audit`, `switch`, `block`. A ledger budget counts `usd` or `tokens` only;
`warn` must be positive and at most `limit`.

### `[egress.<name>]`

`url` (`http://`, `https://`, `socks5://`, `socks5h://`), `required` (default `true`).

### `[provider]` failover keys

| Key | Default |
| --- | --- |
| `cross_provider_failover` | `"countdown"` (3 s cancelable countdown); env `JCODE_CROSS_PROVIDER_FAILOVER` |
| `exhausted_usage_immediate_failover` | `true` |
| `same_provider_account_failover` | `true`; env `JCODE_SAME_PROVIDER_ACCOUNT_FAILOVER` |
| `anthropic_retry_recoverable_rate_limits` | `true` |
| `failover_excluded_models` | `["opus"]` |
| `openai_auto_banked_reset` | `true` |
| `anthropic_auto_fable_reset` | `false` |
| `auto_reset_only_when_no_routes` | `true` |
| `auto_reset_trigger` | `"blocked_wait_over"` (or `exhausted`) |
| `auto_reset_min_wait_minutes` | `10` |

```toml
[routing]
mode = "auto"
paid = "off"

[[routing.accounts]]
id = "claude:claude-fox"
icon = "🦊"
priority = 1

[[routing.accounts]]
id = "claude:claude-panda"
kind = "credits"
priority = 9
on_limit = "ask"

[[routing.budgets]]
name = "claude-credits-month"
account = "claude:claude-panda"
meter = "ledger"
window = "calendar-month"
unit = "usd"
warn = 90
limit = 100
action = "ask"
```

## 12. Surfaces

### CLI

- `jcode routing status [--json]`: mode, config problems and retired-key notes,
  paid state, hold, acks, every routed account (kind, priority, on-limit, models,
  egress) and saved logins that are not routed. This is the place to read the table.
- `jcode routing paid auto|off|<dur>`, `hold <id> [dur]|off`, `ack <id> [dur]`,
  `unstick`, `import-pool [--dry-run]`.
- No `pin`/`switch`: those are per session.

### `/acct` in a session (TUI and Desktop)

`/acct [status]`, `pin <id>|off`, `switch <id>|off`, `hold <id> [dur]|off`,
`ack <id> [dur]`, `paid auto|off|<dur>`, `unstick`. The daemon validates ids and
durations against `[[routing.accounts]]` and answers with text
(`RoutingStatus`). Parsing and completion are shared in
`jcode-session-types/src/routing_acct.rs`.

### Header chip and gate

The daemon sends a `RoutingDecision` per decided turn; clients show the serving
account (with its pending target and warning count) as a chip. A reached limit
opens the gate view with the options of section 5.

### Settings page

`RoutingView` (wire request `routing_view`) joins `[routing]` with the measured
windows, budgets and API-key credentials for a settings UI. It performs no
network I/O and reads no key value.

### Intended (in progress): icons, popover, account-aware picker

These behaviors are being landed by parallel work and describe the target; verify
against the code before relying on them.

- **No transcript dump.** The routing table is never written into the chat. In
  Desktop the account chip opens/toggles the **Account routing** popover with the
  same rows (running account, why, per-account kind/priority/state); in the TUI and
  CLI it is `/acct status` / `jcode routing status`. The only chat notice about a
  change is the single `setting_change` row `old -> new (cause)`, written when the
  account actually changes. Until that lands the TUI prints the
  `RoutingStatus` text of `/acct` into the transcript (that goes away) and writes a
  `Routing: now on <account> (<reason>)` system line when the chip's account changes.
- **Per-account icon.** Every account has a stable emoji, shown **first**
  wherever the account is named: model picker rows, the status-bar chip, the
  routing popover and `routing status`, the change notices, and the usage views
  (Desktop usage sidebar, TUI info widget, `jcode usage`). Source:
  `[[routing.accounts]] icon`, else `account_icon(label)`. Widths are measured in
  display cells (emoji are wide) and chrome rows must not wrap
  (`python3 scripts/check_tui_chrome.py`).
- **Account-aware model picker (TUI and Desktop).** A model route for a provider
  with several logins lists one entry per account (`account = "provider:label"`,
  an optional field on `ModelRoute`/`ModelRouteInfo`, omitted on the wire when
  absent so old peers keep working). The account the session runs on is marked.
  Selecting an account-specific route pins or switches the session to that
  account through the routing intents (`Pin`/`Switch`, the same as `/acct pin|switch`).

## 12b. Identity on provider usage endpoints

Rule (owner decision): usage, limit, profile and reset reads match the request
identity of the reference CLI for the same endpoint (recorded from its usage
readers), and nothing more.

- **Claude** (`/api/oauth/usage`, `/profile`, saved and session resets): headers
  `Accept: application/json, text/plain, */*`, `Content-Type: application/json`,
  `anthropic-beta: oauth-2025-04-20`, `User-Agent: claude-cli/<release> (external, cli)`
  and the bearer token. No `x-client-request-id`, no `x-app`. `<release>` is the
  release the reference CLI presents; `JCODE_CLAUDE_CODE_VERSION` overrides it. The
  endpoint decides per caller whether to evaluate saved resets (`ineligible_reason`
  `surface` / `cli_version`), which is why this identity is needed here. Inference
  keeps the honest `jcode/<version>` identity. Helper: `apply_claude_account_headers`.
- **ChatGPT/Codex** (`/wham/usage`): bearer token, `chatgpt-account-id`, and the
  client's own user agent (the reference CLI sends its own name, jcode sends `jcode/<version>`).
- **Cursor** (Connect RPC on `api2.cursor.sh`): bearer token, `Connect-Protocol-Version: 1`,
  JSON content type; no user-agent override.

## 13. Verify this doc

```sh
# defaults and ranges
rg -n "impl Default for (RoutingConfig|AccountConfig|BudgetConfig|EgressConfig|ProviderConfig)" -A22 \
  crates/jcode-routing-policy/src/config.rs crates/jcode-config-types/src/runtime/provider.rs
rg -n "enum CrossProviderFailoverMode" -A8 crates/jcode-config-types/src/runtime/update_policy.rs
# engine, strategy, state, economics
JUSTRUST_TEST_PARALLEL=0 justrust test -p jcode-routing-policy --lib
JUSTRUST_TEST_PARALLEL=0 justrust test -p jcode-routing-meters --lib
JUSTRUST_TEST_PARALLEL=0 justrust test -p jcode-session-types --lib -- routing_acct
JUSTRUST_TEST_PARALLEL=0 justrust test -p jcode-base --lib -- routing_service routing_view routing_meters
# live view of the policy you configured
jcode routing status --json
jcode auth keys list --json
# isolated daemon (own socket and home, fixture provider): shadow, auto, gate, block, 429 re-decision
python3 scripts/accept-routing-daemon.py <built-jcode>
python3 scripts/check_tui_chrome.py
```
