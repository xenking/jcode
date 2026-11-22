# Code Mode

Code Mode adds one native tool, `codemode`. The model writes a JavaScript or
TypeScript program and runs it in an embedded QuickJS sandbox. The program
calls every jcode tool and every MCP tool as an async function on `tools`. It can use
loops, branches, and `Promise.all`. Intermediate data stays in the VM. Only
the bounded return value, captured `console.*` output, and a host-recorded
receipt of the nested calls reach the model. The model sees the same idea as
Cloudflare's Code Mode: a typed TypeScript API generated from tool schemas.

## When the model is told to use it

Tool descriptions alone were not enough: measured over three days (2026-11-13
to 2026-11-15), `codemode` was 11 of 7,884 tool calls (0.14%, and 6 of those 11
came from probe sessions told to use it), while `bash` was 65.5% and `batch`
1.1%. The cause was steering, not availability: the tool was registered and
eager in every build, but nothing told the model when one script beats many
calls.

Three places now carry that guidance:

- `CODEMODE_PROMPT` in `jcode-base/src/prompt.rs`, appended to the static
  system prompt when both `[features] code_mode` and `[tools] codemode` are
  on. It lists the work shapes: 5+ calls, dependent chains, loops over a
  previous result, and large outputs that only need a summary. For example,
  find matches with `agentgrep`, pass their paths and line numbers to `read`
  for small windows, and return only relevant `path:line` entries. Use
  `Promise.allSettled` when independent reads can succeed or fail separately.
  Call `swarm` directly for short coordination. Use `codemode` for dependent
  swarm reads or to return only needed fields from large results. See the
  [swarm wire measurement](research/swarm-token-cost.md).
- The tool description leads with the same shapes before the mechanics, and
  says which tool wins for each (`batch` for 1-10 independent calls, `bash`
  for shell work).
- `batch`'s description points at `codemode` for dependent or looping work.

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
| `await tools.<name>(args)` / `callTool(name, args)` | Run a tool. JSON object/array output becomes a JS object/array. Other output is a string. Failures throw `Error("<tool>: <reason>")`. |
| `listTools(filter?)` | `[{name, description}]` (first line of each description). |
| `types(filter?)` | TypeScript declarations (`type XInput = {...}; declare const tools: {...}`) for matching tools. |
| `describeTool(name)` | `{name, description, input_schema, typescript}`. |
| `store(k, v)` / `load(k)` / `storedKeys()` | JSON store persisted per session across runs. |
| `console.log/info/warn/error/debug` | Captured output, 64 KiB cap. |

MCP tools appear under their registry dispatch names (`mcp__<server>__<tool>`,
which are always valid identifiers). The unified `mcp` tool is also callable.
The legacy `mcp_search`/`mcp_call` names are hidden.

The script sees the registry's rendered MCP output, not a guaranteed native
MCP envelope. Check the returned shape before using it. Argument errors from
`agentgrep` and JSON shape errors from `read` identify the bad field and show
one valid call. For example, `agentgrep` trace expects a `terms` array with
DSL entries such as `['subject:auth_status', 'relation:rendered']`. Repair the
failed step rather than replaying successful calls with side effects.

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
- Optional `max_output_tokens` (512–15,000): use `4000` when a preview is
  enough. This is a provider-independent estimate of one token per four UTF-8
  bytes, not a billing count. The budget applies only to the final report,
  not nested results or stored values. A truncated preview includes the error
  (or its beginning), call counts by status, and the full report path when
  available. Omit the parameter to keep the original report limit.

To page data without repeating tool calls, save it before returning:

```js
const matches = await tools.agentgrep({mode: 'grep', query: 'TODO'});
store('matches', matches);
return matches; // Call codemode with max_output_tokens: 4000 for a preview.
```

Then run `return load('matches').slice(12000, 14000)` in the same session.
For structured data, slice the array or return selected fields instead.

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

## TypeScript

The script may be TypeScript. `types()` hands the model TypeScript
declarations for every tool, so writing `const args: ReadToolInput = {...}`,
`interface`, `as`, `satisfies`, generics, non-null `!`, optional parameters and
`enum`s has to work rather than fail as a syntax error. `crates/jcode-codemode/src/strip.rs`
erases the types in-process before QuickJS sees the script: types are removed,
never checked, which is the same bargain Deno and Bun make. No `tsc`, no Node,
no subprocess.

### Why oxc, and what it costs

Measured on this machine (aarch64 macOS, the workspace `release` profile:
`opt-level = 1`, 256 codegen units, no LTO) with a 2 KiB TypeScript snippet
covering every construct above, built twice from an empty target directory:

| | oxc 0.146 (picked) | deno_ast 0.53 + swc (what Deno uses) |
| --- | --- | --- |
| release binary added | **+3.25 MB** (433 KB base -> 3.68 MB) | +10.68 MB (433 KB -> 11.11 MB) |
| clean dependency build | **14.8 s** | 53.4 s |
| crates in the graph | 123 | 181 |
| strip 2 KiB TS, p50 | **23-28 us** (p90 28-64 us) | 100-102 us (p90 117-123 us) |
| strip 1.4 KiB JS, p50 | **18 us** | 84 us |

Both produce equivalent JavaScript: types gone, `enum` lowered, top-level
`await` and `return` kept. Costs above include the codegen source map the line
map needs; the map itself is free at this size, it is codegen's token table, not
a JSON round trip. oxc wins on all three axes (3.3x smaller, 3.6x faster to
build, ~4x faster to run), so the smaller one it is.

`oxc` is pinned to `=0.146.0`: that is the last release whose MSRV is the
workspace toolchain (1.95). `0.158` requires 1.97 and would drag a toolchain
bump into this change.

### Bun is not a third candidate

Bun moved its runtime to Rust (v1.4.0, August 2026), so `bun_js_parser` looked
like a candidate. It is not extractable, before performance even matters (no
size or latency figure is obtainable, and the run stops at the build script):

- It is workspace-internal: `version.workspace = true`, workspace lints, and 16
  `bun_*` sibling crates (`bun_core`, `bun_sys`, `bun_crash_handler`,
  `bun_highway`, `bun_react_compiler`, ...), none published to crates.io. A git
  dependency clones the whole 1.6 GB repo (491 MB bare git db here) and drags
  that graph in.
- `lib.rs` opens with `#![feature(optimize_attribute)]` and the repo pins
  `nightly-2026-09-15`. The workspace builds on stable 1.95.
- `bun_core`'s build script panics without Bun's own configure output
  (`build/debug/codegen/build_options.rs`), and `bun bd --configure-only` then
  demands clang >= 23.1.0 < 23.2 (this machine has 21.0.0).
- The strip-and-print entry point is `bun_transpiler`, a re-export of
  `bun_bundler::transpiler`; `bun_js_parser` alone is just the parser.

Rebuilding that tree is the price of using the parser at all, so no figure would
change the answer: erasing types must not pin the harness to someone else's
nightly and someone else's build system. License is the one clean dimension: Bun
is MIT, and the parser tier is deliberately JSC-free (LGPL-2 JSC enters only
through the `*_jsc` crates).

### Design notes

- **Plain JavaScript is untouched.** `strip` parses as an ES module first and
  returns the input byte for byte when that succeeds, so a JS script's stack
  traces are its own and the transform is skipped entirely. Only a script that
  does not parse as JavaScript goes through the semantic builder, the
  TypeScript transform and codegen.
- **Top-level `return` is legal.** The engine wraps the body in
  `(async () => { ... })()`, so the parser runs with
  `allow_return_outside_function`, and the module source type keeps top-level
  `await` legal.
- **Line numbers survive.** Codegen reprints the program, so the stripped script
  has different line numbers than the model's source. `strip` turns the codegen
  source map into a stripped-line to original-line table, and the engine maps
  every `file:line:col` in a QuickJS stack back through it (minus the one
  wrapper line) before reporting. Errors point at the line the model wrote.
- **Parse errors carry a position.** A script that is neither JavaScript nor
  TypeScript reports `line L:C: <message>` from the parser over the original
  source, before any rewriting. That is the only error path that can name a
  typo in the annotations themselves.
- **Enums need `enum_eval`.** The TypeScript transform reads member values the
  semantic builder only precomputes with `SemanticBuilder::with_enum_eval(true)`;
  without it the lowering is wrong (and debug builds assert).

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

- Types are erased, never checked. A wrong annotation is a no-op, not an
  error, so a script can type-check in the model's head and still fail at
  runtime.
- Only syntax is erased. `tsconfig` knobs (`target`, `useDefineForClassFields`,
  `experimentalDecorators`) are the transformer defaults, and JSX is off;
  `tsx` input is not accepted.
- Columns inside a rewritten statement can be approximate: lines are mapped
  exactly through the codegen source map, but column starts come from the
  reprinted text.
- No approval pause clock (codewhale pauses the deadline while a human
  approves). jcode's `pre_tool` gate is non-interactive, so this is not
  needed today.
- Nested calls are not shown as separate transcript tool cards. The receipt is
  the record.
- MCP schemas come from the registry's cached catalog. A server that has
  never been listed is invisible until `mcp` connects it once.
