#!/usr/bin/env bash
# Start a separate jcode agent session that brings every xenking fork up to
# upstream: jcode + Jcode Desktop (scripts/xk-upstream-sync.sh) and OpenRig
# (its scripts/xk-catchup.sh + xk-merge.sh). The session runs on the shared
# daemon, in a Rex tab when Rex is available so you can watch it, otherwise in
# the background with its log under ~/.jcode/logs/.
set -euo pipefail

usage() { echo "Usage: xk-upstream-agent.sh [--dry-run] [--foreground] [--model <model>]" >&2; exit 2; }
dry=false fg=false model=()
while (($#)); do
  case "$1" in
    --dry-run) dry=true ;;
    --foreground) fg=true ;;
    --model) (($# >= 2)) || usage; model=(-m "$2"); shift ;;
    *) usage ;;
  esac
  shift
done

ROOT=${XK_FORKS_DIR:-$HOME/Projects/github.com/xenking}
# Run the sync script as published on origin/master, never a copy from a
# checkout that may be stale or mid-edit by another agent.
git -C "$ROOT/jcode" fetch -q origin master
SYNC="$HOME/.jcode/upsync-bin/xk-upstream-sync.sh"
mkdir -p "$(dirname "$SYNC")"
git -C "$ROOT/jcode" show origin/master:scripts/xk-upstream-sync.sh > "$SYNC.tmp"
chmod 755 "$SYNC.tmp" && mv "$SYNC.tmp" "$SYNC"

read -r -d '' PROMPT <<EOF || true
Bring the xenking forks up to date with upstream. Work autonomously to the end, and report in Russian.

1. jcode + Jcode Desktop: run \`$SYNC\`. It merges upstream in worktrees under ~/.jcode/wt/upsync, runs the fork-feature checks and pushes to origin.
   - Exit 3 (conflicts): resolve them in the named worktree. Keep both sides' intent: upstream changes, plus every fork feature listed in AGENTS.md "Fork notes" and docs (codemode, MCP-Protocol-Version, Soniox voice, native ask tool, xenking self-update source, acp --session-env and reconnect, Rex terminal routing, shared-socket double lock, Darwin runtime_dir). Keep the "Fork notes" section last in AGENTS.md. \`git add\`, then rerun with \`--continue\`.
   - Exit 4 (checks): fix the cause in the worktree, commit, rerun with \`--continue\`. If the fork-removal guard fails, use docs/FORK_LEDGER.md to preserve the removal or its current owner. Never weaken or delete a test to get green.
2. OpenRig: in $ROOT/openrig (main, clean) run \`scripts/xk-catchup.sh\`. It opens a PR on xenking/openrig. Then merge it with \`scripts/xk-merge.sh <pr>\` using Node 24 (\`export PATH=\$HOME/.nvm/versions/node/v24.22.3/bin:\$PATH\`). Never touch upstream mvschwarz/openrig.
3. Do not roll out to the running daemon or Desktop, and do not reload anything. Finish by listing each fork's new origin head, how many upstream commits came in, the conflicts you resolved and how, and what the user should roll out (\`jcode rollout\` on Mac, \`/usr/local/bin/jcode-update\` and \`scripts/xk-update.sh\` on ai-core).
EOF

if $dry; then
  "$SYNC" --dry-run
  printf '\nWould start a jcode session with prompt:\n\n%s\n' "$PROMPT"
  exit 0
fi

cmd=(jcode run --no-update -C "$HOME" ${model[@]+"${model[@]}"} "$PROMPT")
if $fg; then exec "${cmd[@]}"; fi
if command -v rex >/dev/null 2>&1; then
  rex new "upstream-sync-$(date +%m%d-%H%M)" --keep-open -- "${cmd[@]}"
  echo "Started in Rex. Watch with: rex ls"
else
  log="$HOME/.jcode/logs/upstream-sync-$(date +%Y%m%d-%H%M).log"
  nohup "${cmd[@]}" >"$log" 2>&1 &
  echo "Started in background (pid $!). Log: $log"
fi
