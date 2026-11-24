# Several API keys per provider

Fork-only. API-key providers can hold several labelled keys, the way Claude and
OpenAI logins hold several accounts. Typical uses: two OpenCode Go subscriptions,
Hugging Face keys billed to different organizations, several keys on one named
OpenAI-compatible provider.

## Account ids

| Id | Meaning |
| --- | --- |
| `opencode-go` | The unlabelled key. Existing setups keep working unchanged. |
| `opencode-go:second` | A labelled key. |

The same ids go into `[[routing.accounts]] id`. Providers are the built-in
OpenAI-compatible profiles (`opencode`, `opencode-go`, `huggingface`, `deepseek`
and the rest of the login catalog) and `[providers.<name>]` entries of type
`openai-compatible` or `openrouter`. The generic `openai-compatible` profile and
the Muse subscription take no labels.

## Adding, listing, choosing

```bash
jcode login opencode-go --account second        # paste or pipe the key, stored with mode 0600
jcode auth keys add gateway team --api-key-env TEAM_KEY   # named provider
jcode auth keys list                            # * marks the key requests use
jcode auth keys use opencode-go second          # requests use it from the next call on
jcode auth keys use opencode-go                 # back to the unlabelled key
jcode auth keys remove opencode-go second
```

Inside the TUI: `/account opencode-go switch second`, `/account opencode-go
remove second`, `/account list`. `/usage` shows every key as its own entry with
its own windows, for example `OpenCode Go (API key) [second]`.

The first labelled key of a provider with no unlabelled key becomes active on
its own. A labelled key never replaces the unlabelled one. Keys are stored in
`api-key-accounts.json` next to the other credential files (mode 0600) and are
never printed.

## `key_command`

For a Keychain or password manager, name a program and its arguments. There is
no shell, so quoting and expansion do not apply. The trimmed stdout is the key.

```toml
[providers.gateway]
base_url = "https://gateway.example/v1"
key_command = ["security", "find-generic-password", "-s", "gateway", "-w"]
key_command_timeout_secs = 15        # default 15, at most 120

[api_key_profiles.opencode-go.keys.vault]
key_command = ["op", "read", "op://Private/opencode/credential"]
```

The result is cached in memory until the config reloads, never written to disk
or the log. A command that fails, times out or prints nothing gives an error
that names the account and program. Jcode never falls back to another key.
Values beginning with `!` are plain text and never executed.

## Headers

```toml
[api_key_profiles.huggingface.headers]
X-HF-Bill-To = "my-org"

[api_key_profiles.huggingface.keys.org-b]
headers = { X-HF-Bill-To = "other-org" }   # overrides the provider-level header
```

`[providers.<name>] headers` now applies to OpenAI-compatible requests as well.
A header with an invalid name or value stops the request with an error that
names the header, not its value.

`[api_key_profiles.<id>] base_url` points a built-in profile at a proxy.

## Price overrides

```toml
[api_key_profiles.opencode-go.costs."kimi-k2.5"]
input = 0.6          # USD per 1M tokens
output = 2.5
cache_read = 0.15
cache_write = 0.75

[providers.gateway.costs."*"]
input = 1.0
output = 3.0
```

A key is a model id or a `*` pattern, and the most specific match wins. Omitted
categories use the usual catalog price. The prices drive the cost shown in the
TUI and the local spend ledger. Spend is recorded under the exact account id of
the key in use, `opencode-go:second` for a labelled key and the original
`openai-compatible:opencode-go` entry for the unlabelled one.
