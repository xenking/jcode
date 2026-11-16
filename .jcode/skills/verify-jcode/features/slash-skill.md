# Slash commands and skill invocation

A user discovers commands in help and activates a project skill by slash name, optionally sending its trailing prompt in the same turn.

## Sub-features

- SLASH-HELP: command help is displayed locally.
- SKILL-ACTIVATE: the project fixture is resolved and activation is visible.
- SKILL-TURN: the trailing prompt and active skill body reach the real provider request.
- MODE-PIN: `/mode <skill>` persists a session mode without starting inference. `/mode` reports it and `/mode off` clears it.
- MODE-RESUME: the same session retains its mode after reattachment and daemon restart. Skill instructions remain in the transcript rather than the system prompt.

## How to get to it (user POV)

Type `/help`, `/?` or `/commands` and press Enter. Type `/skills` to list skills. Type `/verify-probe` to activate only, or `/verify-probe <prompt>` to activate and submit. Typing a slash prefix also opens inline completion.

## Driving it with verify-jcode

Preconditions: Launch seeded `.jcode/skills/verify-probe/SKILL.md` in the disposable workspace, with valid frontmatter and `VERIFY_PROBE_SKILL_BODY` in its body.

- **Help and skill turn:** `"$V" drive --evidence "$E" slash`. This submits /help, captures the help modal, closes it with Escape, then types /verify-probe with a fresh trailing token.
- **Proof:** require visible `Activated skill: verify-probe`, the model ack, the fresh token and `VERIFY_PROBE_SKILL_BODY` in actual HTTP messages, plus the saved session. Retain `*-help.{txt,frame.json,state.json}` and the final frame.
- **Additional entry points:** /?, /commands, /skills, bare activation and completion selection require separate terminal-input journeys with before/after frames. They are not verified by this automated baseline.
- **Pinned mode:** use `drive input --text "/mode verify-probe"`, then a fresh prompt. Inspect the saved session's `pinned_skill`, the hidden skill reminder and the matching HTTP request. Use `drive resume` and `drive restart`, then `/mode`, and check that each continued request still contains the skill body without a second activation. Capture a wide frame showing `mode: verify-probe` and a narrow frame where an overlong mode label is omitted rather than wrapped. Finally drive `/mode off` and inspect the persisted cleared field. Exploratory drives alone return `observed`, so retain independent session/request assertions.

## Gotchas

- This skill fixture is verification scaffolding, not a real developer skill. Cleanup removes it after preserving evidence.
- A bare skill activation sends no inference until there is a prompt. An ack from an earlier turn does not prove activation.
- A missing seed skill is a failed precondition, not permission to test a different installed skill from the user's home.
- /help is a modal. Close it with Escape before entering a prompt, otherwise input scrolls help instead of reaching the composer.
- The helper currently proves project discovery and active-body forwarding, not skill installation, refresh, multi-word names or global precedence.
