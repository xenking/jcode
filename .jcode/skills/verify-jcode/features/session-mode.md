# Session mode (`/mode`)

User POV: `/mode <skill>` replaces the session's pinned skills. `/mode toggle <skill>` adds or removes one without changing the others. `/mode` lists the selection and `/mode off` clears it. A pinned skill stays active across later turns, compaction and swarm workers. `skill_manage load` reads instructions without pinning them.

Run `drive mode-set` to pin two fixture skills through the terminal, restart the private daemon, check the saved set and the continued provider request, then toggle one off.

## Drive

`"$V" drive --evidence "$E" mode`

1. `/mode verify-probe`, frame shows `Session mode: verify-probe`.
2. Five `[e2e:pad]` turns grow real context past the manual compaction threshold.
3. `/compact`, frame shows `compacted`.
4. A fresh turn. Its provider request must contain `Previous Conversation Summary` and `VERIFY_PROBE_SKILL_BODY`.
5. `[e2e:spawn]` makes the model call the real `swarm` tool. The worker's first provider request must contain `VERIFY_PROBE_SKILL_BODY`.
6. `/mode off`, frame shows `Session mode: off`.

## Proof

`drive-NNN-mode-post-compaction-request.json`, `drive-NNN-mode-child-request.json`, frames `-padded`, `-compacted`, `-after`.

## Known gaps

Auto-compaction, resume of a compacted session, and switching directly between two skills are not driven.
