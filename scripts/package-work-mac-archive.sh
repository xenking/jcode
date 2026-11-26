#!/usr/bin/env bash
# Package the INSTALLED jcode + Jcode Desktop pair as a macOS arm64 release archive
# for another Mac: the server/CLI binary, the signed Jcode.app and INSTALL.md
# (written for a coding agent on the target machine). No credentials, config or
# sessions are copied. Usage: scripts/package-work-mac-archive.sh [OUT_DIR]
set -euo pipefail

out_dir="${1:-$HOME/Downloads}"
app="${JCODE_APP:-/Applications/Jcode.app}"
[ "$(uname -m)" = arm64 ] || { echo "run on the Apple Silicon Mac that owns the installed pair" >&2; exit 1; }

cli_line="$("$app/Contents/MacOS/jcode" --version)"            # jcode v1.1.1343-dev (3fae01587)
desktop_line="$("$app/Contents/MacOS/jcode-desktop" --version)" # Jcode Desktop v0.7.0 (3e4fd64b)
cli_sha="$(sed -n 's/.*(\([0-9a-f]\{7,\}\)).*/\1/p' <<<"$cli_line")"
desktop_sha="$(sed -n 's/.*(\([0-9a-f]\{7,\}\)).*/\1/p' <<<"$desktop_line")"
[ -n "$cli_sha" ] && [ -n "$desktop_sha" ] || { echo "cannot read build ids: $cli_line / $desktop_line" >&2; exit 1; }
binary="$HOME/.jcode/builds/versions/$cli_sha/jcode"
[ -x "$binary" ] || { echo "installed server binary missing: $binary" >&2; exit 1; }
"$binary" --version | grep -q "($cli_sha)" || { echo "$binary is not build $cli_sha" >&2; exit 1; }
codesign --verify --deep --strict "$app"

name="jcode-$cli_sha-desktop-$desktop_sha-macos-arm64"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
root="$stage/$name"
mkdir -p "$root/bin"
install -m 0755 "$binary" "$root/bin/jcode"
ditto --noextattr --noqtn "$app" "$root/Jcode.app"
rustls_cli="66991b3c8"; rustls_desktop="35eb66a8"

cat > "$root/VERSION.txt" <<EOF
server/CLI : $cli_line
desktop    : $desktop_line
built from : jcode $cli_sha, jcode-desktop $desktop_sha (the pair installed on the packaging Mac)
packaged   : $(date -u +%Y-%m-%dT%H:%M:%SZ)
arch       : arm64 (Apple Silicon) only
EOF

cat > "$root/INSTALL.md" <<EOF
# Install Jcode $cli_sha + Jcode Desktop $desktop_sha on a Mac (arm64)

Audience: a coding agent working on the target Mac. Execute the steps in order and
stop at the first failure. This archive holds **no credentials, config or sessions**;
log in on the target Mac (step 7). Paths below are for the current user (\`\$HOME\`).

## 0. Prerequisites
- Apple Silicon: \`uname -m\` prints \`arm64\`. macOS 14 or newer.
- Tools that ship with macOS: \`tar\`, \`shasum\`, \`codesign\`, \`xattr\`, \`ditto\`.
- No Rust toolchain is needed. Tailscale is needed only for step 8.
- Do not run the install while a session on this Mac is mid-turn: \`pgrep -fl 'jcode.*serve'\`
  shows the daemon; check with \`jcode usage\`/the Desktop that nothing is running first.

## 1. Unpack and verify
\`\`\`sh
mkdir -p ~/jcode-release && tar -xzf $name.tar.gz -C ~/jcode-release
cd ~/jcode-release/$name
shasum -a 256 -c SHA256SUMS            # every line must say OK
# A browser/Telegram download is quarantined: clear it for the unpacked tree.
xattr -dr com.apple.quarantine .
cat VERSION.txt
\`\`\`

## 2. Record the current install (rollback data)
\`\`\`sh
mkdir -p ~/.jcode/install-backup
{
  echo "date=\$(date -u +%FT%TZ)"
  echo "launcher=\$(readlink ~/.local/bin/jcode || true)"
  echo "current=\$(readlink ~/.jcode/builds/current/jcode || true)"
  echo "shared_server=\$(readlink ~/.jcode/builds/shared-server/jcode || true)"
  echo "app_backup=none"
} > ~/.jcode/install-backup/before-$cli_sha.txt
\`\`\`

## 3. Install the server/CLI binary (immutable version dir + channel symlinks)
\`\`\`sh
V=$cli_sha
mkdir -p ~/.jcode/builds/versions/\$V ~/.jcode/builds/current ~/.jcode/builds/shared-server ~/.local/bin
install -m 0755 bin/jcode ~/.jcode/builds/versions/\$V/jcode
codesign --verify ~/.jcode/builds/versions/\$V/jcode || codesign --force --sign - ~/.jcode/builds/versions/\$V/jcode
ln -sfn ~/.jcode/builds/versions/\$V/jcode ~/.jcode/builds/current/jcode
ln -sfn ~/.jcode/builds/versions/\$V/jcode ~/.jcode/builds/shared-server/jcode
ln -sfn ~/.jcode/builds/current/jcode ~/.local/bin/jcode        # launcher used from PATH
printf '%s' "\$V" > ~/.jcode/builds/current-version
printf '%s' "\$V" > ~/.jcode/builds/shared-server-version
\`\`\`
\`~/.local/bin\` must come **before** \`~/.cargo/bin\` in \`PATH\` (\`echo \$PATH | tr : '\n' | head\`).
Check: \`jcode --version\` prints \`jcode v… ($cli_sha)\`.

## 4. Install Jcode.app
\`\`\`sh
if [ -d /Applications/Jcode.app ]; then
  B=/Applications/Jcode.app.bak-\$(date +%Y%m%d-%H%M%S)
  osascript -e 'tell application "Jcode" to quit' 2>/dev/null || true
  mv /Applications/Jcode.app "\$B"
  sed -i '' "s|^app_backup=.*|app_backup=\$B|" ~/.jcode/install-backup/before-$cli_sha.txt
fi
ditto Jcode.app /Applications/Jcode.app
xattr -dr com.apple.quarantine /Applications/Jcode.app
codesign --verify --deep --strict /Applications/Jcode.app && echo "signature ok"
codesign -dv /Applications/Jcode.app 2>&1 | grep -E 'Identifier|TeamIdentifier'
/Applications/Jcode.app/Contents/MacOS/jcode-desktop --version   # Jcode Desktop … ($desktop_sha)
\`\`\`
The app is Developer-ID style signed (TeamIdentifier 5VZPN9L6US) with the hardened runtime but is
**not notarized**: if Gatekeeper still blocks the first launch, run
\`xattr -cr /Applications/Jcode.app\` and open it once with right-click → Open.
Never disable Gatekeeper or SIP, and do not re-sign the app: the bundled \`jcode\` and
\`jcode-harness-api-bridge\` binaries are sealed by the bundle signature.

## 5. Restart the daemon onto the new binary
\`\`\`sh
jcode server reload            # graceful, keeps sessions; use when a daemon is already running
# first install / no daemon:  jcode server start
pgrep -fl 'jcode.*serve'       # exactly ONE line, running .../versions/$cli_sha/jcode
\`\`\`
\`jcode server stop --force\` drops live headless sessions: use it only when you know none matter.

## 6. Login (credentials stay on this Mac)
\`jcode login --provider claude\` (repeat per provider/label, e.g. a work Enterprise account), then
\`jcode usage\` (must list each login; Claude accounts show resets and credits when the account has them).

## 7. Create a session end to end
\`\`\`sh
JCODE_TEST_SESSION=1 jcode run --no-update --provider claude "Reply with exactly: install-ok"
\`\`\`
Expected output \`install-ok\`. Then open Jcode.app and start a session in a project directory.

## 8. Pair a remote Desktop with this Mac (this Mac = the server that owns the projects)
Follow \`docs/PAIRED_DESKTOP.md\` of the jcode repository; condensed:
1. In \`~/.jcode/config.toml\`:
   \`\`\`toml
   [gateway]
   enabled = true
   bind_addr = "127.0.0.1"
   port = 7643
   harness_enabled = true
   \`\`\`
2. Restart the daemon (\`jcode server reload\`), then \`curl --fail http://127.0.0.1:7643/health\`.
3. Expose it only inside the tailnet: \`tailscale serve status\`, then
   \`tailscale serve --https=8443 http://127.0.0.1:7643\`. No Funnel, no disabled certificate checks.
4. In a private terminal: \`jcode pair --harness\` → a short-lived one-use invite (a credential: do not paste it into
   tickets or scripts).
5. On the other Mac's Jcode Desktop: Machines → Pair a machine → the HTTPS origin from step 3, a label, an absolute
   project path on this Mac, the invite → Pair and save. Open a session and confirm it runs on this Mac.
Revoke: \`jcode pair --list\`, \`jcode pair --revoke <id>\`, \`tailscale serve --https=8443 off\`.

The rustls CryptoProvider panic that made the paired \`wss\` connect fail ("ring + aws-lc-rs" both enabled, no
default provider) is fixed in this pair: jcode commit \`$rustls_cli\` ("install a rustls provider before the paired
Harness wss connect") and Jcode Desktop commit \`$rustls_desktop\` ("lock rustls ring for paired Harness provider"),
both contained in jcode $cli_sha / Desktop $desktop_sha.

## 9. MCP tool approvals
A tool that an MCP server marks as non-read-only (for example a review server) asks before it runs:
the card offers **Allow once**, **Always allow this tool**, **Always allow every tool on this server** and **Deny**.
The two "Always" answers are written to the MCP file the server is read from (normally \`~/.jcode/mcp.json\`):
the raw tool name in that server's \`approve_free\` list, or \`"*"\` there for every tool on the server. They apply at
once to the running daemon and every session (the policy is re-read on every call). List or undo them with
\`jcode mcp approvals\` (\`jcode mcp approvals revoke <server> [<tool>]\`). A background session that hits a gated
tool shows the request on the attached client (the daemon log has one \`MCP_APPROVAL outcome=… session=… server=…
tool=…\` line per request); with no client attached it waits up to 30 s for one (\`JCODE_MCP_APPROVAL_ATTACH_WAIT_SECS\`,
\`0\` refuses at once), then refuses with the \`jcode mcp approvals allow <server> [<tool>]\` hint instead of hanging.
If the work installer rewrites \`mcp.json\`, re-run \`jcode mcp approvals\` to see which grants survived. Settings written under \`[mcp.servers.*]\` in
\`config.toml\` are not read: jcode warns at startup and names \`~/.jcode/mcp.json\` as the file that is.
The work overlay's installer pre-approves only its local-only review tools; anything with an external write
(posting or approving a merge request, Slack writes, \`orch_approve\`) still asks, once, until you choose "Always".

## 10. Verification checklist (all must hold)
- [ ] \`jcode --version\` → \`($cli_sha)\`; \`/Applications/Jcode.app/Contents/MacOS/jcode --version\` → \`($cli_sha)\`
- [ ] \`jcode-desktop --version\` → \`($desktop_sha)\`; \`codesign --verify --deep --strict /Applications/Jcode.app\` exits 0
- [ ] \`pgrep -fl 'jcode.*serve'\` shows exactly one daemon, from \`versions/$cli_sha\`
- [ ] \`jcode usage\` runs and lists the logins you added
- [ ] step 7 prints \`install-ok\`; a Desktop session opens and answers
- [ ] after \`jcode server stop --force && jcode server start\` no session starts a turn by itself

## 11. Rollback
\`\`\`sh
cat ~/.jcode/install-backup/before-$cli_sha.txt        # previous targets + app backup path
PREV=\$(sed -n 's/^current=//p' ~/.jcode/install-backup/before-$cli_sha.txt)
[ -n "\$PREV" ] && ln -sfn "\$PREV" ~/.jcode/builds/current/jcode \\
  && ln -sfn "\$PREV" ~/.jcode/builds/shared-server/jcode \\
  && ln -sfn ~/.jcode/builds/current/jcode ~/.local/bin/jcode
APP=\$(sed -n 's/^app_backup=//p' ~/.jcode/install-backup/before-$cli_sha.txt)
if [ -n "\$APP" ] && [ "\$APP" != none ] && [ -d "\$APP" ]; then rm -rf /Applications/Jcode.app && mv "\$APP" /Applications/Jcode.app; fi
jcode server stop --force; jcode server start
\`\`\`
If there was no previous install, remove \`~/.local/bin/jcode\`, \`~/.jcode/builds\` and \`/Applications/Jcode.app\`;
\`~/.jcode\` otherwise holds only what this Mac created (logins, sessions) and is left alone.
EOF

(cd "$root" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 shasum -a 256 > SHA256SUMS)
mkdir -p "$out_dir"
archive="$out_dir/$name.tar.gz"
COPYFILE_DISABLE=1 tar --no-xattrs -czf "$archive" -C "$stage" "$name"

# Verify what was written: extract elsewhere, run the binaries, check signatures and sums.
check="$(mktemp -d)"
tar -xzf "$archive" -C "$check"
( cd "$check/$name" && shasum -a 256 -c SHA256SUMS >/dev/null )
"$check/$name/bin/jcode" --version | grep -q "($cli_sha)"
"$check/$name/Jcode.app/Contents/MacOS/jcode-desktop" --version | grep -q "($desktop_sha)"
codesign --verify --deep --strict "$check/$name/Jcode.app"
rm -rf "$check"
echo "archive : $archive"
echo "size    : $(du -h "$archive" | cut -f1) ($(stat -f%z "$archive") bytes)"
echo "sha256  : $(shasum -a 256 "$archive" | cut -d' ' -f1)"
