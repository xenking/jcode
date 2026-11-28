# Runtime generation evidence

`jcode_build_support::runtime_generation` owns the distinction between choosing a future executable and identifying the process currently serving a socket.

## Contract

| Concern | API | Evidence |
| --- | --- | --- |
| Desired selection | `DesiredSelection`, `resolve` | A version label or channel is a request, not a serving observation. |
| Socket-aware cold spawn | `resolve_for_socket`, `fallback_before_spawn` | Reads the daemon-owned rollout journal. A dead, unserved pending generation selects its previous canonical executable. A live pending owner, corrupt journal, or missing previous executable is an explicit error. This reader does not restore channels or mutate the journal. |
| Immutable executable | `ImmutableTarget`, `pin_path`, `validate_target` | Canonical path plus executable and wrapper-payload file identities. Explicit requests never fall back. |
| Exec and independent launch | `prepare_exec`, `prepare_launch` | Validate the exact command executable. Exec inherits the requested target and reload id and increments the generation counter. An independent launch resets generation and clears the inherited reload id. |
| Serving process | `ServingIdentity`, `probe_socket_identity` | The daemon reports executable path, compiled git hash, installed label, reload id, generation, and PID. Socket peer PID must agree when the platform supplies it. |
| Presence without provenance | `observe_socket`, `SocketObservation` | A connected legacy or malformed listener is `Unknown`, never `Absent` or permission to spawn a second daemon. |
| Specific handoff | `assess_handoff` | `Ready` requires the listener's matching reload id and positive generation, plus the expected target when provided. A live old listener remains pending even after a request acknowledgement or marker update. |
| Bridge provenance | `assess_serving_target` | Desktop checks its selected bundle executable and git hash before attaching the local API bridge. |
| Reconnection | `assess_reconnect`, `replacement_spawn_permission` | Recent owned handoffs require self-confirmation. Only an absent socket with a dead handoff owner allows replacement. A replacement must report its generation before reconnect proceeds. |

The synchronous socket adapter bounds frames and bytes, skips acknowledgement/events unrelated to the Ping, rejects malformed identity fields and zero generations, and distinguishes legacy protocol from a missing listener. Platform executable-path and inode observations remain diagnostics, not alternate serving-version authorities.

The reload marker records intent and progress. Neither `socket_ready` in that marker nor the mere existence of a listening socket proves a completed handoff. An inherited executable target is accepted only when it identifies the actual running image, including macOS hardlink aliases.

## Regression checks

All runtime checks use isolated child processes, temporary homes, sockets, and markers. They do not restart the user's daemon or modify the installed Desktop application.

| Scenario | Check |
| --- | --- |
| Old listener answers after acknowledgement | Isolated socket handoff tests require the listener's new reload id before `Ready`. |
| Mutable channel changes after selection | A pinned executable command still runs the originally selected executable. |
| PID is preserved across exec | Identity tests distinguish generations and reload ids independently of PID. The rollout lane additionally exercises production daemon exec. |
| Previous bundle bridge remains alive | Desktop rejects a previous executable path or hash, including a stale binary at the same path. |
| Requested executable disappears or changes | Validation returns an explicit error without choosing another channel. |
| Legacy listener is still alive | Observation is `Unknown`, and replacement-spawn permission is denied. |
| New generation dies before serving | Read-only journal selection returns the previous pinned target only after the owner is dead. |
| Bridge handshake | An actual isolated API bridge child reports its own executable, PID, git hash, generation, and cleared independent-launch reload id in `HelloOk`; SDK clones retain that identity. |
| Stale channel marker | Promotion repairs the real channel even when its version marker already names the desired label. |
| Corrupt handoff evidence | Recent unreadable markers, invalid counters, malformed Pong identity, and invalid journal data remain explicit unknown/error states. |

`ApiEvent::HelloOk.runtime_identity` is optional on the wire for compatibility. A legacy peer remains decodable, but lack of provenance cannot satisfy a generation-sensitive attach or readiness check.
