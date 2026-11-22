# File tool options

Local text `read` reports a range past EOF separately from an empty file. A complete-file marker appears only when the returned window includes every line and no line was clipped. For files larger than 1 MiB the reader stops after the requested window when possible. It returns a continuation instead of scanning the suffix to report an exact count. Invalid UTF-8 in an unvisited suffix is not diagnosed until a read reaches it. A range at the end of a large file still scans its prefix to locate the requested line.

## Version-checked line anchors (experimental)

Enable in the configuration used by the daemon, then open a new session:

```toml
[tools]
line_anchors = true
```

A direct `read` or `agentgrep(mode="grep", path="one/file.rs", query="...")` can return an `edit anchor` next to a complete matching line. Copy the anchor into `edit` with one replacement and use the entire displayed row as `old_string`. This selects the correct occurrence even when identical lines appear elsewhere. Omitting `anchor` retains normal exact-replacement semantics.

```json
{"file_path":"one/file.rs","anchor":"<copy the returned L… token>","edits":[{"old_string":"    enabled: false,","new_string":"    enabled: true,"}]}
```

The token is bound to the session, canonical path, file contents, line number and daemon instance. Under the normal edit lock a stale token fails without writing. Get a new one after a change or restart. An anchor is not a permission grant. Nested `batch`/`codemode`, directory search, paths-only output, files over 1 MiB and lines longer than 512 bytes do not issue anchors. At most eight candidates are checked per result. Do not expect anchors on shortened search excerpts, images, PDF extraction or other transformed data.

Runtime verification: use `verify-jcode launch --line-anchors` and `drive ... line-anchors`. The journey changes the second of two identical CRLF rows using a search result, retries with the stale token, verifies the unchanged file and obtains a fresh token from `read`.
