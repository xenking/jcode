# macOS computer-use acceptance

Run from an unlocked macOS GUI login with Python 3 and the Xcode Command Line
Tools. Supply the three executables from the revisions you want to verify:

```sh
python3 scripts/accept-macos-computer-use.py \
  --jcode /path/to/jcode \
  --bridge /path/to/jcode-harness-api-bridge \
  --desktop /path/to/jcode-desktop
```

The runner starts a private `jcode serve`, its regular harness API bridge and
one Jcode Desktop process. The model fixture listens on loopback. No inference
service, shared daemon, existing history or installation is used.

When the runner prints `PENDING`, check the session ID in the **Computer use**
sheet in the test Desktop. Choose **Allow for this session** for the first
session and **Deny** for the second. The runner never chooses those buttons,
answers a macOS permission prompt, invokes interactive `setup` or changes TCC.

The checks verify the following:

- Without an open Desktop, a real daemon turn fails closed.
- The new session asks separately, its denied work does not execute, and the
  next tool call stays denied without a prompt.
- The allowed session observes, edits, waits and reads back the fixture in the
  background without asking again. The decoy window stays unchanged. The
  receipts report the test Desktop's PID, not the daemon's.
- Vision OCR reads the target marker and updated value from the exact window
  image returned by the tool. It rejects the decoy marker.
- Core Graphics keyboard input reaches the fixture only after it confirms its
  own window and field are focused. AX reads back the result.

Use `--no-desktop` with only `--jcode` to run the missing-broker check. It should
fail on old builds that ask for consent in the daemon and would execute there.

## Interpret the result

`0` means all selected checks passed. `77` means the Aqua session was locked
or a macOS permission prevented execution and the refusal was verified. `1`
means a check failed or the runner could not establish the isolated GUI session.

`desktop_unavailable` means no compatible local Desktop window has the calling
session open. `consent_required` and `consent_denied` come from the Desktop
sheet, not macOS. After an approval, `check_permissions` reports Accessibility
and Screen Recording independently. With no Accessibility grant the AX action
must return `delivery=not_sent`. With no Screen Recording grant the image
observation must not return an image. Do not revoke any production permissions
to exercise these cases. Use a disposable GUI login or ungranted launch context.

If a grant is missing, grant it manually in **System Settings > Privacy &
Security** to the identity macOS attributes to the Desktop executor, not to the
fixture or daemon. The runner prints the Desktop PID. Find it in TCC's
`AUTHREQ_ATTRIBUTION` and follow the `msgID` to `AUTHREQ_SUBJECT` and
`AUTHREQ_RESULT` for the relevant service:

```sh
/usr/bin/log show --last 5m --info --style compact \
  --predicate 'subsystem == "com.apple.TCC" AND eventMessage CONTAINS "<Desktop PID>"'
```

## Historical direct-execution baseline

On November 22, 2026 (Australia/Sydney), installed Jcode `97e3102a9` passed the
old daemon-only AX and screen checks on macOS `27.0.1` when launched via Rex.
macOS attributed both grants to **Rex Server.app** (`com.superlogical.rex.server`).
An ungranted Aqua job instead returned `77` with the expected AX refusal. No
Rex grant was changed. This baseline did not exercise Desktop. Do not infer
current Desktop TCC grants from it.
