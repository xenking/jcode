---
name: native-ui
description: Use when showing interactive cards, embedded images, live progress, forms, or updating a prior card in Jcode Desktop. Prefer text when controls or a visual add no value.
mode: true
---

# Native UI in Jcode Desktop

Pinning this mode is what adds `applet` to the tool list; without it the tool is not advertised at all.

For a live table of the current chat's tasks, if `TaskTable(title)` is advertised, mount `root = TaskTable("Tasks")` inline once without `wait`. The native renderer follows the owning session's task events. Do not send patches to keep it fresh. This macro does not watch arbitrary jobs, files, URLs or other sessions. Ordinary `table` and `progress` nodes still need a producer for updates.

If `applet` is unavailable, give a text equivalent. Do not enable permissions or open a browser just to render UI. For a useful visual or choice, call `applet` instead of printing JSON in prose. Use `placement: "end"` when the user asks to see a demo in the current chat.

1. Prefer `express` when a supported macro fits. The current runtime advertises its signatures in the tool definition. For example `root = ReviewChange("Review patch", "src/ui.rs", "change42")` returns `review.approve` or `review.request_changes`, `args.change_id`, and `state.note`. `GreetingButton(label,event,recipient)` returns the named action. Arguments are positional JSON strings. Do not emit `<a2ui>` tags, arbitrary A2UI wire JSON, or invented macros.
2. For other layouts, fetch `applet({action:"catalog"})` if you need shapes; supply `view` instead of `express`. Compose rows/columns, images, text, lists, tables, inputs and buttons. A flight card, recipe, contact, playlist or order summary is presentation; real flight data, playback, messaging and payment require separate authorized tools. Never claim a picture of a gallery is a collection of interactive cards.
3. Mount once. Keep the returned `instance` and revision. Patch a changing value or label on that instance; use `update` for a different layout. Example mount view: `{"type":"progress","value":0.2,"label":"2 of 10 checked"}`. Example patch: `{"action":"patch","instance":"checks","base_revision":1,"ops":[{"op":"replace","path":"/view/value","value":0.7},{"op":"replace","path":"/view/label","value":"7 of 10 checked"}]}`. Substitute the actual instance, revision and measured counts. Coalesce fast events, avoid token-by-token tool calls and do not fabricate percent complete. This is a sequence of updates, not automatic streaming of partial Express. On a revision mismatch, list instances and reconcile before retrying.
4. For an image, use `assets:[{id,mime,data}]` at mount, with base64 bytes in `data`; reference `{"type":"image","source":{"asset":"preview"},"alt":"...","aspect_ratio":1.8}`. Use a short stable ID matching the asset, a thumbnail and the actual aspect ratio. Pass bytes through tools, not model text. Do not resend them in progress patches. Remote image URLs need a capability that the built-in agent applet does not currently grant. If a source isn't authorized to fetch or embed, fall back to a description.
5. Use `wait:true` only when work needs the user's answer. On an action, update the **same card** with the selection and note, replacing or disabling completed controls. A click is input, not authority for other effects. Identical notifications do not establish how many clicks occurred: never repeat an external effect solely because the notification repeated. Do not claim the screen was seen based only on a mount ACK.

Upstream A2UI separates model-facing instructions, catalog and rendering (`https://a2ui.org/concepts/overview/`, `https://a2ui.org/concepts/inference-formats/`). Here the compiler targets Jcode applet nodes rendered by GPUI. The live Jcode catalog is authoritative; do not assume upstream components, transports or functions exist locally.
