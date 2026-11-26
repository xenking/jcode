# Native swarm and Code Mode token cost

Call `swarm` directly for short coordination. Use `codemode` when a script can
resolve dependencies without the model, or when the model needs only part of a
large result. Use the native multi-target action when it does the whole job,
such as `status` with `session_ids`. The run below does not measure that shortcut.

## Reproduce the measurement

The measurement script is no longer in the repository. Rebuild the harness from
the method below to repeat it.

Measured on CLI `1ced95d9c7682c939992f4f4b47a7df63c1b08d7`. The harness ran
Jcode against a loopback OpenAI-compatible Chat Completions fixture. It uses a
private home and socket and sets `JCODE_TEST_SESSION=1`. Only the tokenizer
installation and its public vocabulary may need a download. The script uses
a fixture key, not real credentials. It does not use external inference or
user sessions. It prints aggregates, not request bodies. It does not measure
latency, provider usage, or billing.

Each arm runs five times per case, with the order rotated. The independent case
puts N calls in one native model response, or in one script using `Promise.all`.
The calls cycle through `list`, `status`, and `plan_status` on a solo synthetic
swarm. The chain starts with `list`. Each `status` takes the target parsed from
the preceding result. This models dependency depth, not a recommendation to
poll the same member. Code Mode passthrough returns the full results as an array.
The projection returns the session ID, lifecycle, or item count. The script
checks each requested value, direct call arguments including `null`, and nested
receipts. Fault injection adds a `summary` call without `to_session`. There
were 15 expected direct or nested failures in 210 runs, and no unexpected errors.
The Code Mode fault arms use `Promise.allSettled` to keep the successful siblings.

## What the tokenizer counted

- **Input** is the local `o200k_base` count of compact JSON `messages` and `tools`
  as received by the fake provider, summed across requests. **Extra input** is
  that sum after subtracting one copy of the first request per request. It
  measures growing history only, not the effect of caching.
- **Output** counts the scripted assistant content and tool calls as compact JSON.
  It includes argument names, arguments, and call identifiers. It excludes SSE
  transport frames and any real model's reasoning. The final fixture reply is
  included in every arm.
- **Request bytes** count the HTTP POST bodies before parsing. **Result bytes**
  count the native result text without its timing prefix, or the `B` values in
  the Code Mode receipts. A Code Mode receipt counts the string or serialized
  JSON before projection. Failed nested calls record `0B` even when the receipt
  contains a diagnostic. Neither byte count is a token count.

The native assistant call has a `function` name of `swarm` and a JSON string in
`function.arguments`, for example `{"action":"status","intent":"Read swarm fixture"}`.
The next input echoes the call and has a `role: "tool"` message keyed by
`tool_call_id`. Code Mode swaps the function name for `codemode`, sends the
script in `arguments`, and returns one report with a result and nested receipts.
The input estimate includes Jcode's actual timing prefixes and JSON escaping.

Both schemas stay visible in every arm. Tokenized on their own, `swarm` is 884
and `codemode` is 364. The captured swarm schema advertises 25 properties and
requires `action` and `intent`. The compatible endpoint does not set `strict`.
When the fixture explicitly sends all 23 unused fields as `null`, Jcode accepts
and echoes them. They add 102 output proxy tokens per call in this fixture and
also increase the echoed input. Omit unused arguments where the endpoint allows
it. This does not measure another provider's schema, tokenizer, or null policy.

## Results

Each cell below is **median extra input / median output** in local proxy tokens.
The script prints the range of every counter. Every independent arm takes two
requests including the final answer. An N-step native chain takes N+1 requests.
Both Code Mode chain arms take two.

| Shape | N | Native | Code Mode passthrough | Code Mode projection |
| --- | ---: | ---: | ---: | ---: |
| Independent | 1 | 155 / 54 | 231 / 88 | 211 / 104 |
| Independent | 2 | 362 / 88 | 391 / 105 | 288 / 165 |
| Independent | 3 | 510 / 123 | 484 / 111 | 314 / 171 |
| Independent | 4 | 661 / 157 | 552 / 116 | 352 / 176 |
| Independent | 8 | 1376 / 294 | 979 / 125 | 447 / 185 |
| Independent | 16 | 2681 / 569 | 1699 / 125 | 637 / 185 |
| Chain | 2 | 550 / 120 | 416 / 134 | 289 / 165 |
| Chain | 4 | 2088 / 255 | 712 / 134 | 324 / 165 |
| Chain | 16 | 31776 / 1059 | 2440 / 134 | 529 / 165 |
| Injected failure | 3 | 447 / 123 | 479 / 136 | 388 / 199 |

- **Independent.** Native wins for a single short call. Passthrough crosses
  between N=2 and N=3 here. At N=2, projection saves 74 input proxy tokens and
  adds 77 output proxy tokens. The equal-weight sums, 450 and 453, overlap
  across runs. At N=3, projection saves 196 input proxy tokens and adds 48
  output proxy tokens. This is a fixture-dependent tradeoff, not a universal
  cutoff by N. Passthrough can reduce repeated call wrappers and arguments,
  but it still sends the underlying results to the model.
- **Chain.** At N=4, cumulative input including the repeated prefix is 12658
  native, 4942 passthrough, and 4551 projected. Wire request bytes are 51783,
  20157, and 19160, respectively. Cache hits or incremental provider requests
  can change which prefix work is charged. The model still has fewer turns
  when the script handles the dependencies.
- **Bytes.** For independent N=8, median internal result bytes are 1657 for
  passthrough and 1615 for projection. The same kind and number of nested calls
  ran. Projection changes what reaches the model, not what those calls read.
  As a separate illustration, 200184 bytes of repeated `agent ready ` encode
  to 33365 local tokens, while 200184 bytes of repeated `session:task/7fc9de3f\n`
  encode to 100091. A receipt for that many bytes alone says nothing about the
  token cost or whether the underlying content reached a provider.

For a different task, compare the extra script output and receipt text against
results omitted and model turns avoided. If input and output have different
weights, apply those weights separately. Do not estimate a bill from this run.
