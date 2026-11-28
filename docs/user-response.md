# Desktop user responses

`user_response` sends a user-visible Markdown answer inline in Jcode Desktop.
Use it for answers, explanations, and completion summaries. Do not repeat the
content in a normal assistant message. It does not open a panel, create an
applet, or write a file.

## Input and output

The model-visible input is an object with these properties:

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `content` | string | yes | Complete response in Markdown, preserved verbatim |
| `title` | string | no | Optional response heading |
| `intent` | string | yes | Standard shared tool UI label |
| `accept_large_output` | boolean | no | Standard shared context-budget opt-in |

Unknown properties are rejected. As with the other tools, strict-provider
nullable optional fields are accepted as null. `content` must be a string,
including when it is empty. At execution time the shared `intent` field may be
omitted, but the advertised schema requires it.

Example input:

```json
{"content":"## Result\n\nThe change is ready.","title":"Completed","intent":"Show result"}
```

The tool returns a `ToolOutput` with:

```json
{"output":"## Result\n\nThe change is ready.","title":"Completed","metadata":null,"images":[]}
```

This describes the internal tool result fields, not a new wire envelope. With
no title (or a null title), `title` is `None`. The output is raw Markdown, not
JSON-encoded Markdown or an acknowledgement. Existing tool hooks and context
budget safeguards still apply. Desktop can render `content` and `title` from
the original tool input, while the Markdown also enters the tool result.

## Client gating

The base registry always includes the tool. Calling it requires at least one
client attached to the same session that opted in with the Subscribe
`supports_user_response: true` capability. The harness API bridge forwards
the dedicated flag from the client's Hello handshake. Rust SDK Desktop clients
set `ConnectOptions.supports_user_response` or
`SshConnectOptions.supports_user_response` to true. PDF support does not imply
response support. TUI, generic SDK clients, and old clients default to false.
Attach, reconnect, and capability changes never rewrite the model's tool list.
Without a compatible attached client, use a normal assistant message instead.

## Runtime delivery

The registration and execution live in the Jcode daemon, not the Desktop
renderer or the harness API bridge. Rebuilding or reloading Desktop alone does
not activate this tool. The daemon must run a Jcode binary containing this
change, and Desktop must subscribe/reconnect to that daemon. The bridge fronts
the daemon selected by `JCODE_SOCKET`/the CLI daemon socket setting.

For safe verification, build with `justrust build --profile selfdev --bin jcode`
and run the resulting binary against its own socket rather than restarting
an in-use shared daemon. A production activation must use the established
safe Jcode runtime build/reload workflow. Restarting only the bridge cannot
update the daemon's tool registry.
