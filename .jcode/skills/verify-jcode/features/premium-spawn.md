# Premium swarm worker approval

Launch the private TUI and daemon with a synthetic Astra model:

```bash
"$V" launch --binary "$BINARY" --evidence "$E" --premium-spawn
"$V" drive --evidence "$E" premium-spawn
"$V" cleanup --evidence "$E"
```

The fixture sets a private swarm default and caps `xhigh` at `high`. The drive submits a real swarm tool turn twice. Before each answer, the TUI must show the exact model and capped effort, without a worker or an Astra request. Enter denies the first request. Down and Enter allow the second. The loopback provider must receive exactly one startup request at `high`, and the worker's reply must be persisted.

The HTTP provider is fake. This drive does not use or validate vendor credentials, upstream reasoning behavior, Fable, or a deployed daemon. App-core tests also cover Fable on a todo lane, durable replay, and revoking an approved route with the allowlist.
