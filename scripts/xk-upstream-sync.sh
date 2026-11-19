#!/usr/bin/env bash
# Merge upstream (1jehuang) into the xenking jcode and Jcode Desktop forks.
#
# Each repo is merged in its own worktree under $XK_SYNC_DIR (default
# ~/.jcode/wt/upsync), never in the shared checkouts. The two worktrees sit side
# by side as jcode/ and jcode-desktop/, because Desktop's crates depend on
# ../../../jcode by path, so Desktop is checked against the freshly merged CLI.
#
# Exit codes: 0 synced (or nothing to do), 2 usage, 3 merge conflict (the
# worktree is left mid-merge for a person or agent to resolve, then rerun with
# --continue), 4 checks failed (worktree left for fixing), 5 push refused.
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
Usage: xk-upstream-sync.sh [--dry-run] [--repo jcode|desktop|all] [--continue] [--no-push] [--skip-checks]
  --dry-run      report how far each fork is behind upstream, change nothing
  --repo         which fork to sync (default all; desktop always builds against the synced jcode)
  --continue     resume after you resolved conflicts or fixed checks in the left-over worktree
  --no-push      merge and check, but leave the result in the worktree
  --skip-checks  merge and push without builds/tests (only for doc-only upstream changes)
EOF
  exit 2
}

dry=false repo=all resume=false push=true checks=true
while (($#)); do
  case "$1" in
    --dry-run) dry=true ;;
    --repo) (($# >= 2)) || usage; repo=$2; shift ;;
    --continue) resume=true ;;
    --no-push) push=false ;;
    --skip-checks) checks=false ;;
    -h|--help) usage ;;
    *) usage ;;
  esac
  shift
done
case "$repo" in jcode|desktop|all) ;; *) usage ;; esac

ROOT=${XK_FORKS_DIR:-$HOME/Projects/github.com/xenking}
WT=${XK_SYNC_DIR:-$HOME/.jcode/wt/upsync}
STAMP=$(date -u +%Y%m%d)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$HOME/.jcode/shared-build/target}
export DEVELOPER_DIR=${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}

log() { printf '[xk-upstream-sync] %s\n' "$*"; }
die() { local code=$1; shift; printf '[xk-upstream-sync] %s\n' "$*" >&2; exit "$code"; }

# name dir branch upstream-url
fork_info() {
  case "$1" in
    jcode) echo "$ROOT/jcode master https://github.com/1jehuang/jcode.git" ;;
    desktop) echo "$ROOT/jcode-desktop main https://github.com/1jehuang/jcode-desktop.git" ;;
  esac
}

check_remotes() {
  local name=$1 src branch up origin
  read -r src branch up < <(fork_info "$name")
  git -C "$src" rev-parse --git-dir >/dev/null 2>&1 || die 1 "$name: no checkout at $src"
  origin=$(git -C "$src" remote get-url origin)
  case "$origin" in *github.com[:/]xenking/*) ;; *) die 1 "$name: origin is not a xenking fork: $origin" ;; esac
  git -C "$src" remote get-url upstream >/dev/null 2>&1 || git -C "$src" remote add upstream "$up"
  # Fetch-only upstream: a push there must be impossible, not merely avoided.
  git -C "$src" remote set-url --push upstream DISABLED-never-push-to-upstream
  git -C "$src" fetch -q origin "$branch"
  git -C "$src" fetch -q upstream "$branch"
}

behind() {
  local src branch _up
  read -r src branch _up < <(fork_info "$1")
  git -C "$src" rev-list --count "origin/$branch..upstream/$branch"
}

# Worktree for $name at $WT/<dir>; the merge branch is xk/upsync-<name>-<date>.
ensure_worktree() {
  local name=$1 src branch _up dir
  read -r src branch _up < <(fork_info "$name")
  dir="$WT/$(basename "$src")"
  if [ -e "$dir" ]; then
    $resume || die 1 "$dir exists from an earlier run. Resolve it and rerun with --continue, or remove it: git -C $src worktree remove --force $dir"
    return
  fi
  mkdir -p "$WT"
  git -C "$src" worktree add -q -b "xk/upsync-$name-$STAMP" "$dir" "origin/$branch"
}

merge_upstream() {
  local name=$1 src branch _up dir
  read -r src branch _up < <(fork_info "$name")
  dir="$WT/$(basename "$src")"
  if git -C "$dir" rev-parse -q --verify MERGE_HEAD >/dev/null; then
    keep_fork_workflow_deletions "$dir"
    local left
    left=$(git -C "$dir" diff --name-only --diff-filter=U)
    [ -z "$left" ] || die 3 "$name: unresolved conflicts remain in $dir:"$'\n'"$left"
    git -C "$dir" commit -q --no-edit
    log "$name: committed the resolved merge"
    return
  fi
  if git -C "$dir" merge-base --is-ancestor "upstream/$branch" HEAD; then
    log "$name: upstream/$branch already merged in $dir"
    return
  fi
  if ! git -C "$dir" merge -q --no-edit "upstream/$branch"; then
    keep_fork_workflow_deletions "$dir"
    local left
    left=$(git -C "$dir" diff --name-only --diff-filter=U)
    [ -z "$left" ] || die 3 "$name: merge conflicts in $dir. Resolve them (keep fork features, see AGENTS.md Fork notes), git add, then rerun with --continue:"$'\n'"$left"
    git -C "$dir" commit -q --no-edit
  fi
  log "$name: merged $(git -C "$dir" rev-list --count "HEAD^1..upstream/$branch") upstream commits"
}

# The forks removed upstream CI workflows on purpose (no Actions budget, no
# cron). Upstream edits to a workflow the fork deleted conflict as
# deleted-by-us; keep the deletion. Anything else stays a real conflict.
keep_fork_workflow_deletions() {
  local dir=$1 f
  git -C "$dir" status --porcelain | while IFS= read -r line; do
    f=${line:3}
    if [ "${line:0:2}" = DU ] && [[ "$f" == .github/workflows/* ]]; then
      git -C "$dir" rm -q -- "$f"
      log "kept fork deletion of $f"
    fi
  done
}

run_checks() {
  local name=$1
  $checks || { log "$name: checks skipped (--skip-checks)"; return; }
  local fail=false
  run() { log "$name: $*"; "$@" || fail=true; }
  if [ "$name" = jcode ]; then
    (
      cd "$WT/jcode"
      run justrust check -p jcode --bins
      # Fork-only features (AGENTS.md "Fork notes") and the stack fixes they rely on.
      run justrust test -p jcode-codemode
      run justrust test -p jcode-voice --lib --features desktop-test-support
      run justrust test -p jcode-base --lib mcp
      run justrust test -p jcode-app-core --lib tool::ask
      run justrust test -p jcode-app-core --lib server::socket
      run justrust test -p jcode-storage --test runtime_dir
      run justrust test -p jcode-terminal-launch rex
      run justrust test -p jcode --lib acp
      run justrust clippy -p jcode-app-core -p jcode-codemode -p jcode-voice --all-targets -- -D warnings
      ! $fail
    ) || die 4 "jcode: checks failed in $WT/jcode. Fix, commit, then rerun with --continue."
  else
    (
      cd "$WT/jcode-desktop"
      run justrust check -p jcode-desktop
      run justrust test -p jcode-desktop-harness
      run justrust test -p jcode-desktop-openrig
      run justrust test -p jcode-desktop-voice
      ! $fail
    ) || die 4 "desktop: checks failed in $WT/jcode-desktop. Fix, commit, then rerun with --continue."
  fi
}

ship() {
  local name=$1 src branch _up dir wbranch
  read -r src branch _up < <(fork_info "$name")
  dir="$WT/$(basename "$src")"
  if ! $push; then log "$name: --no-push, result left in $dir"; return; fi
  git -C "$dir" fetch -q origin "$branch"
  if ! git -C "$dir" merge-base --is-ancestor "origin/$branch" HEAD; then
    git -C "$dir" merge -q --no-edit "origin/$branch" ||
      die 3 "$name: origin/$branch moved and conflicts with the merge in $dir; resolve, then --continue"
  fi
  git -C "$dir" push -q origin "HEAD:$branch" || die 5 "$name: push to origin/$branch refused"
  log "$name: pushed $(git -C "$dir" rev-parse --short HEAD) to origin/$branch"
}

cleanup() {
  local name=$1 src _b _up dir wbranch
  read -r src _b _up < <(fork_info "$name")
  dir="$WT/$(basename "$src")"
  [ -e "$dir" ] || return 0
  wbranch=$(git -C "$dir" branch --show-current)
  git -C "$src" worktree remove --force "$dir"
  # -d, not -D: after a push the branch is merged into origin, so a refusal means something was not shipped.
  [ -z "$wbranch" ] || git -C "$src" branch -q -d "$wbranch"
}

names=()
case "$repo" in jcode) names=(jcode) ;; desktop) names=(desktop) ;; all) names=(jcode desktop) ;; esac

for n in "${names[@]}"; do check_remotes "$n"; done
if $dry; then
  for n in "${names[@]}"; do
    read -r src branch _up < <(fork_info "$n")
    log "$n: upstream/$branch is $(behind "$n") commits ahead of origin/$branch; fork has $(git -C "$src" rev-list --count "upstream/$branch..origin/$branch") own commits"
  done
  exit 0
fi

synced=()
for n in "${names[@]}"; do
  if ! $resume && [ "$(behind "$n")" = 0 ]; then
    log "$n: nothing new upstream"
    continue
  fi
  ensure_worktree "$n"
  merge_upstream "$n"
  synced+=("$n")
done

# Desktop builds against ../jcode: give it the merged CLI, or origin/master if the CLI had nothing to merge.
if [[ " ${synced[*]+${synced[*]}} " == *" desktop "* ]] && [ ! -e "$WT/jcode" ]; then
  git -C "$ROOT/jcode" worktree add -q --detach "$WT/jcode" origin/master
fi

# ${synced[@]+...}: bash 3.2 (macOS) treats an empty array as unset under set -u.
for n in ${synced[@]+"${synced[@]}"}; do run_checks "$n"; done
for n in ${synced[@]+"${synced[@]}"}; do ship "$n"; done
if $push; then
  for n in ${synced[@]+"${synced[@]}"}; do cleanup "$n"; done
  [ ! -e "$WT/jcode" ] || git -C "$ROOT/jcode" worktree remove --force "$WT/jcode"
  rmdir "$WT" 2>/dev/null || true
fi
log "done: ${synced[*]+${synced[*]}}"
