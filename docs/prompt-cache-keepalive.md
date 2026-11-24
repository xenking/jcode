# Prompt cache keepalive

The desktop footer and Prompt cache card estimate remaining retention from the
latest observed provider usage that read or wrote a cache on the active request
route. The daemon schedules a ping using the Anthropic request it actually sent
and the timestamp and token counts from Anthropic's completed stream. It does
not use the UI countdown or an estimate of context length. HTTP/SSE heartbeat
events keep transport alive, not the provider's prompt cache.

```toml
[cache]
keepalive = true                # Set to false to disable all cache pings
allow_5m = false                # First-party Anthropic 5m tier, opt-in
allow_human_wait = false        # Pending direct ask, opt-in
min_cached_tokens = 100000
max_pings_per_wait = 1
max_daily_usd_micros = 10000000  # $10 per session, reset at UTC midnight
```

Default eligibility is first-party Anthropic Messages on the 1-hour tier with
known standard prices. The session must be in an active `bg wait`, a validated
`rollout_wait`, or parked on a pending `swarm await_members` with internal
auto-wake. Merely running a background job, receiving notifications, or having
no active turn does not qualify. A known wake deadline before expiry does not
benefit from a ping. Checks start inside the last 5 minutes of a 1-hour window
or the last minute of an opted-in 5-minute window, but never within 15 seconds
of expiry.

The replay keeps system blocks, tool definitions, messages, cache breakpoints,
thinking settings, effort, endpoint and credential unchanged. It requests
`max_tokens: 0` and `stream: false`. Incompatible shapes such as fixed-budget
extended thinking, assistant prefill, Fast, gateways and unknown prices are
skipped. Replay data is held only in memory and discarded across daemon exec or
session transfer. It is never added to history or the session's context usage.

A ping pays for a read of the cached prefix plus ordinary input rates on any
uncached tail. It must cost less than the assumed chance of returning during the
extra window (50% for automatic waits, 10% for opted-in human waits) times the
avoided rewrite premium over a read. For an Opus 5.5 prefix of 900,000 tokens
with no tail, the list-price read is $0.18 and a 1-hour write is $7.20. The
50% estimate makes the avoided cost $3.51. These probabilities are policy
assumptions, not measured predictions.

Anthropic can still charge for a rewrite if the cache unexpectedly misses. The
daily limit reserves the estimated full rewrite plus tail before dispatch, then
settles it from reported tokens. An ambiguous response keeps the reservation and
is not retried. Only a confirmed full hit advances the countdown or the card's
“Kept warm” count. The card's dollar amount uses public rates, not an invoice
or an estimate of remaining subscription quota.

OpenAI is not pinged. Earlier automatic caching has variable retention, while
newer Responses models document a 30-minute read refresh, not a restart of a
12-hour window. The Anthropic replay and pricing contract does not apply there.

- [Claude prompt caching and zero-token pre-warming](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
- [Claude public prices](https://platform.claude.com/docs/en/about-claude/pricing)
- [OpenAI prompt caching and refresh behavior](https://developers.openai.com/api/docs/guides/prompt-caching)
