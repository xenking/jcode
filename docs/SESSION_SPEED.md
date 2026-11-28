# Session speed modes

Jcode stores speed separately from reasoning effort. New sessions use Standard. The legacy `openai_service_tier` config key does not opt new sessions into a paid speed. Jcode leaves existing config files alone.

## Selection and readback

`set_speed_mode` accepts `standard`, `fast`, or `ultrafast`. `speed_mode_changed` acknowledges the saved choice, not the tier the provider served. The runtime reports the choice and the available options in `runtime_info`.

An option can be available, unavailable, or unknown. Unknown means that the provider lists support for the route but Jcode cannot verify the account's eligibility. An explicit choice can request an unknown option. Jcode does not choose an unknown paid option automatically.

A paid selection belongs to the session, model, and credential route. Switching the route or model clears the selection. If the route changes after a paid request was prepared, Jcode refuses to send the request on the new route. Transport retries keep the tier instead of trying Standard on another route.

`speed_mode_reported` reports an observation from the provider. It does not change the saved choice. If the provider does not report a tier or speed, Jcode does not infer one from a successful request.

## OpenAI routes

| Selection | Platform Responses API | ChatGPT sign-in (Codex) |
| --- | --- | --- |
| Standard | `service_tier: "default"` | No `service_tier` field |
| Fast | `service_tier: "fast"` | `service_tier: "priority"` |
| Ultrafast | `service_tier: "ultrafast"` | `service_tier: "ultrafast"` |

The API uses `default` to opt out of a project's Fast default. A direct custom endpoint does not advertise native paid speed options. Codex routes use the selected account's `service_tiers` catalog entries. The platform API model list does not imply entitlement on the ChatGPT route.

On November 22, 2026, the local Codex cache marked `gpt-6.2-sol` Fast and Ultrafast and marked `gpt-6.2-astra` Fast only. The standalone CLI was `0.166.0`, the bundled app CLI was `0.171.0-alpha.13`, and the cache identified its client as `0.171.0`. This cache records one account, not a global model list. The API guide lists Ultrafast for `gpt-6.2-astra`, `gpt-6.2-sol`, `gpt-6-astra`, and `gpt-6.1-sol`. The guide identifies the last two as available to all API users, subject to rate limits.

For upstream references, see [OpenAI API Fast mode](https://developers.openai.com/api/docs/guides/fast-mode), [OpenAI API Ultrafast mode](https://developers.openai.com/api/docs/guides/ultrafast-mode), and [Codex Fast and Ultrafast modes](https://learn.chatgpt.com/docs/agent-configuration/speed).
