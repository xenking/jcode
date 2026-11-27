# Desktop state

Each daemon keeps the state of the Desktop windows that use it: what is open,
where focus was, unsent drafts. The state lives in the machine's own daemon, so
it does not depend on a Desktop running there, and any Desktop attached to that
machine (locally, over SSH or through a paired gateway) reads and writes the
same document. Preferences, credentials and anything configured in jcode stay
where they are: nothing in this document is a setting.

Capability: `desktop_state_v1`, advertised in the daemon's `pong` and in the
harness API `hello_ok`.

## Model

A daemon holds *spaces* (at most 16, named `[A-Za-z0-9._-]{1,64}`). A space has
a revision and a set of keyed entries. Every successful put bumps the revision;
each entry remembers the revision at which it last changed.

| Key | Value |
| --- | --- |
| `layout` | rows of `{panel, width}` slots, any number of rows |
| `focus` | the row and panel where the user last was |
| `panel/<id>` | `{kind: session\|compose, session_id?, title?}` |
| `draft/<panel id>` | `{text, cursor, model?, reasoning_effort?, attachments[]}` |

Every value carries `v: 1`. The daemon stores each value by deserializing it
into the types in `jcode-session-types/src/desktop_state.rs` and serializing it
back, with unknown fields denied. A field that is not listed (launch
environment or arguments, tokens, working directories, a `paired://` or other
routing-prefixed session id) makes the put fail with reason `invalid`; it is
never dropped silently and never stored.

Limits: 4 MiB per entry, 16 MiB and 256 live entries per space, 64 changes per
put, 20 MiB per request line. Exceeding one fails the put with `too_large`.

## Protocol

A Desktop state connection is dedicated: its first frame is a `desktop_state_*`
request, it never attaches a session, and it serves only these requests. A
session connection that sends one gets an error.

| Daemon request | Harness API request | Answer |
| --- | --- | --- |
| `desktop_state_get` | `desktop_state_get` | `desktop_state` (space, rev, all live entries) |
| `desktop_state_subscribe` | `desktop_state_subscribe` | `desktop_state`, then `desktop_state_changed` pushes |
| `desktop_state_put` | `desktop_state_put` | `desktop_state_applied`, `desktop_state_conflict` or `desktop_state_rejected` |

`desktop_state_put` carries `{space, device, expected_rev, changes[]}`. A
change is `{key, value}`; a missing `value` deletes the entry.

- **Conflicts are per key.** A put is refused only if one of the keys it writes
  changed after `expected_rev`. The answer is `desktop_state_conflict` with the
  current revision and every entry that changed since `expected_rev` (deleted
  entries appear with no value), so the client merges and retries on the new
  head. Puts on other keys apply even when `expected_rev` is behind.
- **Deletions are remembered** for 1024 revisions. A writer older than that gets
  a conflict with `full: true` and every live entry, and must resync.
- **A put that changes nothing** (same value, or deleting an absent key) leaves
  the revision alone and wakes nobody.
- **Pushes** go to every other connection subscribed to the space, never to the
  writer, which already has its answer. `device` is the writer's label. A
  subscriber that fell behind receives an unsolicited `desktop_state` (no
  request id) and replaces what it holds.

## Storage and durability

`$JCODE_HOME/desktop-state-v1.sqlite3`, mode 0600 (SQLite, WAL), the same kind
of store as the model usage and session metadata ledgers. Every put is one
immediate transaction, so concurrent writers in the daemon and in API bridge
processes serialize on the revision check, and limits are enforced inside it.
The file survives daemon restart and rollout exec; subscribers reconnect and
read a snapshot. Drafts are stored as plaintext.

## Version skew

A client checks `desktop_state_v1` in `hello_ok` (or `pong`). A daemon without
it makes the bridge answer `unknown_request` with "Update Jcode on that
machine"; Desktop shows that and keeps working without sync. Desktop never
restarts a remote daemon.

## Not yet done

- Desktop client: one `Space` per machine, push/pull, draft lease. See the
  Desktop repository's remote-workspaces plan.
- One connection per machine for all session panels. Today a paired Desktop
  opens one gateway WebSocket per open session, and the gateway admits 16
  concurrent harness sockets by default (`[gateway] max_harness_connections`,
  1 to 256), so the Desktop state connection takes one of
  them. Multiplexing needs a channel id on harness frames, a bridge that holds
  one daemon connection per channel, a `multiplex_v1` capability and the
  matching change in the SDK's paired transport and Desktop, so it cannot ship
  from the daemon side alone. Until then raise `max_harness_connections` if a machine
  needs more open sessions.
