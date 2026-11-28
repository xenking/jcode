# Interactive turn and real tool rendering

A user types a prompt into the composer, presses Enter, sees an assistant turn and a tool call/result, and gets a real file written in the selected workspace.

## Sub-features

- TUI-TURN: composer input reaches the model and a completed reply is shown.
- TUI-TOOL: the assistant's write call runs through the production executor and renders in chat.
- TUI-SAVE: the conversation and tool result are saved independently of the visible screen.

## How to get to it (user POV)

Launch the normal TUI attached to this run's private daemon. Type in the composer and press Enter. Paste and tool row expand/collapse are additional entry points, not covered by ordinary typing.

## Driving it with verify-jcode

Preconditions: Launch and doctor passed. The workspace is disposable and writable.

- **Send and execute:** `"$V" drive --evidence "$E" tui` types a fresh token plus `[e2e:verify-write]` using terminal events. The helper first removes only its own previous `verify-tool.txt` fixture, the existing fake provider returns a write call, the real app writes `verify-tool.txt`, then the model receives the real tool result and acknowledges it.
- **Observe:** inspect `drive-*-tui-after.txt` and `*.frame.json` for the typed token, write tool row/result and completed assistant ack. Inspect the matching `*-session.json` for the persisted conversation.
- **Side effect:** `drive-*-tui-verify-tool.txt` must contain exactly `verification tool side effect` followed by newline. `provider.jsonl` must contain the tool result after the user token.
- **Read/write ordering:** `"$V" drive --evidence "$E" file-tools` issues four native calls in one assistant message. The provider must receive the original value, a distinct past-EOF result, the edit result, and the updated value in call order. The helper also checks the final file contents. This journey verifies semantics, not concurrent overlap or latency.
- **Capture:** `"$V" frame --evidence "$E" --label tool-reviewed` retains the current terminal, real cell/layout frame and client state.

## Gotchas

- The fake provider is the external boundary only. Creating the file directly in the helper or injecting display messages would invalidate the proof.
- Check the actual tool row visually. A file write alone cannot prove its label, styling, wrapping or expansion behavior.
- Queueing, soft interrupts, cancellation, paste, expand/collapse and narrow terminal sizes need additional terminal journeys. Do not infer them from this baseline.
- The default 160x48 terminal does not validate narrow-width chrome. Start a separate run with `launch --cols 60 --rows 30`, then use input/keys and frames to test the changed path.
