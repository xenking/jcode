#!/usr/bin/env bash
# Run the real sync entry point against local-only Git remotes.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
jcode=$(dirname "$here")
desktop=${1:-$(dirname "$jcode")/jcode-desktop}
tmp=$(mktemp -d "${JCODE_SCRATCH_DIR:-${TMPDIR:-/tmp}}/fork-removals.XXXXXXXX")
trap 'rm -rf "$tmp"' EXIT
export GIT_ALLOW_PROTOCOL=file

forks="$tmp/forks"
origins="$tmp/github.com/xenking"
mkdir -p "$forks" "$origins"
removed=.github/workflows/discord-release.yml

make_fork() {
  local name=$1 source=$2 branch=$3 evidence=$4
  local checkout="$forks/$name" origin="$origins/$name.git" upstream="$tmp/upstream-$name.git"
  local base
  base=$(git -C "$source" rev-parse "origin/$branch")
  git clone -q --shared --bare "$source" "$origin"
  git --git-dir="$origin" update-ref "refs/heads/$branch" "$base"
  git --git-dir="$origin" symbolic-ref HEAD "refs/heads/$branch"
  git clone -q --shared "$origin" "$checkout"
  git -C "$checkout" config user.name 'Fork removal fixture'
  git -C "$checkout" config user.email 'fixture@example.invalid'
  git -C "$checkout" config commit.gpgsign false
  git -C "$checkout" config core.hooksPath /dev/null
  git -C "$checkout" switch -q -c incoming
  git -C "$checkout" show "$evidence^:$removed" > "$checkout/$removed"
  git -C "$checkout" add -- "$removed"
  git -C "$checkout" commit -qm 'Fixture upstream reintroduces a retired workflow'
  git clone -q --shared --bare "$checkout" "$upstream"
  git --git-dir="$upstream" update-ref "refs/heads/$branch" "$(git -C "$checkout" rev-parse HEAD)"
  git -C "$checkout" remote add upstream "$upstream"

  git -C "$checkout" switch -q "$branch"
  # The production sync script takes its guard from the published CLI fork.
  if [ "$name" = jcode ]; then
    cp "$here/check_fork_removals.py" "$checkout/scripts/check_fork_removals.py"
    git -C "$checkout" add scripts/check_fork_removals.py
  fi
  git -C "$checkout" commit -qm 'Fixture fork advances independently' --allow-empty
  git -C "$checkout" push -q origin "HEAD:$branch"
}

expect_rejected() {
  local repo=$1 name=$2 branch=$3 before after output code
  local syncdir="$tmp/sync-$repo"
  before=$(git --git-dir="$origins/$name.git" rev-parse "refs/heads/$branch")
  code=0
  output=$(XK_FORKS_DIR="$forks" XK_SYNC_DIR="$syncdir" "$here/xk-upstream-sync.sh" \
    --repo "$repo" --skip-checks 2>&1) || code=$?
  printf '%s\n' "$output"
  [ "$code" -eq 4 ]
  [[ "$output" == *"retired path returned: $removed"* ]]
  [ "$(git -C "$syncdir/$name" rev-list --parents -n 1 HEAD | awk '{ print NF }')" -eq 3 ]
  git -C "$syncdir/$name" cat-file -e "HEAD:$removed"
  after=$(git --git-dir="$origins/$name.git" rev-parse "refs/heads/$branch")
  [ "$before" = "$after" ]

  git -C "$syncdir/$name" rm -q -- "$removed"
  git -C "$syncdir/$name" commit -qm 'Fixture resolves the returned workflow'
  XK_FORKS_DIR="$forks" XK_SYNC_DIR="$syncdir" "$here/xk-upstream-sync.sh" \
    --repo "$repo" --continue --skip-checks --no-push
}

expect_late_origin_rejected() {
  local output code=0
  mkdir -p "$tmp/bin"
  cat > "$tmp/bin/justrust" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [ ! -e "$RACE_TIP" ]; then
  git -C "$RACE_CHECKOUT" show "c7614da09^:$RACE_PATH" > "$RACE_CHECKOUT/$RACE_PATH"
  git -C "$RACE_CHECKOUT" add -- "$RACE_PATH"
  git -C "$RACE_CHECKOUT" commit -qm 'Fixture concurrent origin advance returns a workflow'
  git -C "$RACE_CHECKOUT" push -q origin HEAD:master
  git -C "$RACE_CHECKOUT" rev-parse HEAD > "$RACE_TIP"
fi
EOF
  chmod +x "$tmp/bin/justrust"
  output=$(PATH="$tmp/bin:$PATH" RACE_CHECKOUT="$forks/jcode" RACE_PATH="$removed" RACE_TIP="$tmp/race-tip" \
    XK_FORKS_DIR="$forks" XK_SYNC_DIR="$tmp/sync-jcode" "$here/xk-upstream-sync.sh" \
    --repo jcode --continue 2>&1) || code=$?
  printf '%s\n' "$output"
  [ "$code" -eq 4 ]
  [[ "$output" == *"retired path returned: $removed"* ]]
  [ "$(git -C "$tmp/sync-jcode/jcode" rev-parse HEAD^2)" = "$(< "$tmp/race-tip")" ]
  [ "$(git --git-dir="$origins/jcode.git" rev-parse refs/heads/master)" = "$(< "$tmp/race-tip")" ]
}

make_fork jcode "$jcode" master c7614da09
make_fork jcode-desktop "$desktop" main c256d495
expect_rejected jcode jcode master
expect_rejected desktop jcode-desktop main
expect_late_origin_rejected
printf '%s\n' 'Fork removals fixtures: clean upstream and late origin merges rejected, corrected commits passed.'
