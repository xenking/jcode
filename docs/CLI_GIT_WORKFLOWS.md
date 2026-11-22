# Run Jcode in CI and work with Git

## Run a prompt from stdin

```sh
jcode run --ephemeral --output-last-message answer.txt - < prompt.txt
```

Omit the prompt or pass `-` to read stdin. `--output-last-message` replaces the file after a successful run. It writes the final assistant text, without the token summary or a JSON envelope. The existing `--json` and `--ndjson` output formats still apply to stdout.

`--ephemeral` does not save a resumable session. It does not stop tools from writing files or change how providers retain requests. Do not combine it with `--resume`.

## Review a Git change

```sh
jcode review --uncommitted
jcode review --base main
jcode review --commit HEAD
```

`--uncommitted` includes staged, unstaged, and non-ignored untracked changes. `--base` reviews committed changes from the merge base to `HEAD`. `--commit` compares the commit with its first parent.

The selected provider receives the Git diff. Jcode does not run model tools during review. Stdout contains a JSON object with `target` and `findings`. Each finding has a path, a line range, a severity, and a message. Findings do not change the exit status. A failed review exits nonzero. The TUI `/review` command still reviews the recent work of the current session.

## Start an isolated working tree

```sh
jcode --worktree
jcode --worktree=feature/my-change
```

Jcode starts a new local session in a worktree based on `HEAD`. Uncommitted changes stay in the original worktree. Jcode keeps the new branch and worktree after the session closes. Existing sessions keep their own working directories.

## Inspect changes in the TUI

Run `/git diff` to see the working tree patch, including non-ignored untracked files. Run `/git diff staged`, `/git diff unstaged`, or `/git diff untracked` to see one group. `/git status` still summarizes changes. `/diff` still controls how Jcode renders tool output.

`/git diff` reads the local session repository and does not ask the model. It reports that the command is unavailable when the TUI connects over SSH.
