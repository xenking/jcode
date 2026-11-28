# Fork updates (xenking/jcode)

This fork updates from itself, not from `1jehuang/jcode`.

## Settings

| Config (`~/.jcode/config.toml`) | Env | Default |
|---|---|---|
| `[features] update_repo` | `JCODE_UPDATE_REPO` | `xenking/jcode` |
| `[features] update_branch` | `JCODE_UPDATE_BRANCH` | `master` |
| `[features] update_channel` | `JCODE_UPDATE_CHANNEL` | `stable` |
| (cloud bootstrap script) | `JCODE_CLOUD_RELEASE_REPO` | `1jehuang/jcode` |

`update_repo` drives: the release check (`/releases/latest`, preview list,
dev-build compare), the main-channel commit check, the source-build clone,
and the `jcode self-dev` / selfdev tool clone URL.

## Releases

Pushing a `vMAJOR.MINOR.PATCH` tag runs `.github/workflows/release.yml`, which
builds `jcode-macos-aarch64`, `jcode-linux-x86_64` and `jcode-windows-x86_64`
(unsigned), adds `SHA256SUMS`, and publishes the latest release. Asset names
match upstream, so `scripts/install.sh`, `scripts/install.ps1` and the stable
`jcode update` check work against the fork unchanged. Other platforms have no
asset: `install.sh` builds from source there, `install.ps1` needs `-BuildFromSource`.

Fork tags live next to mirrored upstream tags: pick the next free patch above
both (`git ls-remote --tags origin`). The binary takes its version from the tag
(`JCODE_BUILD_SEMVER`). Linux binaries are built on `ubuntu-22.04` and need
glibc 2.35 or newer.

## Source-build channel

```toml
[features]
update_channel = "main"
```

The main channel polls `GET /repos/xenking/jcode/commits/master`, and when the
commit differs from the running binary it clones/pulls the fork into the
source-build dir and runs `cargo build --release`.

For the daily-driver daemon, use `jcode self-dev --build` from the checkout.

## Cloud-managed hosts

The managed cloud bootstrap downloads published Linux x86_64 tarballs plus
`SHA256SUMS` and still defaults to `1jehuang/jcode`. Set
`JCODE_CLOUD_RELEASE_REPO=xenking/jcode` to use fork releases.

## Upstream sync

The `upstream` remote (1jehuang) is untouched. Sync with
`scripts/xk-upstream-agent.sh` (`--dry-run` shows the gap), which runs
`scripts/xk-upstream-sync.sh` for jcode and Desktop. The sync script runs the
`scripts/check_fork_removals.py` guard before any push, even after a clean merge;
do not `git merge upstream/master` by hand.
