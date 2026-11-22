# Use arguments in a skill

```markdown
---
name: issue
description: Work on an issue
arguments: repo, number
---
Look up issue $number in $repo. The full request was $ARGUMENTS.
```

`/issue "my repo" 42` binds `repo` to `my repo` and `number` to `42`. `$ARGUMENTS` preserves the text after the skill name. `$0` and `$ARGUMENTS[0]` both refer to the first argument. An omitted named argument is empty; a missing positional argument stays literal.

`${JCODE_SKILL_DIR}` points to the directory containing `SKILL.md`. `${JCODE_PROJECT_DIR}` points to the session working directory. Both accept `CLAUDE_` instead of `JCODE_`. Use `\$` for a literal dollar sign. Substituted text is not expanded again.

Invoking a skill again replaces its previous arguments. Jcode adds another model reminder only when the rendered instructions change. A bare invocation clears the arguments.
