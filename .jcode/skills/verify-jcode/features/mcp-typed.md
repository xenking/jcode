# Typed MCP results and per-call approval

`scripts/accept-mcp-typed.py` is self-contained: it starts its own scripted OpenAI-compatible provider, three fixture MCP servers (stdio, HTTP, a 150-tool catalog), a private daemon and, with `--tui`, a private tmux server. It does not use `launch`, so no evidence directory is needed. Build the binary as in the skill's Launch, then:

```bash
python3 scripts/accept-mcp-typed.py "$BINARY" --tui --keep "$E/mcp-typed"
```

It exits nonzero on the first failed check and prints one `ok` line per check. Without `--tui` it takes about 20 s, with it about 40 s. `--keep` retains the TUI frames (`tui-prompt-N.txt`, `tui-final.txt`).

What it proves, each against a fixture server whose write tools append to a log, so a refusal is shown by the log staying empty:

- A tool with an `outputSchema` is declared as `Promise<...Output>` in `types()` and returns its `structuredContent`, over stdio and over HTTP. `jcode mcp test` marks typed and read-only tools and counts the tools that will ask.
- With `approval = "annotations"`, a read-only tool runs free and every other tool (unannotated included) is refused when nobody can be asked (`jcode run`, swarm worker), with no side effect. `approve_free` exempts exactly the tool it names.
- A 150-tool catalog adds no tool to the provider's tool list (the list equals a run without the server) and the first request stays far below the size of the schemas, yet `listTools('mcp__big')` returns 150 and a deferred tool is typed and callable.
- Through the daemon protocol, each gated write sends its own `ask_user_request` with the tool and arguments. `background_tool` while the prompt is open answers `moved: false`. Allow once runs that call only. Deny and a free-text reply do not run it. The finished `codemode` row carries one receipt per call, the refused ones as `error` with a safe category and not the raw message.
- In the TUI, Alt+B leaves the prompt open, Down+Enter allows, Enter denies, and the codemode row lists one `✓`/`✗` row per nested call with the reason under each refused one.

Not covered: real vendor models, stdio servers that need credentials, Jcode Desktop (its acceptance script lives in the Desktop repository: `scripts/accept-mcp-approval.py`).
