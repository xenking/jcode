# Coordinated cloud migration

Run on Linux with working `bubblewrap`, GNU coreutils, `setsid`, Git, Python 3 and ripgrep. Build `jcode` from this checkout, then run:

```bash
V="$PWD/.jcode/skills/verify-jcode/scripts/verify-jcode"
E="${JCODE_SCRATCH_DIR:-$HOME/.jcode/scratch}/verify-cloud-group-$(date -u +%Y%m%dT%H%M%SZ)-$$"
"$V" cloud-group-return --binary "$BINARY" --evidence "$E"
```

This recipe needs no `launch`, `tmux`, account or shared daemon. It runs `scripts/test_cloud_group_e2e.sh` using the real CLI and two private daemons. It drives `cloud move` and `cloud return --session group-worker`, then checks the return receipt, session leases, Git work, persisted messages, todos and swarm assignments. The fixture simulates lost activation and return responses, a killed client and interrupted local apply. The verifier removes the private fixture and retains the logs and receipts in `$E`.

The transport fixture does not prove managed host resolution, SSH or real account credential sync. See `docs/cloud-acceptance.md` for those paths.
