# Recorded GitHub pull requests

Fields consumed by the script, recorded on November 22, 2026 UTC:

- `jcode-open.json`, `jcode-desktop-open.json`: `gh api --method GET --paginate --slurp 'repos/1jehuang/<repo>/pulls?state=open&per_page=100'`. Kept a few rows and split the CLI rows across two pages to test pagination.
- `jcode-1788.json`, `jcode-1789.json`: `gh api --method GET repos/1jehuang/jcode/pulls/<number>`. The first was closed without a merge and still has a `merge_commit_sha`. The second was merged.

Bodies are shortened. Merge tests replace the merged commit SHA in a temporary
copy with the corresponding commit in their local Git repository. The fixture
stub rejects non-GET requests and never uses the network.
