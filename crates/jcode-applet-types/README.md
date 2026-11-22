# Native Express authoring

Use text for explanations, progress updates, and answers. Use `applet` when the user can do something useful with the card. A code review with a note and two decisions is a good fit; an announcement of a completed build usually isn't. A click is user input, not permission to bypass tool approvals.

The model can call `applet` with `{"express":"root = ReviewChange(\"Review patch\", \"src/ui.rs\", \"change42\")"}`. Add `wait: true` only when the next step actually depends on the answer. For a single named action use `GreetingButton(label,event,recipient)`. For other layouts request `{"action":"catalog"}` once, then send a `view`. The catalog request does not create an applet. Desktop renders the expanded applet nodes with GPUI. No JS process runs in the client.

## Order of work

1. **Compact discovery (this change).** Keep the available macro signatures and the text-versus-UI rule in the ordinary tool definition. Load the full view reference only for custom layouts. Macro names in the model hint and the parser come from one declaration in the existing `jcode-applet-types` package. Measure authoring arguments using a named tokenizer, not JSON byte length.
2. **Composable choices and forms (next).** Add stable IDs, a small layout vocabulary and bound inputs when there is a concrete workflow the two macros cannot express. Require a keyboard-operable control, stable focus on update and a typed action round trip. Benchmark against the equivalent explicit view before keeping new syntax. Do not add another spelling of the built-in `ask` flow without a distinct use case.
3. **External A2UI interoperability (later).** Negotiate a versioned renderer catalog and validate create/update/delete plus returned actions against pinned upstream wire fixtures. Today this compiler targets Jcode applet documents, not arbitrary A2UI streams. Avoid a new transport until a real external producer needs it.

The [upstream inference formats](https://a2ui.org/concepts/inference-formats/) distinguish what a model writes from what a renderer consumes. [Upstream macros](https://a2ui.org/guides/macros/) expand on the producer side. These are the patterns reused here. [CopilotKit's examples](https://github.com/CopilotKit/generative-ui) illustrate several frontend choices; they are not dependencies or a native GPUI conformance suite.
