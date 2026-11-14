# Code Mode

Code Mode adds one native tool, `codemode`. The model writes a JavaScript
program and runs it in an embedded QuickJS sandbox. The program calls every
jcode tool and every MCP tool as an async function on `tools`. It can use
loops, branches, and `Promise.all`. Intermediate data stays in the VM. Only
the bounded return value, captured `console.*` output, and a host-recorded
receipt of the nested calls reach the model. The model sees the same idea as
Cloudflare's Code Mode: a typed TypeScript API generated from tool schemas.

## Enabling

```toml
[features]
code_mode = true   # default; master switch

[tools]
codemode = true    # legacy alias, also default; either false disables
```

`JCODE_CODEMODE=0|1` overrides both. When off, the tool is not registered.

## Script API

| Global | Meaning |
| --- | --- |
| `await tools.<name>(args)` / `callTool(name, args)` | Run a tool. JSON output is parsed to objects; failures throw `Error("<tool>: <reason>")`. |
| `listTools(filter?)` | `[{name, description}]` (first line of each description). |
| `types(filter?)` | TypeScript declarations (`type XInput = {...}; declare const tools: {...}`) for matching tools. |
| `describeTool(name)` | `{name, description, input_schema, typescript}`. |
| `store(k, v)` / `load(k)` / `storedKeys()` | JSON store persisted per session across runs. |
| `console.log/info/warn/error/debug` | Captured output, 64 KiB cap. |

MCP tools appear under their registry dispatch names (`mcp__<server>__<tool>`,
which are always valid identifiers). The unified `mcp` tool is also callable.
The legacy `mcp_search`/`mcp_call` names are hidden.

Report shape returned to the model:

```
console:
...
result:
<returned JSON, pretty-printed, or raw string>

calls:
  #1 mcp__fake__add ok 79ms 9B
  #2 shout failed 3ms 0B - Tool call blocked by pre_tool hook: ...
[2 tool calls, 0.1s, stored: k]
```

Receipt statuses: `ok`, `failed` (tool ran or a gate blocked it), `refused`
(never dispatched: unknown tool, call cap, bad args, cancelled), and
`in_flight` (the run ended by deadline or cancel while the call ran, so it may
have partly executed). The first 40 receipts are listed, then a per-status
summary.

## Authority: no extra grants

Each nested call is dispatched through `Registry::execute` with a sub-call
`ToolContext` (`codemode-<n>-<tool>`). This is the same path `batch` uses and
the same one direct calls end in. So every nested call gets:

- session allow/deny policy and SDK custom/disabled policy, plus the
  Desktop/selfdev product separation checks,
- `transform_tool_input` hooks and the user's `pre_tool` gate (block, rewrite,
  context), all keyed by the **real** tool name,
- `post_tool` hooks, telemetry, `TOOL_LIFECYCLE` logs, and remote-exec routing.

`codemode` and `batch` cannot be called from inside a script. The sandbox has
no filesystem, network, timer, module, or process API.

One deliberate difference: nested calls set `accept_large_output: true`. A
tool's output goes into the VM, not into the model context, so the per-call
context guard and the MCP token budget would only corrupt data the script is
about to filter. The final report is bounded on its own terms (below). This
affects output size only, never permissions.

## Limits

- QuickJS heap: 64 MiB, stack: 1 MiB.
- Per-program deadline: 300 s wall clock, including awaited tool calls. It is
  enforced by both an interrupt handler (busy loops) and a select on the
  future (hung tools). When the deadline fires, in-flight host calls are
  aborted.
- Call cap: 2000 nested calls per run. Calls past the cap throw and are
  receipted as `refused`.
- Report: 60k chars. Past that, the full report is spilled to
  `~/.jcode/codemode/spill/<session>-<call>.txt` and the path is included in
  the report.

## Engine choice: rquickjs

We picked rquickjs (QuickJS, `futures` feature) over V8/deno_core and boa:

- It is the established choice in this lineage. codewhale's `workflow-js`
  (the port reference) and `pi_agent_rust`'s extension runtime both embed
  rquickjs.
- It is small and builds from C with no 100+ MB V8 prebuilt or gn toolchain.
  It sits in its own leaf crate (`jcode-codemode`), so app-core edits never
  recompile it.
- It has the controls we need: heap limit, stack limit, interrupt handler,
  and async host functions bridged into Rust futures.
- Its limits are acceptable for glue code: no JIT (scripts are orchestration,
  not compute), and runtimes are `!Send`, so each run uses a dedicated thread
  with a current-thread Tokio runtime. Host calls are spawned back onto the
  caller's runtime.

FreeBSD has no prebuilt rquickjs bindings. There the tool reports itself
unavailable instead of failing the build.

TypeScript is not compiled. `types()` gives the model the declarations to
write against, and the program itself must be plain JavaScript (QuickJS
ES2023). This matches Cloudflare's model, which also strips types first.

## References

- codewhale `crates/tui/src/tools/codemode.rs` and `crates/workflow-js` (MIT):
  receipts, nested-call gate, deadlines, truncation and spill.
- Cloudflare Code Mode: typed TS API generated from MCP schemas.
- mitsuhiko/pi-codemode-mcp: a sandboxed JS `call_mcp`. Its author reports
  little efficiency gain over plain MCP with today's servers. The payoff is
  in filtering large or many outputs in-VM, which is what receipts and
  unbudgeted nested results target.
- vekexasia/pi-extensible-workflows: deterministic script orchestration.

## Gaps

- No TS transpile step, so type annotations in the program are syntax errors.
- No approval pause clock (codewhale pauses the deadline while a human
  approves). jcode's `pre_tool` gate is non-interactive, so this is not
  needed today.
- Nested calls are not shown as separate transcript tool cards. The receipt is
  the record.
- MCP schemas come from the registry's cached catalog. A server that has
  never been listed is invisible until `mcp` connects it once.
