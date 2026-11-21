# macOS computer-use acceptance

Run from an unlocked macOS GUI login with Python 3 and the Xcode Command Line Tools.
Use the executable and launch context you want to verify:

```sh
python3 scripts/accept-macos-computer-use.py --jcode "$(command -v jcode)" --allow-fixture
```

The runner starts one isolated `jcode serve` with a private home, socket, and
loopback provider. It drives real subscribed session turns without using an
inference service, the shared daemon, Desktop, or existing session history.
It tests a declined computer-use request first. `--allow-fixture` authorizes
the runner to answer Jcode's consent request in a second synthetic session.
It never answers a macOS permission dialog, calls interactive `setup`, or
changes a TCC grant.

The fixture opens two non-activating AppKit windows. Jcode receives only the
fixture PID and target window ID. The checks cover AX observation, an `act`
write, matching and non-matching `wait`, a fresh AX readback, and independent
readback from the fixture. The target must remain in the background and the
decoy must stay unchanged. For the image check, Vision OCR reads only the
image attached to the window-scoped `observe` tool result. The test requires
the target marker and updated text and rejects the decoy marker. It does not
call legacy `ocr`, which captures the display even when given a region.

## Interpret the result

| Exit | Meaning |
| --- | --- |
| `0` | The session denial, permission status, AX/write/wait/readback, and fixture image/OCR checks passed. |
| `77` | The positive path was not attempted without `--allow-fixture`, or a required macOS permission was missing and the matching refusal was verified. |
| `1` | The test could not establish its fixture/session or an observation disagreed with the contract. |

To exercise macOS refusals without revoking production permissions, run the
same command in a disposable GUI login or in a fresh, ungranted launch context
in the current Aqua domain. With no Accessibility grant, expect
`accessibility_permission_required`, `delivery=not_sent`, no image, and an
unchanged fixture. With Accessibility but no Screen Recording, expect the AX
checks to pass and `screen_recording_permission_required` with no image.

On a Rex-launched Mac, this one-shot Aqua job can reproduce the ungranted
case without using Rex as the responsible process. Run from the repo root:

```sh
work=$(mktemp -d)
label=dev.jcode.acceptance.ax-denied.$$
domain=gui/$(id -u)
python3 - "$work" "$label" "$(command -v jcode)" <<'PY'
from pathlib import Path
import plistlib
import sys

work, label, binary = sys.argv[1:]
root = Path(work)
with (root / "job.plist").open("wb") as job:
    plistlib.dump({
        "Label": label, "RunAtLoad": True,
        "ProgramArguments": ["/usr/bin/python3",
                             str(Path("scripts/accept-macos-computer-use.py").resolve()),
                             "--jcode", str(Path(binary).resolve()), "--allow-fixture"],
        "StandardOutPath": str(root / "stdout"), "StandardErrorPath": str(root / "stderr"),
    }, job)
PY
launchctl bootstrap "$domain" "$work/job.plist"
```

After the job exits, `launchctl print "$domain/$label"` should show one run
with exit code `77` and `$work/stdout` should report `PASS AX permission refusal`.
Remove only this job with `launchctl bootout "$domain/$label"`. If that
executable already has both grants in this context, use a disposable GUI login
instead of removing a grant.

`consent_required` is not a macOS permission status. It means the calling
Jcode session has no attached user route for computer-use consent. Attach a
user client to that session and grant session consent before diagnosing TCC.
`consent_denied` means the session refused. After session consent,
`check_permissions` reports Accessibility and Screen Recording independently
without opening Settings. It does not prove that a real window operation works.

If a grant is actually missing, grant it manually in **System Settings →
Privacy & Security → Accessibility** or **Screen & System Audio Recording**
to the identity macOS attributes to the tool executor, not to the fixture.
The runner prints the executor PID. In the TCC unified log, find
`AUTHREQ_ATTRIBUTION` mentioning that PID and follow its `msgID` to
`AUTHREQ_SUBJECT` and `AUTHREQ_RESULT` for the relevant service:

```sh
/usr/bin/log show --last 5m --info --style compact \
  --predicate 'subsystem == "com.apple.TCC" AND eventMessage CONTAINS "<executor PID>"'
```

## Recorded run, November 22, 2026 (Australia/Sydney)

- Installed Jcode `97e3102a9`, macOS `27.0.1`. The Rex-launched Aqua acceptance
  passed on exit `0`. TCC attributed both `kTCCServiceAccessibility` and
  `kTCCServiceScreenCapture` to **Rex Server.app** (`com.superlogical.rex.server`),
  both already granted. No new grant was required in that context.
- The one-shot Aqua job reported both grants missing, returned `77`, and
  verified the Accessibility refusal without revoking either Rex grant.
  TCC attributed its Accessibility preflight to the versioned `jcode`
  executable itself. The AX-granted, screen-denied combination was not
  exercised on this Mac.
- The active headless worker separately returned `consent_required` before
  reaching TCC. The installed Desktop `8adf0b38` was identified, not driven by
  this CLI acceptance. Do not infer Desktop bridge or other process grants
  from the Rex run.
