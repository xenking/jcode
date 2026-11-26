# MCP tools: typed results, approval, and large catalogs

Per-server options live next to the transport settings in `~/.jcode/mcp.json`
or a project `.mcp.json` (`servers` or `mcpServers`). See
[MCP_REMOTE.md](MCP_REMOTE.md) for the transport fields.

```json
{
  "mcpServers": {
    "tracker": {
      "command": "tracker-mcp",
      "approval": "annotations",
      "approve_free": ["add_note"]
    }
  }
}
```

| Option | Values | Default | Meaning |
| --- | --- | --- | --- |
| `approval` | `"off"`, `"annotations"` | `"off"` | `"annotations"` makes every tool call that is not declared read-only wait for a human yes. Any other value is treated as `"annotations"`, so a typo never turns the gate off. |
| `approve_free` | list of raw MCP tool names | `[]` | Tools exempt from approval. Names are the server's own (`add_note`), not jcode's `mcp__<server>__<tool>` alias. |

There is no `expose` option. The default `[tools] mcp_tools = "auto"` already
keeps per-server tools out of the model's tool list (see "Large catalogs").

## Typed results

jcode keeps each tool's `outputSchema` from `tools/list` and the
`structuredContent` of each `tools/call` result.

- In [codemode](CODEMODE.md), a tool with an `outputSchema` is declared
  `Promise<XOutput>` by `types()` and `describeTool()`, generated from the
  schema the same way inputs are (including local `$ref`s). The call returns
  the `structuredContent` object. A tool without an `outputSchema` is
  unchanged: `Promise<any>`, the parsed JSON text or the raw string.
- The result the model reads is unchanged: the server's text content. A server
  that returns only `structuredContent` gets that JSON as the text.
- A malformed `outputSchema` (not an object) is ignored and the tool stays
  untyped. An error result (`isError`) is never typed.

## Approval from annotations

With `approval = "annotations"` a call runs without asking only if the tool
declares `readOnlyHint = true` or is listed in `approve_free`. Everything else
asks first, including tools with no annotations, because the spec's defaults
would otherwise treat them as read-only.

| Situation | Result |
| --- | --- |
| `approval` off (default) | Runs, as before. |
| `readOnlyHint = true` | Runs. |
| Anything else, a person is attached | One prompt per call with the server, tool name and arguments. Choices: Deny, Allow once, Always allow this tool, Always allow every tool on this server. |
| Anything else, a swarm worker whose root session has a client | The same card is shown on the root session's client, labelled with the worker's name plus the server, tool and arguments. The answer resolves the worker's call; "Always" answers are saved as for any session. With several root clients the first answer wins. Only that client can answer: the coordinator model cannot, and a message typed in the root session is not an answer. Logged as `MCP_APPROVAL ... session=<worker> routed_to=<root> ...`. |
| Anything else, no person (`jcode run`, a worker whose root has no client) | Waits up to 30 s (`JCODE_MCP_APPROVAL_ATTACH_WAIT_SECS`, `0` = refuse at once) for a client to attach (to the session or its root), then refuses with an error naming `jcode mcp approvals allow`. The tool does not run. The refusal and every prompt are logged as `MCP_APPROVAL`. |
| Listed in `approve_free` (`"*"` = every tool) | Runs. |
| Malformed annotations (for example `"readOnlyHint": "yes"`) | Treated as a write. |

"Always allow" answers are saved as `approve_free` entries in the config file the
server is read from (`~/.jcode/mcp.json`, or a project `.jcode/mcp.json` /
`.mcp.json`; never `~/.claude.json`, which Claude Code owns). The policy is read
from those files on every call, so a grant or a revoke applies to every running
session at once. Manage them with `jcode mcp approvals` (list, `--json`),
`jcode mcp approvals allow <server> [<tool>]` and
`jcode mcp approvals revoke <server> [<tool>]` (no tool: every grant of the
server). Writes use the file's JSON keys; other keys are kept, key order and
indentation are normalised. A file that does not parse is never rewritten.

`[mcp.servers.*]` in `config.toml` is not read; jcode warns at daemon start and in
`jcode mcp list` / `approvals`. Define servers in `mcp.json` under `servers`.

The gate sits in the tool registry, so direct calls, `batch`, `codemode`
programs, and the unified `mcp` tool (`action=call`) all meet it.

The model cannot answer for the user. The prompt is a human approval: the
Desktop automation tool will not click it, TUI key injection is refused while
it is open, and a plain-text stdin reply is rejected. A free-text answer is
not a yes.

While a call waits for a person it stays in the foreground: a direct call is not
moved to the background after a few seconds, and a `codemode` or `batch` call
keeps its foreground (up to the 15 minute blocking ceiling) whenever some MCP
tool is gated, because the prompt belongs to the live turn. Inside `codemode`
the wait is also not counted against the script's 300 s limit.

`jcode mcp test <server>` shows what jcode learned from the server and what
will ask:

```
Approval: annotations (2 of 3 tools ask a human first)
  bare  (unannotated, asks first)
  read-item  (read-only, typed result)
  write-item  (writes, asks first)
```

Annotations are hints from the server. Opt in only for servers you trust to
label their own tools. A server that mislabels a write as read-only gets no
prompt.

## Large catalogs

A server with ~150 tools does not inflate the system prompt in the default
mode. With `[tools] mcp_tools = "auto"` (or `"deferred"`):

- Providers with native deferred loading get each MCP tool as a deferred
  definition outside the cached prefix.
- Other providers get the fixed `mcp` search/call surface. Tools that appear
  after the tool list is locked are named once in the transcript (names and one
  line each, at most 64), not added to the list.
- In `codemode`, every MCP tool is `tools.mcp__<server>__<tool>`. Discover
  them with `listTools('mcp__<server>')` and read typed signatures with
  `types('<tool name fragment>')`. Filter before calling `types()` on a large
  server.

Only `mcp_tools = "eager"` puts every tool in the list. Use it for small
servers.
