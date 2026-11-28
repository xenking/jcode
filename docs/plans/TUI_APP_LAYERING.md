# TUI App layering

`crates/jcode-tui/src/tui/app.rs` defines one `App` struct with about 450 fields.
Its methods are spread over about 190 files as `impl App` blocks. Those blocks
call each other in both directions, which is what `deslop-please` reports:

- one module cycle of 79 sibling modules under `tui::app`,
- one tangle of 380 items (`App` plus the free functions that take `&mut App`),
- ten shallow parent modules whose children use their items,
- two wide directories (`tui/` with 174 entries, `app/` with 188).

The fix is structural. A field cluster and the logic that touches only that
cluster move into their own type. `App` owns the type and calls it. The type
never names `App`.

## Rules

1. A sub-state is a plain struct in its own module. It owns its fields and
   implements the methods that read or write only those fields.
2. A sub-state never imports `App`, `TuiState`, `RemoteConnection` or any module
   above it in the table below. Anything it needs from outside arrives as an
   argument or a small input struct.
3. `App` methods that need two or more sub-states stay on `App`. They are the
   mediator layer and are the only code allowed to combine sub-states.
4. Edges point down the table. A lower layer is never called back from a lower
   layer's own methods.
5. A sub-state exposes behaviour (`zoom_in`, `toggle`, `scroll_by`), not its
   fields. Tests that need fields live in the sub-state's own module.

## Layers

| Layer | Contents | May depend on |
| --- | --- | --- |
| L0 sub-states | the types in the next table | `std`, config types, protocol types |
| L1 mediators | `impl App` methods that combine sub-states (turn bookkeeping, history paging, failover) | L0 |
| L2 input dispatch | key, mouse, paste and slash-command handling | L0, L1 |
| L3 backend IO | remote event handlers, reconnect, cloud handoff, ssh | L0, L1 |
| L4 rendering | `ui*`, `info_widget*`, `session_picker` views, reading `TuiState` | L0 (read only) |

L2 and L3 do not call each other. When input dispatch needs a remote action it
returns an `Action` value. The run loop, which is above both, performs it.

## Sub-states

Field counts come from a scan of the `App` struct at
`origin/master` 66991b3c8. "Self-contained methods" is the number of `impl App`
methods that touch only fields of that cluster (a lower bound for how much logic
moves with it).

| Sub-state | Owns (examples) | Fields | Messages it handles |
| --- | --- | --- | --- |
| `DiagramPane` | mode, focus, index, scroll, ratio animation, zoom, drag | 14 | diagram hotkeys, mouse drag, resize ticks |
| `SidePanel` | side panel snapshot, diff pane scroll and focus, split view, observe view, panel images | 27 | panel toggles, scroll, refresh ticks |
| `TodoState` | todos view, pinned todos, remote todo cache, auto-poke counters, background task rows | 27 | todo server events, poke ticks, `/todos` |
| `Composer` | input text, cursor, pasted content, pending images, command suggestion caches, prompt history, queued messages | 24 | key edits, paste, submit, history recall |
| `Voice` | voice input, dictation session, jarvis speaker and listener, external voice activity | 11 | voice hotkeys, capture events, speech playback |
| `Onboarding` | flow state, import progress, update simulator, version checks | 16 | onboarding steps, update notices |
| `Cloud` | cloud job, handoff, recovery context | 8 | `/cloud` commands, handoff events |
| `Pickers` | inline interactive, model picker, ask picker, routing gate, account picker, btw card | 23 | picker keys, catalog refresh, model switch |
| `RemoteLink` | session id, history recovery, turn fence, ssh sessions, reload recovery | 57 | server events, reconnect, resume |
| `TurnState` | processing flag, streaming progress, thinking and reasoning buffers, token and cost accounting, status | 44 | stream deltas, turn end, cancel |
| `Transcript` | messages, display messages and counters, compacted history, scroll offset | 20 | message append, history paging, scroll |
| `Swarm` | swarm panel, members, plan items, file activity | 10 | swarm events, panel keys |
| `Failover` | pending failover, fallback and merge offers | 9 | provider errors, offer replies |
| `Keybindings` | resolved key tables | 11 | config reload |
| App core | 152 fields not yet classified (provider, registry, session handles, request flags) | 152 | orchestration only |

The App-core remainder shrinks as clusters are extracted. Anything still
there after the table is done is either orchestration state or a missing
sub-state, and gets a row here when it is classified.

## Extraction order

Smallest and most self-contained first, so each step is one verifiable commit:

1. `DiagramPane`: 14 fields, 16 self-contained methods, no remote or input
   dependency.
2. `SidePanel` and its view companions.
3. `Voice`.
4. `Onboarding`.
5. `Cloud`.
6. `TodoState`.
7. `Composer`.
8. `Pickers`.
9. `Transcript` and `TurnState` together (they share the streaming path).
10. `RemoteLink`, last, once L1 mediators are the only callers left.

Each step:

1. Add the struct and move the fields, keeping names but dropping the prefix.
2. Move every method that touches only those fields onto the struct.
3. Leave a one-line delegate on `App` only when a caller outside the cluster
   needs it. Otherwise change the caller to go through the field.
4. `justrust test -p jcode-tui` and `justrust clippy -p jcode-tui` must be green.
5. Run `deslop-please` and shrink the allowlist for whatever the step fixed.

## What the lints measure

- Module cycle and tangle: an edge exists when module A uses an item defined in
  module B. Moving a method from `impl App` in module A onto a sub-state in
  module B removes the A to B edge only if B no longer needs A.
- Shallow module: counts functions that other modules (children included) use,
  not declared visibility. A parent split into children is shallow when the
  children call most of its functions. Moving the cluster's callers into the
  parent, or the shared functions into a leaf, fixes it.
- Field leak: a field is "known outside" when any other function reads or writes
  it directly or through a trivial accessor. Sub-states expose behaviour, so
  this goes away as a side effect.
- Wide directory: more than 15 entries. Each sub-state gets a directory
  (`app/diagram_pane/`, `app/voice/`, ...) holding its module and tests.

## Probe API for frame capture

`jcode-tui-visual-debug::FrameCaptureBuilder` exposes `layout_mut`, `state_mut`
and `rendered_text_mut`, so `ui/draw_frame.rs` writes about 60 fields one at a
time. The replacement is a probe trait implemented in `jcode-tui`:

```text
trait FrameProbe { fn state(&self) -> StateFacts; fn rendered(&self) -> RenderedFacts; }
builder.record_frame(&probe, &geometry)
```

The builder derives its `StateSnapshot` and `RenderedText` from the facts. The
renderer passes one geometry value per stage instead of mutating the builder.
