use jcode_harness_api::{ApiEvent, ServerFrame};
use jcode_session_types::assistant_text::AssistantTextFrame;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub(super) struct AssistantTextProjection(HashMap<String, usize>);

impl AssistantTextProjection {
    pub(super) fn reset(&mut self) {
        self.0.clear();
    }

    pub(super) fn project(
        &mut self,
        session_id: &str,
        delta: Option<&str>,
        frames: Vec<AssistantTextFrame>,
    ) -> Vec<ServerFrame> {
        let mut out = Vec::new();
        for frame in frames {
            let replace = |id: String, text| {
                ServerFrame::event(ApiEvent::TextReplace {
                    session_id: session_id.into(),
                    text,
                    message_id: Some(id),
                })
            };
            match frame {
                AssistantTextFrame::Append { message_id, offset } => {
                    let Some(text) = delta else { continue };
                    let delivered = self.0.entry(message_id.clone()).or_default();
                    // A reattachment may have missed the prefix. Publishing
                    // this delta would either corrupt it or create a tail-only
                    // duplicate. Wait for the exact block at completion.
                    if offset != *delivered {
                        continue;
                    }
                    *delivered += text.len();
                    out.push(ServerFrame::event(ApiEvent::TextDelta {
                        session_id: session_id.into(),
                        text: text.into(),
                        message_id: Some(message_id),
                    }));
                }
                AssistantTextFrame::Replace { message_id, text } => {
                    if text.is_empty() {
                        self.0.remove(&message_id);
                    } else {
                        self.0.insert(message_id.clone(), text.len());
                    }
                    out.push(replace(message_id, text));
                }
                AssistantTextFrame::Complete { message_id, text } => {
                    if self.0.remove(&message_id) != Some(text.len()) {
                        out.push(replace(message_id.clone(), text));
                    }
                    out.push(ServerFrame::event(ApiEvent::TextDone {
                        session_id: session_id.into(),
                        message_id: Some(message_id),
                    }));
                }
            }
        }
        out
    }
}
