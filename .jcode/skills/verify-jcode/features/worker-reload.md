# Headless swarm worker restart

From a fresh verification run, execute `"$V" drive --evidence "$E" worker-reload`.

1. A terminal prompt drives a real `swarm spawn`. Wait for the worker to finish, confirming its saved transcript and ready status.
2. SIGTERM only the private daemon, leaving the coordinator attached until the daemon shuts down. The replacement must list the same worker as ready before the coordinator reconnects, with no loaded agent.
3. Resume the coordinator and send a real `swarm message` via a terminal prompt. Match the original worker history in `provider.jsonl` and verify a completed turn in the same worker session.
4. Send another message. The worker runs an `edit` tool that appends a marker once, then the fake provider holds inference after receiving the tool result. Restart again. The replacement lists the same worker as running and sends one continuation with unchanged conversation messages. Its output is saved, it becomes ready, and the marker is still present only once.

Evidence includes both daemon identity pairs, read-only worker status, actual HTTP messages, persisted worker transcripts, and the one-line marker. Run cleanup as usual. This drives SIGTERM/restart, not binary exec, and uses only the local fake model. Unit tests exercise how `await_members` classifies the startup status, reports, todo state and orphan retention.
