# Desktop tabs during macOS rollout

Rollout and rollback of the app bundle stage each running Desktop process's
window manifests before requesting quit. They reopen the saved windows with
their own workspace snapshots, including the tab order, selection and unsent
drafts. This path does not use the 30-second crash-recovery cutoff. An idle
window's manifest remains valid for its process lifetime.

If a running Desktop process has no valid manifest, the rollout stops before
quitting it. The installed bundle is not replaced in that case. The handoff
uses the latest checkpoint written by Desktop, which normally updates once per
second. It cannot recover edits that have not reached that checkpoint.

## Open an archived workspace separately

`Resume` opens an individual session's history without replacing your current
workspace. It does not load the workspace layout or unsent drafts.

To view an entire archived workspace, use `--restore-state` in a Desktop
instance with separate `XDG_STATE_HOME`, `XDG_RUNTIME_DIR`, and
`JCODE_DESKTOP_RESTART_DIR`. Extract `.snapshot` from the archive into a new
owner-only file. The app consumes this file, so never pass the original
archive. Keep the working Desktop open and do not replace its recovery file.
Use a private `JCODE_HOME` and `JCODE_RUNTIME_DIR` as well if you need the
view to be completely disconnected from the live runtime.

## Regression check

Run `scripts/accept-rollout-tabs.py --old OLD_CLI --new CLI_UNDER_TEST
--bundle DESKTOP_APP` on macOS. It copies the app into a temporary directory
and verifies two native windows, ordered tabs, drafts, expired timestamps,
verification-triggered rollback, and refusal to quit without a manifest. The
local provider fixture must receive no turns. The check keeps the CLI daemon
generation fixed while exercising the app-bundle transaction.
