# Local build admission

`scripts/dev_cargo.sh`, macOS rollout build preparation, and Desktop's local
`package-macos.sh` share a per-user disk budget. They reject a build before
Cargo writes artifacts or packaging fetches Sparkle. Rollout also guards the
creation and checkout of its build worktrees.

A lease reserves estimated additional bytes. It does not allocate disk blocks
or enforce a quota. Use the same `HOME` for these entry points. The ledger at
`~/.local/state/jcode/build-admission` is independent of `JCODE_HOME`, `TMPDIR`,
and worktree ownership. APFS volumes in one container share a budget. Other
local filesystems are identified by device. Cargo metadata resolves target and
intermediate build directories. Packaging also includes output and Sparkle
staging. The check includes temp and cache storage on other filesystems.

| Work | Estimated peak additional space | Free space to preserve |
| --- | ---: | ---: |
| Development Cargo | 16 GiB | 24 GiB |
| Release Cargo | 24 GiB | 24 GiB |
| Desktop packaging | 24 GiB per architecture | 24 GiB |
| Rollout worktree checkout | 2 GiB | 24 GiB |
| Separate temp/cache filesystem | 2 GiB | 1 GiB |

These are initial operating estimates, not measured peaks. On a shared pool
the largest claim for one build covers that build's combined output. Claims
from separate builds are added. The calculation conservatively includes the
full claims of builds that have already written files. A normal capacity
refusal returns exit code 75 and reports the requesting owner, paths, free
space, required floor and competing leases. It does not wait for a build to
finish or kill a process. Only the short ledger transaction waits up to two
seconds. A missing storage probe or invalid ledger also refuses the build.

A file descriptor keeps each lease live across `exec` and through child
processes that inherit it. A nested package build validates and reuses its
parent's lease. A later admission attempt discards closed leases automatically.
Do not remove the ledger, its lock, or active lease files while builds run.

To adjust the estimates for a host, put a JSON object with integer byte values
at `~/.local/state/jcode/build-admission/policy.json`. Recognized keys are
`floor`, `dev`, `release`, `desktop`, `checkout`, `aux_floor` and `aux`; omitted
keys keep their defaults. Create the containing directory with mode `0700` if
it does not already exist.

Cargo queries, the SSH remote build route, hosted Desktop CI, and normal
hosted/default routing through a recognized justrust Cargo proxy do not take
a local compile lease. A justrust proxy explicitly configured not to route
can be guarded by `dev_cargo.sh`. Direct `justrust --local`, its automatic
local fallback, direct Cargo, existing builds, and the daemon's later rollout
installation copies are not covered. A Cargo subprocess hook in justrust 0.1.20
runs too late: justrust can seed the local slot before spawning that subprocess.

Run the small offline macOS acceptance test from a Jcode checkout with the
matching Desktop source checkout:

```bash
python3 scripts/test_build_admission.py ../jcode-desktop/scripts/package-macos.sh
```

It compiles only fixture crates, replaces Apple packaging tools with fixture
commands, and kills only its own wrapper. The checked-in Rust rollout adapter
requires a new CLI build before it is active in an already installed CLI.
