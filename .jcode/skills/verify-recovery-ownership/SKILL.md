---
name: verify-recovery-ownership
description: Check worker ownership when changing server reload recovery, coordinator liveness evidence, or startup continuation admission. Use for orphan workers after reload, stop/recovery races, and changes to recover_headless_sessions_on_startup.
---

# Verify recovery ownership

Keep a worker from resuming after its coordinator stops or disappears. The current implementation does not satisfy that whole contract. Preserve the findings until the product behavior changes deliberately.

## Run the check

1. Read `verification/recovery/README.md` for the modeled boundaries and the known findings.
2. Compare the covered code below with sync commit `0ada82a27f0928e275e01d6e5cd2beb05d1eebb2`. Confirm that the final owner read, intervening awaits, turn claim, and continuation dispatch still match the model. The exporter follows expressions, not control-flow moves.
3. Run from the repository root.

   ```sh
   python3 verification/recovery/check.py
   ```

   Python 3.9+, `justrust`, a Rust 2024 toolchain, and Java 11+ must be available. The first run downloads checksum-pinned TLC 1.7.4. Artifacts go outside the checkout. Exit 0 means all properties hold in the bounded model. Exit 1 means a property failed. Exit 2 means the check did not complete. The recorded baseline exits 1, not 0.
4. Read every TLA+ result with `/statechart`. Use the exported `RecoveryOwnership.tla`, its config, generated `RecoveryPolicy.tla`, and `view.mjs` from the printed artifact folder. Follow that skill's current invocation instructions. Show each shortest violation path in the code's terms. Its chart hides only unchanged terminal self-loops.
5. Replay the stop race against the actual server function.

   ```sh
   justrust test -p jcode-app-core --lib --no-default-features formal_recovery_ -- --ignored --nocapture --test-threads=1
   justrust test -p jcode-app-core --lib --no-default-features startup_recovery_ -- --test-threads=1
   ```

   The first command runs intentionally ignored assertions for both unsatisfied ownership rules. The second includes the existing test that deliberately permits a current reload intent to resume work for a stopped owner. A compiler error or timeout is not a reproduced violation.

## Prove sensitivity after an update

```sh
python3 verification/recovery/check.py --mutation-test
```

This command copies the real `server.rs` into scratch storage, replaces its `live_before_restart.contains` guard with `true`, exports that Rust policy, and requires an additional `HistoricalEvidenceCannotAuthorize` violation. It then exports the untouched source again and requires the original findings. Exit 0 validates the mutation experiment, not the product contract. It leaves the checkout untouched.

If an intended change removes the known failures, update the model's scheduling steps and runtime replay. If a property fails only in the model, treat the model as wrong until its steps reproduce in the server. Keep the property when the server really violates it. Move the sync commit forward only after inspecting all covered functions and rerunning both the model and runtime checks. Use `/maintain-formal-verification-skill` after covered changes that bypassed this skill.

## Coverage record

Product code confirmed against `0ada82a27f0928e275e01d6e5cd2beb05d1eebb2` on 2026-11-16.

| Source | Covered responsibility |
| --- | --- |
| `crates/jcode-app-core/src/server.rs`, `coordinator_was_live_at_restart` | Historical membership, recovery note, and timestamp evidence. The exporter compiles this function verbatim with field-compatible fixtures. |
| Same file, `worker_has_live_coordinator` | Resolves the owner before the modeled read. Resolution is a reviewed assumption, not an exhaustively checked graph. |
| Same file, `recover_headless_sessions_on_startup` | Final owner read, `has_stored_recovery_intent`, `open_lane`, interruption guard, `should_resume`, and admission before `claim_server_turn`. |
| `crates/jcode-app-core/src/server/swarm_persistence.rs`, `recover_member_status`, `load_runtime_state` | Supplies normalized statuses and `live_before_restart`. Represented by bounded fixtures. |
| `crates/jcode-app-core/src/server/swarm.rs`, `cancel_spawned_descendant_turns` | A stop before a turn exists finds no turn to cancel. Exercised by the real replay, not by modeling the cancel registry. |
| `crates/jcode-app-core/src/server/startup_recovery_tests.rs` | Actual startup admission, durable-intent bypass, and the ignored stop-race assertion. |

The model covers one worker and one resolved coordinator across one restart. It does not prove Rust memory safety, child-process cleanup, filesystem writes, session transfer, multiple-worker ownership, eventual completion, or every recovery status.
