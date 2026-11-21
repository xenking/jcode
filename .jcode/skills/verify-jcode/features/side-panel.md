# Side panel

A user opens a second view of the conversation to read context while leaving the main composer usable. Todo and swarm pages are related panel entry points but require different state.

## Sub-features

- PANEL-OPEN: split view becomes visible alongside chat.
- PANEL-CONTENT: the panel mirrors the live conversation.
- PANEL-COMPOSER: the main conversation remains visible and can receive input.
- PANEL-OTHER: todo and swarm pages represent real task/worker state.

## How to get to it (user POV)

Type `/splitview on` or `/splitview` to open mirrored chat, `/splitview off` to close it and `/splitview status` to inspect status. `/todos panel` opens the dedicated todo view. Swarm commands and worker activity expose swarm-related state.

## Driving it with verify-jcode

Preconditions: Launch and doctor passed, terminal 160x48. The helper creates a fresh real chat turn before opening the panel.

- **Mirrored chat:** `"$V" drive --evidence "$E" side-panel`. Type a fresh prompt, wait for its reply, then send /splitview on through the terminal.
- **Proof:** inspect `*-panel.json` for the live panel page, nonzero pane dimensions and content. Inspect `*-after.txt` and `*.frame.json` for the same conversation in the main chat and side panel, with the composer still visible.
- **Other entries:** toggle/off/status, todo pages and swarm worker spawn/report/cleanup need separate journeys. The baseline does not verify a real swarm lifecycle.

## Gotchas

- A narrow terminal may hide or rearrange the panel. Do not weaken a visibility assertion to accept a status message.
- This seed has no todo items or swarm workers. Todo/swarm journeys are unverified until actual tool-created state and worker lifecycle are added, not injected snapshots.
- Split view proves a side panel entry point, not every side panel page or keyboard focus behavior.
