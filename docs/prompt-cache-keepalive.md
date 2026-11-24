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
min_cached_tokens = 100000
max_daily_usd_micros = 10000000  # $10 per session, reset at UTC midnight
```

Default eligibility is first-party Anthropic Messages on the 1-hour tier with
known standard prices. Pings run only while the session is parked on a pending
`swarm await_members` with internal auto-wake, waiting on its workers. A
blocking `bg wait`, `rollout_wait` or `ask` never pings: the turn that resumes
after them is cheap to rebuild or soon enough not to need it. Checks start
inside the last 5 minutes of a 1-hour window or the last minute of an opted-in
5-minute window, but never within 15 seconds of expiry.

Each ping follows rule 6 of the routing economics
([ROUTING_POLICY_PLAN.md](ROUTING_POLICY_PLAN.md), "Economics"):

- A ping costs a read of the cached prefix plus ordinary input rates on any
  uncached tail. A rebuild costs a cache write plus the tail. Pinging continues
  while the pings needed until the wait's deadline cost less than one rebuild.
  A wake before expiry needs no ping.
- On an account with free room (quota that would expire unused), pings cost
  nothing and always run.
- When routing has a pending move for the session that is worth its cache miss
  (the move costs less than staying warm with the pings it takes), pinging
  stops. The cache goes cold and the move happens at the next turn for free.

With reads at 0.1x and 1-hour writes at 2x the input price and no tail, up to
19 pings (about 17 hours of waiting) cost less than one rebuild.

The replay keeps system blocks, tool definitions, messages, cache breakpoints,
thinking settings, effort, endpoint and credential unchanged. It requests
`max_tokens: 0` and `stream: false`. Incompatible shapes such as fixed-budget
extended thinking, assistant prefill, Fast, gateways and unknown prices are
skipped. Replay data is held only in memory and discarded across daemon exec or
session transfer. It is never added to history or the session's context usage.

Anthropic can still charge for a rewrite if the cache unexpectedly misses. The
daily limit reserves the estimated full rewrite plus tail before dispatch, then
settles it from reported tokens, also on free room. An ambiguous response keeps
the reservation and is not retried. Only a confirmed full hit advances the
countdown or the card's "Kept warm" count. The card's dollar amount uses public
rates, not an invoice or an estimate of remaining subscription quota.

OpenAI is not pinged. Earlier automatic caching has variable retention, while
newer Responses models document a 30-minute read refresh, not a restart of a
12-hour window. The Anthropic replay and pricing contract does not apply there.

- [Claude prompt caching and zero-token pre-warming](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
- [Claude public prices](https://platform.claude.com/docs/en/about-claude/pricing)
- [OpenAI prompt caching and refresh behavior](https://developers.openai.com/api/docs/guides/prompt-caching)
