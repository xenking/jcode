# Session journal recovery limits

The snapshot and append-only journal form one persisted session. Full load, remote startup and the transcript-free startup stub must agree on the selected route.

## Covered recovery

`crates/jcode-base/src/session_tests/journal_recovery.rs` checks corrupt UTF-8, an interrupted checkpoint with stale message records, and equal or backwards journal timestamps with newly appended message IDs. Startup parses message IDs only, not message content, and shares the stale-prefix classifier with the full loader.

For metadata-only records without message IDs, equal timestamps are retained. File order determines which retained record wins. A wall clock timestamp is not a durable checkpoint position.

## Known ambiguity: metadata-only checkpoint with a backwards clock

A checkpoint writes the snapshot before deleting the old journal. If it is interrupted between those operations and the snapshot timestamp moved backwards, the remaining metadata-only records can overwrite the newer snapshot. Their file positions cannot distinguish them from records appended after that snapshot. The legacy format has no checkpoint watermark.

The ignored regression `metadata_only_stale_checkpoint_with_rollback_does_not_revert_snapshot` preserves this defect. Run it explicitly:

```sh
justrust test --target-dir "$PWD/target/audit-tests" -p jcode-base --lib metadata_only_stale_checkpoint_with_rollback_does_not_revert_snapshot -- --ignored --nocapture
```

It currently fails because the loaded model is `new-model` from the old journal instead of `checkpoint-model` from the newer snapshot. It is not part of claimed passing coverage. Metadata-only records written after a clock rollback can also be mistaken for stale records.

## Follow-up design

Add an optional generation or monotonic sequence to snapshots and journal records with backward-compatible `serde(default)` readers. A snapshot must persist the exact journal head included in that checkpoint. Readers skip covered records using that watermark, not wall time. Legacy files without the marker retain an explicit fallback until rewritten.

The marker alone is insufficient. Independent server owners and transcript-free clients can save the same session from different in-memory heads. Allocating the next sequence only from local state creates collisions. The disk-head read, sequence allocation, append, snapshot replacement and journal retirement need a per-session cross-process lock or an equivalent compare-and-swap protocol. Reject stale destructive checkpoints instead of replacing newer durable history. Do not silently merge incompatible metadata.

Acceptance should model and execute two writers, interrupted snapshot-before-unlink, metadata-only records, a backwards clock, replay idempotence and retry after a write failure. A formal model is appropriate before changing this persistence protocol.
