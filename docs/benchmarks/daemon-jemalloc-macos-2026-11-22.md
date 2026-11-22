# Jemalloc default: isolated daemon A/B on November 22, 2026

## macOS arm64, fresh idle daemon

Both binaries were built from the same allocator change with
`scripts/dev_cargo.sh build --locked --profile selfdev -p jcode --bin jcode`.
The control added `--no-default-features --features pdf,bedrock`. Each variant
ran three times with a fresh `JCODE_HOME`, `JCODE_TEST_SESSION=1` and a private
socket. No sessions or model turns were started. The first `server:memory`
snapshot was taken three seconds after socket readiness, before profiling.

| Metric (median of three, MiB) | System malloc | Jemalloc | Change |
| --- | ---: | ---: | ---: |
| macOS physical footprint (`TASK_VM_INFO.phys_footprint`) | 48.72 | 53.50 | +4.78 (+9.81%) |
| RSS | 93.97 | 99.17 | +5.20 (+5.54%) |
| Binary file size | 404.77 | 412.85 | +8.09 (+2.00%) |

Binary sizes were 424,427,984 and 432,908,928 bytes. Footprint ranged from
48.52 to 48.84 MiB for system malloc and 51.80 to 54.03 MiB for jemalloc.
This measures the fixed cost of observability in a fresh daemon. It does not
measure fragmentation or savings in the long-lived production daemon.

## Functional verification

- The control reported `name=system` and no allocator stats. The default build
  reported `name=jemalloc`, `stats_available=true`, and both decay values at
  1,000 ms. It had four initialized arenas and five total (including the
  oversize arena).
- `allocator:profile:on`, `allocator:profile:dump`, and `allocator:profile:off`
  succeeded without a session. The initial sampling state was inactive, then
  active, then inactive again. The 1,174-byte `heap_v2/524288` dump contained the
  executable map and a sampled stack. On a fixture holding a 4 MiB socket read
  buffer, `jeprof --text --inuse_space --show_bytes` reported 4,195,715 bytes and
  resolved `tokio::io::util::read_line::read_line_internal` and `read_until_internal`.
- `justrust clippy -p jcode`, `justrust clippy -p jcode --all-features -- -D warnings`,
  and `justrust test -p jcode-base process_memory` (11 tests) passed on macOS.
  The same 11 library tests passed with `jcode-base/jemalloc-prof` enabled, in
  the test binary that has its own allocation-counting global allocator. Both
  separate renderer parity assertions fail with and without jemalloc.
  The broader `--all-targets --all-features` command was blocked by pre-existing
  missing `text_frames`, `speed_options`, and `lane_speed` fields in test literals.
- The locked Linux target graph contains stats and profiling. The Windows MSVC
  graph excludes jemalloc even with `--all-features`. Native smoke checks are
  wired into CI and release builds for Linux and macOS. The local runtime
  measurements are macOS only.

See the [memory incident runbook](../MEMORY_INCIDENT_RUNBOOK.md#4-unattributed_live_heap)
for commands against a daemon on the new build. No installed daemon was
replaced or reloaded during verification.
