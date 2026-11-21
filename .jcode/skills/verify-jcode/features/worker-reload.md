# Headless swarm worker restart

From a fresh verification run, execute `"$V" drive --evidence "$E" worker-reload`.

1. A terminal prompt drives a real `swarm spawn`. Wait for the worker to finish, confirming its saved transcript and ready status.
2. SIGTERM only the private daemon, leaving the coordinator attached until the daemon shuts down. The replacement must list the same worker as ready before the coordinator reconnects, with no loaded agent.
3. Resume the coordinator and send a real `swarm message` via a terminal prompt. Match the original worker history in `provider.jsonl` and verify a completed turn in the same worker session.
4. Send another message. The worker runs an `edit` tool that appends a marker once, then the fake provider holds inference after receiving the tool result. Reload via pinned exec while the coordinator is still attached. The daemon keeps its PID, records a headless recovery intent, and starts one continuation with unchanged conversation messages. The worker is running and does not publish a terminal status before the provider is released. Its output is then saved, it becomes ready, and the marker is still present only once.

Evidence includes both daemon identity pairs, the exec trace, worker status, actual HTTP messages, persisted worker transcripts, and the one-line marker. Run cleanup as usual. The active-worker leg uses the same daemon exec path as a forced rollout, but re-executes the same staged binary without installing a new generation or swapping Desktop. The model is local and fake. Unit tests exercise how `await_members` classifies the startup status, reports, todo state and orphan retention.
