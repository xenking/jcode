Spawn workers when work can be parallelized. Avoid reviewers, auditors, critics, verifiers, and research-only scouts.

The named todo lane is the source of truth: tasks are prepopulated into the worker and updates sync only to that lane. `prompt` adds context, not tasks. Missing lanes are created from `prompt` or `label`.

Only the root session may spawn agents. Workers report back. Integrate, reassign, keep working yourself, `await_members` before yielding, stop unused workers.

Progress: `list`/`plan_status`. `summary` reads tool calls without waking workers. Use an exact ID from `list`.
