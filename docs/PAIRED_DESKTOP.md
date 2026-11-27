# Connect Desktop to a paired Jcode server

Run Jcode on the Mac that owns the projects. Run Desktop on the Mac you use. Both need compatible builds with Harness pairing. You do not need Desktop, SSH, or OpenRig on the remote Mac for this connection.

## Prepare the remote Mac

1. Confirm that your Tailnet administrator allows HTTPS Serve for this Mac. Limit access to the client devices that should control this daemon. Check `tailscale serve status` first and choose an unused HTTPS port. Do not alter the Herdr relay.
2. Use a separate Jcode home and daemon for the first test. In that home's `config.toml`, set:

   ```toml
   [gateway]
   enabled = true
   bind_addr = "127.0.0.1"
   port = 7643
   harness_enabled = true
   max_harness_connections = 16   # optional: one WebSocket per open remote session
   ```

3. Start the daemon with `jcode --socket "$JCODE_HOME/daemon.sock" serve`. Use a short absolute `JCODE_HOME` path so the Unix socket path fits. Confirm that the new loopback port answers `curl --fail http://127.0.0.1:7643/health`. An existing wildcard phone gateway is not a paired Desktop endpoint.
4. If your Tailnet permits it, run `tailscale serve --https=8443 http://127.0.0.1:7643` for an initial foreground test. Use the HTTPS origin printed by Serve. Do not use Funnel or disable certificate verification.
5. In a private terminal, run `jcode pair --harness` with the same `JCODE_HOME`. Treat the short-lived, one-use invite as a credential. Don't send it in a ticket or store it in a shell script. Pair Desktop only to the exact HTTPS origin you confirmed in step 4.

## Connect from Desktop

1. In **Machines**, choose **Pair a machine**. Enter the HTTPS origin, a label such as `Work Mac`, an absolute default project path on that Mac, and the invite. Choose **Pair and save**. Desktop stores the token in macOS Keychain.
2. Choose **Sessions** to open an existing session. Each paired machine row has a **Project** path. Edit it and choose **Save** to change where new sessions start, or edit it and choose **Connect** to open one session in another project without saving.
3. Confirm that a session opens from the remote machine, not a local checkout with a similar path. Disconnect and reconnect to check that Desktop opens the same session ID.

## When a connect fails

Desktop shows the cause inside the machine's row, with the next step:

| Cause | What you see | Action |
| --- | --- | --- |
| The name does not resolve (Tailscale or the VPN is off) | Can't reach the machine (name did not resolve) | Retry |
| Nothing listens, or the host is asleep | Connection refused, or no answer in time | Retry |
| The certificate or address is wrong | Can't open a secure connection | Retry, or Re-pair with the right address |
| The token was revoked or is unknown (HTTP 401 or 403) | The machine no longer accepts this device | Re-pair |
| The address does not serve Jcode (HTTP 404) | The saved address does not serve Jcode | Re-pair |
| The gateway has no `harness_v1` | Its Jcode is too old for Desktop | Update Jcode there, then Retry |

**Details** shows the technical cause. **Re-pair** keeps the machine's id, label and project path and replaces its token, so sessions and the default keep pointing at it. A failed name lookup fails at once. Other failures end within the 15 second connect deadline.

## Remove access

Use `jcode pair --list` and `jcode pair --revoke <device-id>` on the remote Mac. Use the exact ID from the list. Then stop the foreground Serve command, or use `tailscale serve --https=8443 off` if you left that listener running. Remove only this listener, not the existing Serve configuration. Stop only the test daemon if you no longer need it.

A paired device can control sessions on that daemon with the daemon user's authority. Pairing is not a per-project permission or a sandbox. Keep provider credentials on the remote Mac. For work data, confirm that the daemon is allowed to reach the work provider before sending a real prompt.

## Protocol compatibility

The gateway keeps its phone WebSocket route. The paired Desktop route carries the existing versioned Harness API and shares the gateway's device registry. Phone invites and credentials cannot authorize the Harness route. Local Unix sockets and existing Desktop SSH and Cloud connections do not change.
