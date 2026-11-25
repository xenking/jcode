use super::*;
use crate::session::OpenUserAsk;
use std::fmt::Write as _;

const STATUS_FORMAT: &str = "End your reply with one status line per open user request. Use `- [x] Ask #ID: result` for a finished or withdrawn request, or `- [ ] Ask #ID: status and next step` for a request that remains open. Report what is known; do not claim unfinished work is done.";

pub(super) enum AskReport {
    Accept,
    Retry,
    Unreported(String),
}

impl Agent {
    pub(super) fn record_user_ask(
        &mut self,
        role: Option<StoredDisplayRole>,
        text: &str,
        has_attachment: bool,
    ) -> bool {
        if self.is_swarm_worker()
            || self.session.agent_started
            || !matches!(role, None | Some(StoredDisplayRole::JarvisUser))
            || !self.uses_user_ask_reports()
        {
            return false;
        }
        let text = text.trim();
        if (text.is_empty() && !has_attachment) || crate::todo::is_auto_poke_message(text) {
            return false;
        }
        let redacted = crate::message::redact_secrets(text);
        let mut chars = redacted.chars();
        let shortened: String = chars.by_ref().take(320).collect();
        let mut preview = shortened.split_whitespace().collect::<Vec<_>>().join(" ");
        if chars.next().is_some() {
            preview.push('…');
        } else if preview.is_empty() {
            preview.push_str("(attachment)");
        }
        let ledger = &mut self.session.user_asks;
        ledger.last_id += 1;
        ledger.open.push(OpenUserAsk {
            id: ledger.last_id,
            preview,
        });
        true
    }

    fn uses_user_ask_reports(&self) -> bool {
        self.session
            .pinned_skill_names()
            .iter()
            .any(|name| matches!(name.as_str(), "poteto-mode" | "swarm"))
            || self
                .provider
                .reasoning_effort()
                .as_deref()
                .is_some_and(crate::prompt::is_swarm_mode_effort)
    }

    pub(super) fn user_ask_instructions(&self) -> Option<Message> {
        let ledger = &self.session.user_asks;
        if self.is_swarm_worker() || ledger.open.is_empty() {
            return None;
        }
        let mut text = format!("<system-reminder>\n{STATUS_FORMAT}\nOpen user requests:\n");
        list_asks(&mut text, ledger.open.iter());
        text.push_str("</system-reminder>");
        Some(Message::user(&text).into_request_only())
    }

    pub(super) fn finish_user_asks(&mut self, reply: &str) -> Result<AskReport> {
        if self.is_swarm_worker() || self.session.user_asks.open.is_empty() {
            return Ok(AskReport::Accept);
        }

        let ledger = &mut self.session.user_asks;
        let mut statuses = std::collections::HashMap::new();
        let mut in_code = false;
        for line in reply.lines() {
            let line = line.trim();
            if line.starts_with("```") || line.starts_with("~~~") {
                in_code = !in_code;
            }
            if !in_code && let Some((id, done)) = parse_status(line) {
                statuses
                    .entry(id)
                    .and_modify(|old| *old &= done)
                    .or_insert(done);
            }
        }
        let missing = ledger
            .open
            .iter()
            .filter(|ask| !statuses.contains_key(&ask.id));
        let mut unreported = String::new();
        list_asks(&mut unreported, missing);

        if !unreported.is_empty() && !ledger.reminder_sent {
            ledger.reminder_sent = true;
            self.append_model_reminder(&format!(
                "<system-reminder>\n[user-asks feedback] Your last reply did not report the requests below. Give one complete user-facing update, including requests already covered in your last reply. Do not wait for background work to finish before giving its current status.\n{STATUS_FORMAT}\nUnreported requests:\n{unreported}</system-reminder>"
            ));
            self.session.save()?;
            return Ok(AskReport::Retry);
        }

        ledger.reminder_sent = false;
        ledger
            .open
            .retain(|ask| statuses.get(&ask.id) != Some(&true));
        let outcome = if unreported.is_empty() {
            AskReport::Accept
        } else {
            let notice = format!("\n\nStill open without a status update:\n{unreported}");
            self.add_message(
                Role::Assistant,
                vec![ContentBlock::Text {
                    text: notice.clone(),
                    cache_control: None,
                }],
            );
            AskReport::Unreported(notice)
        };
        self.session.save()?;
        Ok(outcome)
    }
}

fn parse_status(line: &str) -> Option<(u64, bool)> {
    let (done, line) = if let Some(line) = line.strip_prefix("- [x] Ask #") {
        (true, line)
    } else {
        (false, line.strip_prefix("- [ ] Ask #")?)
    };
    let (id, status) = line.split_once(':')?;
    (!status.trim().is_empty()).then_some((id.parse().ok()?, done))
}

fn list_asks<'a>(text: &mut String, asks: impl Iterator<Item = &'a OpenUserAsk>) {
    for ask in asks {
        let _ = writeln!(text, "- Ask #{}: {}", ask.id, ask.preview);
    }
}
