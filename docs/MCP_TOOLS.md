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
| Anything else, a person is attached | One prompt per call with the server, tool name and arguments. Choices are Allow once and Deny. There is no "remember". |
| Anything else, no person (`jcode run`, swarm worker) | Refused with an error naming `approve_free` and `approval = "off"`. The tool does not run. |
| Listed in `approve_free` | Runs. |
| Malformed annotations (for example `"readOnlyHint": "yes"`) | Treated as a write. |

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
