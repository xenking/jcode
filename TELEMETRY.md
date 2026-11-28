# jcode Telemetry

**This fork disables telemetry entirely.** It collects nothing and sends nothing
to any telemetry endpoint. There is no opt-in, no opt-out, and nothing to
configure.

- `crates/jcode-telemetry-core` is a no-op facade kept only so upstream call
  sites compile. `is_enabled()` always returns `false`; every `record_*`
  function does nothing. The crate has no state, files, identifiers, counters,
  threads, network code or endpoint URLs.
- Provider OAuth account emails are never read for upload; the upload code has
  been deleted.
- `scripts/install.sh` no longer reports installer funnel events or stores an
  install conversion id.
- No first-run telemetry notice is shown. `/telemetry` and the onboarding
  "Telemetry" page only state that telemetry is disabled in this build.
- `jcode telemetry status` reports `disabled`; `jcode telemetry enable` fails.
  `jcode telemetry disable` only clears any leftover local settings.

The `telemetry-worker/` directory is the upstream server and is not used by this
fork. The upstream policy (what the original project collects) is available in
the upstream repository history of this file.
