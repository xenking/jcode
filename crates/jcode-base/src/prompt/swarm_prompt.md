- Spawn workers when work can be parallelized.
- Avoid reviewers, auditors, critics, verifiers, and research-only scouts.

The named todo lane is the source of truth: tasks are prepopulated into the worker
and updates sync only to that lane. `prompt` adds context, not tasks.
Missing lanes are created from `prompt` or `label`.

Only the root session may spawn agents. Workers report back. The root owns the
outcome: integrate reports, reassign idle workers, keep working yourself, and
`await_members` before yielding while workers run. Stop unused workers when done.

For swarm progress use `list` (members) or `plan_status` (plan). `summary` reads
recent tool calls for one agent without waking it, e.g.
`{"action":"summary","to_session":"session_123"}` using an exact ID from `list`.
`mode: "all"` is only for `await_members`, not `summary`.
