# Side-question history

`/btw <question>` starts a hidden child session. It inherits conversation context, but the parent does not receive the question or answer. An empty `/btw` lists the parent's ten most recent topics in the TUI. Use `/btw #N` to reopen a topic or `/btw #N <question>` to ask a follow-up. The number selects the ID shown in the last list. Ctrl+p opens the same child as a normal chat.

## Harness API

Check `side_questions_v1` in `hello_ok`. It requires support from both the bridge and daemon.

- `fork_side_question { session_id }` takes an attached parent and returns `side_question_forked { topic }`. It does not send a prompt.
- `list_side_questions { parent_session_id }` returns `side_questions { topics }` without attaching. It lists only direct children with BTW provenance. Topics sort by creation time descending, then session ID ascending. Follow-ups and promotion do not change the order.
- `get_side_question_history { session_id }` returns `history` containing user and assistant rows authored in that child. It does not attach, wake the session, or include the inherited context.
- To ask a follow-up, attach to `topic.session_id` and use the existing `send_message`. To branch from a topic, attach to that child and use `fork_side_question` with its ID as the parent.
- `promote_side_question { session_id }` makes the existing child visible in the ordinary session catalog. Calling it twice does not create a second session or a turn.

`SideQuestionTopic.context`, when present, reports the child’s last persisted model and effort without attaching. `SideQuestionTopic.message_start` is a raw persisted transcript offset, not an index into rendered history. Older TUI records with the original `hidden` marker still identify their parent, but their transcript boundary is unknown. Older Desktop forks and already-promoted legacy TUI forks cannot be distinguished reliably from normal forks. They are not inferred from prompt text. Older bridges return an unsupported-capability error from the SDK; they do not fall back to the normal catalog.

## Session lineage is not a message tree

Jcode stores each topic as one child session and uses its session ID as the stable topic ID. This is separate from the swarm owner reported on `SessionInfo`. Another agent harness stores BTW topics and their follow-ups in a session sidecar, keyed by topic ID and starting message leaf. Its separate `/tree` command changes the active message leaf inside one session. Listing Jcode child sessions does not move a message leaf or provide that operation.
