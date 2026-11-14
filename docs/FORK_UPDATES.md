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

## Recommended: source-build channel

The fork publishes no GitHub release artifacts, so use:

```toml
[features]
update_channel = "main"
```

The main channel polls `GET /repos/xenking/jcode/commits/master`, and when the
commit differs from the running binary it clones/pulls the fork into the
source-build dir and runs `cargo build --release`. Without cargo it falls
back to the stable release check (which finds nothing on the fork).

For the daily-driver daemon, use `jcode self-dev --build` from the checkout.

## Cloud-managed hosts

The managed cloud bootstrap downloads published Linux x86_64 tarballs plus
`SHA256SUMS`. Only upstream publishes those, so it keeps `1jehuang/jcode` by
default. Set `JCODE_CLOUD_RELEASE_REPO=xenking/jcode` once the fork publishes
matching releases.

## Upstream sync

The `upstream` remote (1jehuang) is untouched. Sync with
`git fetch upstream && git merge upstream/master`.
