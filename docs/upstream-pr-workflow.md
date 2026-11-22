# Upstream PR workflow

The CLI fork tracks `1jehuang/jcode` and `1jehuang/jcode-desktop` in one
[decision ledger](upstream-pr-decisions.json). It records a skip, deferral, or
verified fork equivalent against the exact GitHub head SHA reviewed. A changed
head resurfaces. A fork commit with an `Upstream-PR: 1jehuang/<repo>#<number>`
trailer takes precedence over the ledger, even after the PR is closed or
merged. Put `Upstream-PR` and `Co-authored-by` in the same final Git trailer
block, without blank lines between them. For an already-published port missing
its trailer, use an empty attribution commit with `Upstream-PR` and
`Upstream-PR-Commit: 1jehuang/<repo>#<number> <full original fork SHA>` in the
same block. The original commit remains the source patch for reconciliation.

## Triage

From a fresh fork worktree, with authenticated `gh` and Python 3.10 or newer:

```sh
python3 scripts/xk-upstream-prs.py status
python3 scripts/xk-upstream-prs.py record 'jcode#123' skipped \
  --head '<full head SHA from status>' --reason 'Describe the decision'
```

Use `already-in-fork --covering <fork commit>` to record an existing equivalent
or `deferred` to record a deliberate postponement. Decisions suppress only the
reviewed head. Commit the updated ledger through the regular fork workflow.
The command reads published fork refs for both repositories. Refresh those refs
with `git fetch origin master` (CLI) and `git fetch origin main` (Desktop) if
other ports have just landed. It does not fetch or edit their worktrees itself.

The one-time November 22, 2026 import reads `audit-{a,b,c}.md` and the
`jcode-prs.json` / `desktop-prs.json` snapshots in the audit directory. Preserve
selected work with repeatable `--keep-undecided 'jcode#123'`. The original
snapshots lacked head SHAs, so the importer requires unchanged `updatedAt`
before recording the live head; it leaves intervening changes undecided.
It also checks covering commits against the published fork, preserves any prior
decision, and does not record ports. The ledger's `audited_through` numbers
separate new PRs from the undecided audit backlog. An older PR that reopens
without a matching decision will appear in that backlog.

## Sync a previously ported PR

`xk-upstream-sync.sh` prints the new and changed PRs before merging. It checks
merged ports in the candidate before validation and after merging any late
origin changes. The original port stays in history for provenance. If the
landed patch matches one port exactly and the actual candidate matches upstream
on every affected path, sync records `upstream` automatically.

If Git reports conflicts, resolve those first and call sync with `--continue`
to commit the merge and see any outstanding port reviews. Sync will not push
an unreviewed variant. For each review:

1. Inspect `git show --first-parent <upstream merge SHA>` and each fork port SHA
   listed by sync. Check the actual candidate tree, not just the merge conflict
   list. For a rebase merge, inspect all of the PR's landed commits. For a merge
   commit, use the diff against its first parent.
2. Keep upstream's implementation unless the fork already contains all of it
   and preserves additional required behavior. Remove any duplicate paths or
   call sites and retain unrelated fork changes. Commit any edits and verify
   the affected behavior using the repository's scoped tests.
3. Record the outcome in the sync worktree. Use `jcode-desktop` in place of
   `jcode` for the Desktop worktree. The reason is written to Git history.

```sh
repo="$HOME/Projects/github.com/xenking/jcode"
git -C "$repo" show origin/master:scripts/xk-upstream-prs.py |
  python3 - reconcile jcode "$HOME/.jcode/wt/upsync/jcode" \
    --choose 'jcode#123' upstream --reason 'Explain why the merged tree has one implementation'
```

Use `fork-superset` instead of `upstream` for a verified superset, then run
`xk-upstream-sync.sh --continue`. If a new port commit appears on origin during
sync, the old reconciliation does not attest to the expanded set of ports.
Closed PRs that were never merged are reported but do not change the fork.

## Daily report on macOS

Install the report-only job from the published CLI fork:

```sh
bash scripts/xk-upstream-pr-report.sh --install
```

The LaunchAgent runs when loaded and at 09:00 local time. It refreshes fork
tracking refs, reads GitHub PRs and the published ledger, updates
`~/.jcode/upstream-pr-report/status.md`, and posts a macOS notification when
that report changes. It does not merge, port, push, change checkouts, or touch
the live Jcode daemon or app. `errors.log` records failures, and a failed read
leaves the last report intact. Run `bash scripts/xk-upstream-pr-report.sh --run`
for an immediate report, or `launchctl print gui/$(id -u)/com.xenking.upstream-pr-report`
to inspect the scheduled service. To disable it, boot out that service and
remove its plist from `~/Library/LaunchAgents`.
