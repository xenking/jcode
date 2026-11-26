use crate::protocol::{ServerEvent, TurnIdentity};
use jcode_session_types::assistant_text::AssistantTextFrame;

#[derive(Default)]
pub(super) struct AssistantText {
    next_block: u64,
    open: bool,
    response_ended: bool,
    blocks: Vec<TextBlock>,
}

struct TextBlock {
    id: String,
    text: String,
    complete: bool,
}

impl AssistantText {
    pub(super) fn frames(
        &mut self,
        identity: &TurnIdentity,
        event: &ServerEvent,
    ) -> Option<Vec<AssistantTextFrame>> {
        if self.response_ended
            && matches!(
                event,
                ServerEvent::TextDelta { .. }
                    | ServerEvent::ReasoningDelta { .. }
                    | ServerEvent::ToolStart { .. }
                    | ServerEvent::ToolExec { .. }
                    | ServerEvent::ToolCall { .. }
            )
        {
            self.new_response();
        }
        Some(match event {
            ServerEvent::TextDelta { text } => {
                if text.is_empty() {
                    return Some(vec![]);
                }
                let block = self.open_block(identity);
                let offset = block.text.len();
                block.text.push_str(text);
                vec![AssistantTextFrame::Append {
                    message_id: block.id.clone(),
                    offset,
                }]
            }
            ServerEvent::TextDone
            | ServerEvent::ToolExec { .. }
            | ServerEvent::SoftInterruptInjected { .. }
            | ServerEvent::Done { .. }
            | ServerEvent::Error { .. } => self.complete(),
            ServerEvent::MessageEnd { .. } => {
                let frames = self.complete();
                self.response_ended = true;
                frames
            }
            ServerEvent::KvCacheRequest { .. } => {
                let frames = self.complete();
                self.new_response();
                frames
            }
            ServerEvent::TextReplace { text } => self.replace(identity, text),
            ServerEvent::TextSuffixReplace { old_text, new_text } => {
                let Some(block) = self.blocks.last_mut().filter(|_| self.open) else {
                    return Some(vec![]);
                };
                if old_text.is_empty() || old_text == new_text {
                    return Some(vec![]);
                }
                let Some(prefix) = block.text.strip_suffix(old_text) else {
                    return Some(vec![]);
                };
                block.text.truncate(prefix.len());
                block.text.push_str(new_text);
                vec![AssistantTextFrame::Replace {
                    message_id: block.id.clone(),
                    text: block.text.clone(),
                }]
            }
            ServerEvent::RetryRollback { .. } => {
                let frames = self.replace(identity, "");
                self.new_response();
                frames
            }
            _ => return None,
        })
    }

    fn new_response(&mut self) {
        self.open = false;
        self.response_ended = false;
        self.blocks.clear();
    }

    fn open_block(&mut self, identity: &TurnIdentity) -> &mut TextBlock {
        if !self.open {
            let id = format!(
                "text:{}:{}:{}",
                identity.runtime_generation, identity.turn_id, self.next_block
            );
            self.next_block += 1;
            self.blocks.push(TextBlock {
                id,
                text: String::new(),
                complete: false,
            });
            self.open = true;
        }
        self.blocks.last_mut().expect("open text block")
    }

    fn complete(&mut self) -> Vec<AssistantTextFrame> {
        if !std::mem::take(&mut self.open) {
            return vec![];
        }
        self.blocks
            .last_mut()
            .and_then(TextBlock::complete)
            .into_iter()
            .collect()
    }

    fn replace(
        &mut self,
        identity: &TurnIdentity,
        mut replacement: &str,
    ) -> Vec<AssistantTextFrame> {
        if self.blocks.is_empty() && !replacement.is_empty() {
            self.open_block(identity);
        }
        let count = self.blocks.len();
        let mut frames = Vec::new();
        for (index, block) in self.blocks.iter_mut().enumerate() {
            let mut len = if index + 1 == count {
                replacement.len()
            } else {
                block.text.len().min(replacement.len())
            };
            while !replacement.is_char_boundary(len) {
                len -= 1;
            }
            let (next, rest) = replacement.split_at(len);
            replacement = rest;
            if block.text != next {
                block.text.clear();
                block.text.push_str(next);
                frames.push(AssistantTextFrame::Replace {
                    message_id: block.id.clone(),
                    text: block.text.clone(),
                });
            }
            if (self.response_ended || !self.open || index + 1 != count)
                && let Some(frame) = block.complete()
            {
                frames.push(frame);
            }
        }
        if self.response_ended {
            self.open = false;
        }
        frames
    }
}

impl TextBlock {
    fn complete(&mut self) -> Option<AssistantTextFrame> {
        if self.complete || self.text.is_empty() {
            return None;
        }
        self.complete = true;
        Some(AssistantTextFrame::Complete {
            message_id: self.id.clone(),
            text: self.text.clone(),
        })
    }
}
