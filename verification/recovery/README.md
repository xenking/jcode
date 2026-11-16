# Startup recovery ownership

An orphan worker can keep editing a worktree after its coordinator has stopped. This check asks whether startup recovery admits that worker. It does not reproduce the user's particular reload incident or prove that a file write occurred.

Run `python3 verification/recovery/check.py` from the repository root. Run it with `--mutation-test` to validate the checker on a scratch break. Read results with `/statechart` through the project skill `/verify-recovery-ownership`.

The [findings](findings.md) record the checked paths and the real-server replays.

## Why this target

Reload recovery crosses durable membership, a current reload marker, session publication, owner liveness checks, and turn admission. An owner can stop between awaits. Existing startup tests cover a stopped owner before selection and durable continuation after reload, but not a stop after the final ownership read. This gap is more relevant to orphan workers than a local parser or formatting invariant.

TLC explores the bounded orders of ownership read, stop, and launch. The checker uses TLA+ rather than an input fuzzer because the question is about event ordering. TLC 1.7.4 is pinned by SHA-256 in `check.py`. Java is an execution dependency, not a proof dependency.

## Rules

| Rule | Promise | Harm when broken |
| --- | --- | --- |
| `HistoricalEvidenceCannotAuthorize` | An old reload note without membership in `live_before_restart` cannot authorize unfinished work by itself. | A historical worker resumes on an unrelated restart. |
| `StopAfterReadPreventsLaunch` | Stopping the coordinator after the final owner read prevents a new continuation from starting. | A stop appears successful, but work begins afterward. |
| `ContinuationHasOwner` | A resumed worker has a coordinator accepted as live at this restart. | Recovery starts work nobody currently owns. |

`ContinuationHasOwner` is a requested safety contract, not a claim about current intended behavior. `startup_recovery_honors_pending_intent_for_stopped_worker` intentionally permits its violation when a matching current reload intent exists. Treat this as a policy gap when discussing fixes, not as an accidental branch to delete blindly. `running_before_restart` also independently authorizes an interrupted worker.

`TypeOK` checks the model's state shape. It is not a product property.

## Source connection

`check.py` extracts `coordinator_was_live_at_restart` and the continuation-decision expressions directly from `server.rs`. A dependency-free Rust crate compiles them through `justrust`. Field-compatible fixtures supply the same `Instant`, `String`, `HashSet`, and option operations. Its results become `LiveOwners` and `ResumeCases` in the generated `RecoveryPolicy.tla`. `source-map.json` records the exact snippets and source hash.

The exporter fails when its source anchors disappear or its fixtures no longer compile. It does not prove equivalence of an arbitrary refactor. In particular, moving the owner read or adding a later admission guard requires updating the TLA+ actions after inspecting the actual control flow.

| Action or input | Production boundary |
| --- | --- |
| `selected` | A headless recovery candidate with a session that loads successfully. |
| `ReadOwner` | The final member read and `worker_has_live_coordinator` call in `recover_headless_sessions_on_startup`, before `has_stored_recovery_intent`. The members lock protects this read, not the subsequent continuation. |
| `StopCoordinator` | The owner status mutation under the members lock. Descendant cancellation runs first and finds no turn, so it stutters in this model. The replay invokes both functions in the order used by `cancel_processing_message`. |
| `DecideContinuation` | After the agent lock reads interruption evidence, `should_resume` uses the cached owner result. |
| `ClaimTurn` | `claim_server_turn` under the agent mutex. |
| `PublishRunning` | `update_member_status` changes the worker to running under the members lock. |
| `StartContinuation` | `tokio::spawn` dispatches the continuation after successful persistence. |
| `SkipContinuation` | `should_resume` is false. |
| `reload` owner | Persisted ready coordinator, normalized to stopped, in `live_before_restart`, with the fresh recovery note at the load boundary. This evidence is permitted even before its client reconnects. |
| `historical` owner | Same note and timestamp but absent from `live_before_restart`. |
| `missing` owner | Owner resolution finds no member. |
| `stopped` owner | Owner changes to stopped after the load boundary. |

## Bounds and assumptions

- One headless worker, one resolved owner, one restart, at most one coordinator stop, and one admission attempt.
- Twelve initial cases combine three owner states and two Boolean inputs. Candidate selection is overapproximated, including inert cases that the server would skip before the final read. Each reported start path must also satisfy real selection, as the runtime replays demonstrate. TLC checks each invariant separately so an early violation cannot hide another rule.
- Every fixture has an open lane and an interrupted-session marker. The Rust expressions still gate those inputs on owner evidence or `running_before_restart`.
- A current intent is pending and matches the current reload ID. Its absence means there is no intent. Mismatched or delivered intents are outside this model.
- Registry setup, session load, persistence, and turn claim succeed. No second agent competes for the same session lease.
- Owner resolution, candidate selection, and persistence normalization are reviewed assumptions backed by the existing startup tests. They are not generated from the full async recovery function.
- The model admits a stop after the final owner read and before the continuation decision, matching the runtime replay's mutex gate. Registry, persistence, and channel work do not change the modeled ownership facts. Stops after the decision, and cancellation of an already running continuation, are outside this scope.
- The model verifies safety in this scope. There is no fairness or eventual-progress claim.

## Reproduce and maintain

The project skill contains the real-server test commands and the last source sync commit. Generated artifacts include plain TLC logs per property, the source policy, and machine-readable results. A model pass alone does not establish product correctness.

Ordinary runs exit 1 while the documented ownership findings remain. Mutation-test mode exits 0 only when the real-source guard removal adds the historical-evidence violation and untouched source returns to its original findings. Errors exit 2. This distinction keeps a successful sensitivity test from being mistaken for a clean product result.
