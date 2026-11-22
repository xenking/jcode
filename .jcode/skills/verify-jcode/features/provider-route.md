# Provider route and authentication boundaries

A user sees the selected provider and model, manages saved accounts, and sends a turn through the selected route.

## Sub-features

- ROUTE-LOCAL: the fixture uses the selected OpenAI-compatible endpoint and fake model.
- ROUTE-STATUS: the TUI shows the selected provider and model.
- AUTH-VENDOR: real vendor login, account switching, token refresh, revocation, and credential permissions.

## How to get to it (user POV)

Use `--provider` and `--model` for a CLI turn. Use `/model` in the TUI to choose a discovered route. Use `/model refresh` to request a fresh catalog. `/account` opens account management. `/login` starts a provider login flow.

## Driving it with verify-jcode

Preconditions: Launch and doctor passed. Only the synthetic fixture key exists in the private environment.

- **Selected route:** run `"$V" drive --evidence "$E" cli --mode json`. Require `provider` equal to `openai-compatible`, `model` equal to `fake-model`, a matching fresh request in `provider.jsonl`, and the completed saved conversation. Inspect the TUI frame for the same model.
- **Model entry:** run `"$V" drive --evidence "$E" input --text "/model" --expect "fake-model"`, then capture a frame. This is exploratory evidence. A single-route fixture can select its only route without leaving a picker open. It does not prove multi-route selection.

## Gotchas

- The fake provider accepts the synthetic key. A successful fixture turn does not prove real vendor authentication.
- Real login requires a disposable vendor account, explicit authorization for its external login flow, and a separate credential-isolated run. Record the attempted route and unmet prerequisites. Do not import credentials from the developer home.
- Forced catalog refresh, last-good catalog retention, expired-token refresh races, account switching, OAuth callback validation, and secret-file modes need dedicated fixtures. They are unverified by the baseline drives.
- Capture only synthetic credential values. Never retain real tokens, callback codes, or authorization headers in evidence.

## Source entry points

- `src/cli/args.rs` defines provider and model selection.
- `crates/jcode-tui/src/tui/app/remote/key_handling.rs` handles `/model` and explicit catalog refresh.
- `crates/jcode-tui/src/tui/app/auth_account_commands.rs` handles account management.
- `crates/jcode-base/src/auth` and the provider runtime crates own vendor credential behavior.

## Driving native OpenAI compaction

Use the standalone journey with a build from before the fix and a build from
the current checkout. No `launch`, tmux, or vendor credentials are needed.

```bash
"$V" native-compaction --baseline "$OLD_BINARY" --binary "$BINARY" --evidence "$E"
```

The isolated `openai-api` route calls a local Responses fixture. The journey
checks the outgoing `compact_threshold`, reads the saved meter after restart,
reattaches without inference, and checks the live meter and next saved policy.
It retains the requests, meter payloads, CLI replies, and cleanup receipt.
It does not contact OpenAI OAuth or test server-side compaction execution.

For the manual action forecast, run `python3 .jcode/skills/verify-jcode/scripts/context_action_forecast.py "$BINARY"`.
The probe opens a private blank session, checks the read-only forecast, and then
checks that Shake preview finds nothing and manual Compact refuses to start.
It uses a dummy loopback endpoint and does not touch the shared daemon.
