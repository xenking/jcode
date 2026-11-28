# Token estimation

jcode counts tokens in three ways, in this order of preference:

1. **Provider-reported usage.** Once a request has been answered, the provider's own count for
   that prompt is exact. The context percentage, the compaction trigger and the Desktop context
   bar are anchored on it. Only the content added since that report is estimated:
   `CompactionManager::effective_token_count_with` is `reported + estimate(now) - estimate(when
   the report was taken)`. With no anchor (no report yet, or history rewritten since) the larger
   of the estimate and the report wins.
2. **The family estimator** (`jcode_message_types::token_estimate`). Used before any report
   exists: a restored session, the next request's size, compaction cut points, shake thresholds,
   tool-output guards. One module; there is no second rule anywhere.
3. **Size-only conversion** (`TokenFamily::tokens_for_bytes`) for content known only by its byte
   size (the `/context` prompt section sizes, the Desktop sidebar's transcript size).

There is no flat chars-per-token constant. The previous rule (`bytes / 4`, +50/+20 per tool
call/result) under-counted Anthropic context by 38% and over-counted OpenAI context by 18%
(median, held-out sessions), and charged Cyrillic by UTF-8 bytes.

## The estimator

`TokenFamily::from_model(name)` picks the family: `claude*` is Anthropic, `gpt*` / `o<digit>*` /
`*codex*` is OpenAI, `deepseek*` is DeepSeek, `gemini*` is Gemini, anything else is Generic.
Gemini and Generic share one table.

Every character is classified once (ASCII letters, word starts, digits, whitespace, whitespace
runs, ASCII punctuation, Cyrillic, Cyrillic word starts, CJK, other) and the counts are weighted
by a per-family table of milli-tokens. The model is linear, so estimates are monotonic and
additive up to one token of rounding and a word split at a seam. Code and JSON cost more than
prose only because they contain more punctuation; there is no content-kind detection to get
wrong. Reasoning text, thinking signatures, encrypted reasoning and provider-native JSON are
charged per byte, text blocks and tool calls (the call plus its result) per block, and images
and OpenAI compaction blobs a flat token cost.

API (`jcode_base::message` re-exports it): `estimate_text(family, &str)`; `Estimator`, which
accumulates pieces and is also a `fmt::Write` so a serializer can stream into it;
`TokenFamily::{tokens_for_bytes, bytes_for_tokens, prefix_within, system_overhead_tokens}`.
`bytes_for_tokens` turns a token budget into a truncation cap (a lower bound for mixed
content); `prefix_within` truncates a string to an exact token budget.

Tool-output budgets (MCP results, codemode reports), the summarizer's prompt cap and the
TUI/Desktop token badges use the Generic table: the text exists before any model reads it and
may be replayed to another.

## Calibration

Fitted by non-negative least squares, reweighted toward median relative error, on
consecutive-turn deltas of provider-reported prompt tokens against the content added between
the two turns, from the recorded sessions of one machine. The split is 70/30 by session id hash;
every number below is on the held-out 30%. Median / p95 absolute percent error on the delta:

| family | held-out pairs | flat rule | estimator |
|---|---|---|---|
| Anthropic | 12858 | 40% / 74% | 4.6% / 45% |
| OpenAI | 25626 | 36% / 291% | 5.1% / 36% |
| DeepSeek | 3787 | 27% / 60% | 3.4% / 50% |

The p95 is content the estimator cannot see (system reminders, memory injections, images
decayed before sending). Whole-context estimates before a report: median 3.6% (Anthropic), 2.8%
(OpenAI), 1.9% (DeepSeek), against 29% / 21% / 13.5% for the flat rule. The guard for the tables
is `token_estimate_tests.rs`: class counts and reported usage of random held-out pairs
(numbers only, no message text), plus the classifier's parity with the fitting script.

Limits:

- **Gemini has no recorded sessions.** It uses the Generic table, a pooled fit of the three
  measured families. On a family it was not fitted on it measured 15-25% median error
  (leave-one-family-out), still better than the flat rule. Calibrating Gemini is a table change.
- **CJK weights are priors**, not fits: too few recorded pairs.
- **Images** keep the flat 1600-token cost (PNG header for dense pages). Recorded image turns
  are far noisier than text and were not tuned.
- Provider tokenizers change between model generations. Re-fit when the median bias of
  reported-vs-estimated drifts past a few percent.

## System prompt and tool overhead

Before the first report the estimate adds an overhead for the system prompt and tool catalog:
Anthropic 31,000, OpenAI 17,500, DeepSeek 19,000, Generic/Gemini 18,000 tokens (the fitted
intercepts), only for windows of 100k tokens or more.
