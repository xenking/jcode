# Responses replay after request-only memory expires

Two local traces showed the same Responses prefix break. Each request consumed pending memory and added one model-facing tail item. The journal recorded one recall card and no new history message. The next request replaced that item with a tool call. The model, system prompt, tools, and request options stayed the same. Both requests logged `input_prefix_changed` and sent a full frame.

| First changed item (zero-based) | Responses items before and after | Full replay JSON bytes | Deflate payload bytes | Prior input minus new cached input |
| ---: | ---: | ---: | ---: | ---: |
| 158 | 159 → 160 | 1,675,545 | 980,081 | 138,287 − 117,760 = 20,527 |
| 218 | 219 → 221 | 2,406,922 | 1,272,343 | 156,634 − 149,248 = 7,386 |

`persist_memory_injections = false` gives recall one request. The saved recall card is for the transcript, not for the model. A continuation with the old `previous_response_id` would keep the expired item visible to the model even if the delta itself did not contain it. The full replay is required. Neither window contains a different reset to remove. No source lifecycle, fork, compaction, retry, or transport behavior changes.

The app-core tests send synthetic history through the real Responses WebSocket client to a loopback server. The server counts JSON text-frame payload bytes. One fixture returns a text tool result. The other returns a text result with an image. The tests assert a new socket for request-only recall and a same-socket delta when the existing durable policy is enabled. Both policies keep one recall card. Only the durable policy keeps the model-facing item in reloaded history.

| Fixture | Policy | Full frames | Full bytes | Delta frames | Delta bytes |
| ---: | --- | ---: | ---: | ---: | ---: |
| 158 | Request-only baseline | 2 | 88,859 | 0 | 0 |
| 158 | Durable control | 1 | 44,375 | 1 | 956 |
| 218 | Request-only baseline | 2 | 122,347 | 0 | 0 |
| 218 | Durable control | 1 | 60,935 | 1 | 1,324 |

The durable control is a different user policy, not an optimization under the baseline policy. A temporary change that ignored the opt-out produced a forbidden delta and failed both tests. Restoring the contract passed both. The loopback server does not simulate a provider cache. The trace describes why Jcode sent a full frame, not why a provider reported fewer cached tokens.

```sh
justrust test --local='Responses loopback only' -p jcode-app-core --lib --no-default-features request_only_recall_wire_item -- --nocapture
```
