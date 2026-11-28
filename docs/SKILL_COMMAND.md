# Run a CLI from a skill

On a Unix TUI, add `command` to a skill you trust:

```markdown
---
name: fleet
description: Manage worker machines
command: fleet
---
Instructions for the model when it loads this skill.
```

Typing `/fleet dispatch box-1 'fix the build'` runs `fleet` in the session's working directory with three arguments. The shell output appears in the transcript, as with `!`, and no model turn starts. Model-initiated skill loads still return the Markdown body.

The installed `command` itself is shell code. Only invoke command skills from locations you trust. Quoting in your input groups arguments, and those arguments are passed as data rather than shell source. SSH TUI connections do not execute commands from client-local skills.
