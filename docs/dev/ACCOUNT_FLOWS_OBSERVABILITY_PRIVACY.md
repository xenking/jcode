# Account Flows: Observability, Privacy, and Support Diagnostics

> **xenking fork:** telemetry is disabled entirely and the OAuth email upload has been removed (see `TELEMETRY.md`). Telemetry statements below describe what this fork does; backend sections describe the subscription service, not code in this repo.

Security logging, audit events, metrics, traces, alerting, redaction,
retention, and support diagnostics for the six subscription account flows:
(1) device login, (2) browser approval/denial, (3) account state (`/v1/me`),
(4) checkout, (5) billing portal, (6) webhooks/revocation.

Companion to `docs/dev/ACCOUNT_CONTRACT_CONFORMANCE_TESTS.md`. Client-side
surfaces live in this repo; backend surfaces live in `solosystems-backend`.

## Existing surfaces (grounding)

- Client telemetry: `crates/jcode-telemetry-core/src/lib.rs` is a stateless
  no-op facade (see `TELEMETRY.md`). It records nothing: no counters, ids or
  files, and no auth/onboarding event schema (no `AuthEvent`, no account id or
  email slot).
- Failure classification labels: `crates/jcode-base/src/auth/status/login_diagnostics.rs`
  (`AuthFailureReason::label`) — the only failure detail that may be reported.
- Local logs: `~/.jcode/logs/jcode-YYYY-MM-DD.log`.
- Backend telemetry store: `telemetry-worker/` (D1; subscription analytics
  columns from migration `0016_web_subscription_analytics.sql`).
- Public privacy contract: `TELEMETRY.md` (this fork disables telemetry
  entirely; nothing is collected or sent, and there is no opt-in or opt-out).

## Never-log list (both repos, enforced by tests)

These values must never appear in logs, telemetry, traces, crash reports, or
support bundles, at any log level:

- `api_key` (JCODE_API_KEY) — full or truncated beyond `jc_...last4`.
- `device_code`, magic-link tokens, approval URL query params.
- Raw email addresses in app logs, traces, telemetry or support bundles. This
  fork uploads none; backend audit email remains confined to the audit table.
- Stripe secrets: webhook signing secret, customer payment details, full
  checkout/portal session URLs (they embed session secrets).
- Env-file contents (`jcode-subscription` env file) and `Authorization`
  headers in any HTTP client debug logging.

Enforcement: SN-03 conformance test greps captured login output; add a
CI grep over `record_*` call sites asserting only `AuthFailureReason::label`
values flow into the facade, should telemetry ever be re-enabled.

## Audit events (backend, durable)

Append-only audit table keyed by `account_id`, retained 400 days:

| Event | Required fields |
|---|---|
| `device_auth.requested` | account_id?, email_hash, ip_hash, user_agent class, device_code_id (opaque id, not the code) |
| `device_auth.approved` / `denied` | device_code_id, approver session id, reason |
| `device_auth.expired` | device_code_id |
| `key.issued` | key_id (not the key), tier |
| `key.revoked` | key_id, actor (user/portal/admin/system), reason |
| `checkout.started` / `completed` / `abandoned` | stripe session id, tier target |
| `portal.opened`, `subscription.updated` / `canceled` | stripe ids, old->new tier/status |
| `webhook.received` / `applied` / `rejected` | stripe event id, type, signature result, dedup outcome |
| `me.tier_changed` | old, new, cause (webhook/admin) |

Correlation requirements: every audit row carries `account_id`,
`request_id` (per HTTP request), and `flow_id` (one device-login attempt or
one checkout attempt). `device_code_id` links flows 1-2; `stripe_event_id`
links 4-6. The client sends no correlation IDs today; if added, use a random
per-attempt UUID only, never a persistent client identifier joined with account_id.

## Client telemetry (this repo)

None. `crates/jcode-telemetry-core` is a stateless no-op facade (see
`TELEMETRY.md`): `is_enabled()` is always `false` and every `record_*` function
does nothing. The crate has no counters, install id, files, threads or network
code, so nothing about an account flow is recorded or sent.

The login code still calls the facade so upstream merges stay clean (for
example `record_auth_success("jcode-subscription", "device_code_browser")` at the
end of `src/cli/login/jcode_device.rs`, and `record_auth_failed_reason(...)` with
`AuthFailureReason::label` values in `src/cli/login.rs`). These calls are inert.
There is no `AuthEvent` type and no `account_id` slot anywhere in the client;
backend audit owns subscription account-linked events.

### Provider OAuth email

Provider OAuth account emails are never read for upload; the upload code, the
first-run notice and the opt-in/opt-out switches were deleted in this fork. The
`/v1/oauth-account/check` and `/v1/oauth-account` endpoints, the `oauth_accounts`
table and migration `0030_oauth_accounts.sql` exist only in the unused upstream
`telemetry-worker/`. Never transmit OAuth tokens or other authentication secrets.

## Metrics and alerting (backend)

Metrics (per flow, labeled by outcome and tier where applicable):

- `device_auth_requests_total`, `device_auth_approvals_total`,
  `device_auth_denials_total`, `device_auth_expiries_total`;
  alert: approval rate < 50% over 1h, or expiry rate > 40% (email delivery
  problem), or request spike > 10x baseline (enumeration/abuse).
- `token_poll_requests_total{result}`; alert on `slow_down` ratio > 20%
  (client misbehavior or attack) and on unexpected-5xx ratio > 1%.
- `me_requests_total{status}`; alert on 401 spike (mass revocation bug) and
  p99 latency > 2s (client timeout is 5s: `ME_FETCH_TIMEOUT`,
  `crates/jcode-base/src/subscription_api.rs:14`).
- `webhook_events_total{type, outcome=applied|duplicate|rejected}` and
  `webhook_apply_lag_seconds` (Stripe `created` -> state applied);
  alert: rejected signatures > 0 sustained, lag p95 > 60s (this is the
  RV-01/CK-02 conformance bound), any DLQ depth > 0 for 15m.
- `key_revocations_total{actor}`; alert on system/admin bulk revocations.

Client-side: none. This fork emits no client telemetry, so there is no login
success ratio or `auth_failure_reason` distribution to dashboard; failure mix
is only visible in local logs and backend audit events.

## Traces

Backend: one trace per request; parent span per flow_id so a device-login
attempt shows request -> email send -> approval -> token issue as linked
spans. Span attributes limited to the audit-field set (ids and hashes, never
secrets/emails). Stripe webhook handling gets a span per event with
`stripe_event_id` and dedup decision.

Client: no distributed tracing for auth (privacy). Local log lines around the
login flow use the daily log with the flow outcome only.

## Retention

| Data | Where | Retention |
|---|---|---|
| Audit events | backend | 400 days, append-only |
| Backend app logs | backend | 30 days |
| Traces | backend | 7-14 days |
| Stripe webhook payload archive | backend (encrypted) | 90 days |
| Client usage telemetry | none | this fork collects and stores nothing (`TELEMETRY.md`); `telemetry-worker/` is unused upstream code |
| Local client logs | `~/.jcode/logs/` | user-owned; must satisfy never-log list |
| Support bundles | created on demand | delete after case close (<= 90 days) |

## Support diagnostics

`jcode doctor`-style account diagnostics (extend existing provider doctor,
`crates/jcode-provider-doctor/`): prints auth_base, masked key
(`jc_...last4`), account email as stored locally, cached tier
(`subscription_catalog::cached_tier`), last `/v1/me` status, env-file path
and permissions. Copy-safe by construction (never full key). Support asks
the user for: masked key id, approximate login time, and the
`AuthFailureReason` label; backend support joins on `key_id`/`email_hash` in
the audit table. No flow requires the user to paste a key or link.

## Per-flow one-page summary

1. Device login: client logs outcome label only; backend audits request/
   approve/expire with device_code_id; metric+alert on approval/expiry rates.
2. Browser approval/denial: backend-only; audit approver session, alert on
   denial spikes; page must not log the magic token (only its hash).
3. `/v1/me`: backend request logs with account_id + request_id; client caches
   tier and logs nothing sensitive; alert on 401 spikes and latency.
4. Checkout: audit started/completed/abandoned via Stripe ids; funnel metric;
   never log payment details (Stripe owns them).
5. Portal: audit opened + resulting subscription changes; portal URLs are
   secrets (never logged).
6. Webhooks/revocation: audit every event with signature + dedup outcome;
   lag and DLQ alerts; revocation audit rows are the support source of truth.

## Follow-ups (actionable in this repo)

- Add SN-03-style output-capture test asserting the never-log list for the
  login flow, and a doctor command for account diagnostics.
