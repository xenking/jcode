# jcode-base layering plan

Why this exists: `deslop-please` reports a 27-module `module-cycle` and a 52-module `tangle` rooted in `jcode_base::auth`. Both are one knot. This note fixes the layers first so each cut can be judged against them.

Measured on `crates/jcode-base/src` with a `crate::<module>` import scan (non-test code). It finds 36 modules in one strongly connected component, more than deslop counts because deslop follows items, not paths. The shape is the same.

## The knot

Hubs by edges into and out of the component:

| module | in | out | role |
| --- | --- | --- | --- |
| `provider_catalog` | 240 | 45 | static provider facts and credential lookup |
| `provider` | 144 | 318 | runtime routing, HTTP client, model routes |
| `auth` | 128 | 218 | credentials, accounts, login flows |
| `config` | 148 | 6 | parsed `config.toml` |
| `usage` | 38 | 114 | quota and spend meters |
| `session` | 41 | 49 | persisted sessions |
| `sidecar` | 7 | 51 | background model calls |

`config` has only 6 outgoing edges. Everything else in the component either feeds these hubs or hangs off them.

## Layers (low to high)

1. **L0 facts.** `config`, `provider_catalog` facts, `message`, `todo`, `model_pricing`, `egress` client construction. Depend on nothing above.
2. **L1 credentials.** `auth` (accounts, trust, refresh, login). Depends on L0 only.
3. **L2 routing.** `provider`, `usage`, `subscription_*`, `provider_activity`, `routing_meters`. Depend on L0 and L1.
4. **L3 workflows.** `session`, `sidecar`, `memory*`, `compaction`, `prompt`, `skill`, `import`, `bus` consumers.

A module may import downward, never upward. Today the upward edges are:

| upward edge | where | proposed cut |
| --- | --- | --- |
| `config` -> `provider::populate_context_limits_from_config*` | `config.rs:364,445` | provider registers a reload hook with config at provider init |
| `config` -> `auth::external_trust::forget_memo` | `config/config_file.rs:177` | auth registers its memo invalidation as a reload hook |
| `config` -> `auth::account_pool::sync_order_with_default_route` | `config.rs:427`, `config_file.rs:234` | the caller that saves the default route does the sync, not `Config::save` |
| `message` -> `provider::is_anthropic_reasoning_provider` | `message.rs:336` | move the predicate to `jcode-message-types` or `provider_catalog` facts |
| `bus` -> `provider::{ModelRoute, catalog_scheduler}` | `bus.rs:764` | `ModelRoute` is a plain type: move to L0. `bump_catalog_generation` becomes a bus event the provider reacts to |
| `companion_client` -> `auth::running_in_test_harness` | `companion_client.rs:282` | move the predicate to `env_facts` (no auth dependency) |
| `auth` -> `egress::client`, `companion_client::open_url`, `memory_rerank::clear_failure_backoff` | `auth/flow/oauth.rs`, `auth/mod.rs:879` | take an injected HTTP client and URL opener at the login entry point, and drop the rerank call into a reload hook |
| `egress` -> `provider_catalog::active_api_key_account` | `egress/mod.rs:378` | pass the account label in, do not look it up |
| `subscription_catalog` -> `auth::lifecycle`, `provider::ModelRoute` | | follows the `ModelRoute` move |

## Order of work

1. Move the small L0 predicates and types (`is_anthropic_reasoning_provider`, `running_in_test_harness`, `ModelRoute`). These are pure moves with no runtime ordering risk.
2. Add the config reload hook registry and invert the four `config` edges. This needs a startup ordering check: the first `config()` load must see the provider hook already registered, or context limits are not populated for the first session. Test with `verify-jcode` `cli` and `tui`, not only unit tests.
3. Inject HTTP client and URL opener into `auth` login flows.
4. Once `config`, `message`, `bus`, `egress`, `companion_client` leave the component, `auth` and `provider` are the remaining two-way pair (`auth` -> `provider` 55 edges, `provider` -> `auth` 62). Split by owner: credential *lookup* stays in `auth`, credential *use* stays in `provider`. The `AccountPool` and `AuthStatus` caches (the `tangle` allowlist entry) belong to `auth` and should expose a narrow read API so `provider` stops reaching into their fields.

## Why this was not done in one pass

- Step 2 changes startup behaviour and needs runtime proof.
- Step 4 touches 117 call sites in `provider` and `auth`, which are owned by other lanes and change daily.
- The measured knot is 36 modules. A partial cut that leaves one cycle edge does not change the lint result, so each step above is only worth landing together with the ones it depends on.

The allowlist entries for the cycle and the tangle stay until step 4 lands.
