# Daemon-owned reload recovery acceptance

Date: 2026-11-17 (+11:00). Scope: candidate 1, incident #4.

## Contract

The daemon retains each interrupted session's durable obligation until a continuation input or answered-ask receipt is committed. Admission uses the stable reload ID, session ID and logical continuation UUID, with the concrete runtime/turn identity recorded per attempt. An exclusive turn lease and publication pin prevent concurrent continuations. A crash before the receipt retries the same logical continuation. A crash after the receipt reconciles the journal rather than appending a duplicate input.

Stop and a new user message persist cancelled/superseded outcomes and retire an outstanding recovery reservation before ordinary input admission. These authoritative outcomes never expire into a resurrected legacy recovery marker. Desktop and TUI display recovery state only. Legacy reminder submissions are deduplicated, not used as admission identities.

An interrupted unanswered ask remains pending while no interactive client exists. On attach the original ask tool call is presented again. Only a durable answer admits further model execution. Declining the card cancels the intent.

## Isolation and oracle

`tests/e2e/reload_recovery_owner.py` uses a fresh HOME, JCODE_HOME, TMPDIR, runtime directory and Unix socket per scenario, and a loopback fake OpenAI-compatible inference provider. It performs a real exec reload and pins the exact daemon/bridge artifacts for the complete suite. It never opens the installed user's socket or changes `/Applications/Jcode.app`.

The attached Desktop case executes the real `jcode-desktop-harness` worker through the actual SDK and API bridge. The negotiated bounded attach requests `recent_messages=0`. No client submits a continuation. Successful rows require exactly one new canonical `TURN_START` identity plus exactly one continuation inference, not matching reminder text. Crash recovery also requires the same continuation UUID and at least two attempts.

## Runtime matrix

| Requirement | Runtime check | Result |
| --- | --- | --- |
| a | interrupted attached session, bridge and Desktop harness, zero-message attach | PASS |
| b | interrupted session with no client at all | PASS |
| c | two attached clients, one continuation | PASS |
| d1 | Stop during claimed recovery, no continuation | PASS |
| d2 | new user prompt supersedes recovery and starts only user input | PASS |
| e | kill after durable claim before input receipt, restart, same UUID once | PASS |
| ask | no client: no model turn; attach: original card; answer: one continuation | PASS |
| ask cancellation | decline original card, durable cancelled outcome, no model continuation | PASS |
| ask supersession | a new user prompt replaces the unanswered question without answering it | PASS |

The complete nine-row suite passed together at 2026-11-16 15:28 UTC against implementation revision `19dece0d7` with canonical admission `1dc1be8f6`. The pinned daemon SHA-256 was `311baa0fb04a20655b4c41c8643e4b58a5b71a8cf1954f88ccdfe8f35c72250a` (task `795681om78`). The crash case recorded two attempts but only one continuation turn.

The independent combined bash/ask runtime trace on integrated revision `1c2a1bdc6` also passed: `InTool` and `AskBlocked` were captured, durable checkpoints cleared every outstanding obligation, and the replacement process reached socket readiness after actual exec. Evidence: `turn-activity-acceptance-1794842970.json` in the session scratch directory (task `92791619i3`).

## Red-first evidence

- Bounded empty attachment history hid explicit recovery before the early bridge fix, and the replacement oracle failed before the production edit.
- The old real daemon did not continue a detached interrupted session.
- Desktop and TUI initially submitted their own automatic recovery messages. Replacement display-only tests failed before the edits.
- The real Desktop path exposed swallowed daemon `turn_started` events. A red bridge identity test now pins their visible running status, including stale-event rejection.
- A real new-prompt race persisted Superseded but rejected the replacement input as busy. Cancellation and reservation-retirement synchronization fixed the actual runtime case.
- A long-offline regression proved cancellation tombstones were removed after seven days. Authoritative outcomes now remain durable.

- A forced initiator reload can precede the interrupted ask result checkpoint. The real pinned runtime exposed a dangling original ask call that ordinary repair would turn into an auto-continuation. Missing-result asks now remain pending and preserve the original question identity.
- A new user prompt previously replayed the old unanswered card. A red regression now pins supersession for both missing-result and explicitly interrupted asks.
- The actual ActiveTurnGuard resets its interruption flag before returning from the model loop. A real guard-order regression proved the post-loop flag check lost checkpoint acknowledgements. Completion now uses the canonical outstanding checkpoint obligation, verifies the migration lease, persists successfully, then acknowledges.

## Gates

- App core: 51 recovery tests passed, 2 ignored.
- Bridge: all 241 library tests passed, 1 ignored.
- TUI: 77 reload tests passed.
- Desktop harness: 8 recovery tests passed on the integrated runtime-generation head.
- Strict `clippy --all-targets -- -D warnings`: app-core, TUI, bridge, Desktop harness and Desktop model passed. The integrated core/TUI/bridge gate passed on `1c2a1bdc6` at 15:30 UTC (task `992189vhe5`).
- Rust incremental query corruption was bypassed with `CARGO_INCREMENTAL=0`. justrust repaired inconsistent native dependency artifacts before the successful strict gates.

## Reproduction

```sh
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
df -h /
justrust build --profile selfdev -p jcode -p jcode-harness-api-server --bin jcode --bin jcode-harness-api-bridge
python3 tests/e2e/reload_recovery_owner.py ~/.jcode/shared-build/target/selfdev/jcode
```

First build the Desktop harness test binary with the ignored real runtime entrypoint present. `JCODE_RECOVERY_DESKTOP_TEST_BIN` can pin its path explicitly. The Python suite otherwise discovers the newest matching harness test executable in the shared target or a justrust slot.

## Landed changes

Core: `4615b3068` (early bridge fix), `e6b5284a4` (durable daemon owner), `55bf36561` (visible daemon turn lifecycle), `e859ed62b` (display-only TUI), `00ecdeff7` (cancelled reservation retirement), `c2e2088b6` (durable explicit outcomes and checkpoint acknowledgements), `fa21336da` (uncheckpointed unanswered asks), `19dece0d7` (canonical checkpoint acknowledgement after guard reset and explicit prompt supersession).

Desktop: `52c61da` (strict lint blocker), `4215e97` (display-only harness and real runtime entrypoint).
