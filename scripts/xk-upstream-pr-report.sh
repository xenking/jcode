#!/usr/bin/env bash
# Install or run the report-only upstream PR LaunchAgent.
set -euo pipefail

label=com.xenking.upstream-pr-report
state=${XK_PR_REPORT_DIR:-$HOME/.jcode/upstream-pr-report}
root=${XK_FORKS_DIR:-$HOME/Projects/github.com/xenking}
plist="$HOME/Library/LaunchAgents/$label.plist"
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
export GH_HOST=github.com GH_PROMPT_DISABLED=1 GIT_TERMINAL_PROMPT=0

case "${1:---run}" in
  --run)
    [ "$#" -le 1 ] || exit 2
    # Update remote-tracking refs only. Neither checkout, index nor branch changes.
    git -C "$root/jcode" fetch -q --no-tags --no-write-fetch-head origin master
    git -C "$root/jcode-desktop" fetch -q --no-tags --no-write-fetch-head origin main
    published=$(git -C "$root/jcode" rev-parse --verify refs/remotes/origin/master)
    git -C "$root/jcode" show "$published:scripts/xk-upstream-prs.py" |
      python3 - --forks "$root" --ledger-ref "$published" status --report "$state/status.md" --notify
    ;;
  --install)
    [ "$#" -eq 1 ] || exit 2
    [ "$(uname -s)" = Darwin ] || { echo "LaunchAgent requires macOS." >&2; exit 2; }
    domain="gui/$(id -u)"
    launchctl print "$domain" >/dev/null
    mkdir -p "$state" "$(dirname "$plist")"
    runner="$state/run.sh"
    if [ "$0" != "$runner" ]; then
      install -m 700 "$0" "$runner.new"
      mv "$runner.new" "$runner"
    fi
    python3 - "$plist" "$label" "$runner" "$root" "$state" <<'PY'
import os
from pathlib import Path
import plistlib
import sys
import tempfile

path, label, runner, root, state = sys.argv[1:]
job = {
    "Label": label,
    "ProgramArguments": ["/bin/bash", runner, "--run"],
    "EnvironmentVariables": {"XK_FORKS_DIR": root, "XK_PR_REPORT_DIR": state},
    "StartCalendarInterval": {"Hour": 9, "Minute": 0},
    "RunAtLoad": True,
    "StandardOutPath": "/dev/null",
    "StandardErrorPath": str(Path(state) / "errors.log"),
}
with tempfile.NamedTemporaryFile(dir=Path(path).parent, delete=False) as output:
    temporary = output.name
    try:
        plistlib.dump(job, output)
        output.close()
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)
PY
    plutil -lint "$plist"
    if launchctl print "$domain/$label" >/dev/null 2>&1; then
      launchctl bootout "$domain/$label"
    fi
    launchctl bootstrap "$domain" "$plist"
    printf 'Scheduled at 09:00 local. Report: %s\n' "$state/status.md"
    ;;
  *) printf 'Usage: %s [--run|--install]\n' "$0" >&2; exit 2 ;;
esac
