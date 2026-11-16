//! Inline picker for the `ask` tool (`ServerEvent::AskUserRequest`).
//!
//! Pure state machine: keys in, an optional `AskResponse` out. Rendered as an
//! inline view above the composer, so it reuses the passive inline renderer.
use crate::protocol::{AskAnswer, AskQuestion, AskResponse};
use crossterm::event::KeyCode;

#[derive(Debug, Clone)]
pub(crate) struct AskPickerState {
    pub request_id: String,
    pub questions: Vec<AskQuestion>,
    /// Index of the question currently shown.
    pub current: usize,
    /// Cursor row within the current question (options, then Other).
    pub cursor: usize,
    /// Per-question toggled option indices.
    pub chosen: Vec<Vec<usize>>,
    /// Per-question free text for Other.
    pub other: Vec<String>,
}

pub(crate) enum AskKeyOutcome {
    Pending,
    Done(AskResponse),
}

impl AskPickerState {
    pub fn new(request_id: String, questions: Vec<AskQuestion>) -> Self {
        let n = questions.len();
        Self {
            request_id,
            questions,
            current: 0,
            cursor: 0,
            chosen: vec![Vec::new(); n],
            other: vec![String::new(); n],
        }
    }

    fn question(&self) -> &AskQuestion {
        &self.questions[self.current]
    }

    fn rows(&self) -> usize {
        self.question().options.len() + usize::from(self.question().allow_other)
    }

    fn on_other(&self) -> bool {
        self.question().allow_other && self.cursor == self.question().options.len()
    }

    fn toggle(&mut self) {
        let cursor = self.cursor;
        let chosen = &mut self.chosen[self.current];
        if let Some(pos) = chosen.iter().position(|&i| i == cursor) {
            chosen.remove(pos);
        } else {
            chosen.push(cursor);
        }
    }

    fn answered(&self, q: usize) -> bool {
        !self.chosen[q].is_empty() || !self.other[q].trim().is_empty()
    }

    fn advance(&mut self) -> AskKeyOutcome {
        if self.current + 1 < self.questions.len() {
            self.current += 1;
            self.cursor = 0;
            return AskKeyOutcome::Pending;
        }
        AskKeyOutcome::Done(self.response())
    }

    pub fn response(&self) -> AskResponse {
        let answers = self
            .questions
            .iter()
            .enumerate()
            .map(|(q, question)| {
                let mut idx = self.chosen[q].clone();
                idx.sort_unstable();
                AskAnswer {
                    selected: idx
                        .into_iter()
                        .filter_map(|i| question.options.get(i).map(|o| o.label.clone()))
                        .collect(),
                    other: Some(self.other[q].trim().to_string()).filter(|s| !s.is_empty()),
                    comments: Default::default(),
                        ..Default::default()
                }
            })
            .collect();
        AskResponse {
            answers: Some(answers),
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) -> AskKeyOutcome {
        let multi = self.question().multi_select;
        match code {
            KeyCode::Esc => return AskKeyOutcome::Done(AskResponse::declined()),
            KeyCode::Up => {
                let rows = self.rows();
                self.cursor = (self.cursor + rows - 1) % rows;
            }
            KeyCode::Down | KeyCode::Tab => self.cursor = (self.cursor + 1) % self.rows(),
            KeyCode::Left if self.current > 0 && !self.on_other() => {
                self.current -= 1;
                self.cursor = 0;
            }
            KeyCode::Backspace if self.on_other() => {
                self.other[self.current].pop();
            }
            KeyCode::Char(c) if self.on_other() => self.other[self.current].push(c),
            KeyCode::Char(' ') if multi => self.toggle(),
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let i = (c as u8 - b'1') as usize;
                if i < self.rows() {
                    self.cursor = i;
                    if !self.on_other() {
                        if multi {
                            self.toggle();
                        } else {
                            self.chosen[self.current] = vec![i];
                            return self.advance();
                        }
                    }
                }
            }
            KeyCode::Enter => {
                if self.on_other() {
                    if !multi && !self.other[self.current].trim().is_empty() {
                        self.chosen[self.current].clear();
                    }
                } else if !multi {
                    self.chosen[self.current] = vec![self.cursor];
                } else if self.chosen[self.current].is_empty() {
                    self.toggle();
                }
                if self.answered(self.current) {
                    return self.advance();
                }
            }
            _ => {}
        }
        AskKeyOutcome::Pending
    }

    /// Lines for the inline view: (title, status, rows).
    pub fn view(&self) -> crate::tui::InlineViewState {
        let q = self.question();
        let multi = q.multi_select;
        let mut lines = vec![q.question.clone()];
        for (i, option) in q.options.iter().enumerate() {
            let pointer = if i == self.cursor { '›' } else { ' ' };
            let mark = match (multi, self.chosen[self.current].contains(&i)) {
                (true, true) => "[x] ",
                (true, false) => "[ ] ",
                _ => "",
            };
            let mut row = format!("{pointer} {}. {mark}{}", i + 1, option.label);
            if let Some(desc) = option.description.as_deref().filter(|d| !d.is_empty()) {
                row.push_str(" - ");
                row.push_str(desc);
            }
            lines.push(row);
        }
        if q.allow_other {
            let i = q.options.len();
            let pointer = if i == self.cursor { '›' } else { ' ' };
            let text = &self.other[self.current];
            let shown = if text.is_empty() && i != self.cursor {
                "(type your own answer)".to_string()
            } else {
                format!("{text}▏")
            };
            lines.push(format!("{pointer} {}. Other: {shown}", i + 1));
        }
        let header = q.header.as_deref().unwrap_or("Question");
        let title = if self.questions.len() > 1 {
            format!(
                "Ask · {header} ({}/{})",
                self.current + 1,
                self.questions.len()
            )
        } else {
            format!("Ask · {header}")
        };
        let keys = if multi {
            "↑↓ move · Space toggle · Enter confirm · Esc decline"
        } else {
            "↑↓ move · Enter choose · Esc decline"
        };
        crate::tui::InlineViewState {
            title,
            status: Some(keys.to_string()),
            lines,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::AskOption;

    fn q(multi: bool, other: bool) -> AskQuestion {
        AskQuestion {
            question: "Pick?".into(),
            header: None,
            options: vec![
                AskOption {
                    label: "A".into(),
                    description: Some("first".into()),
                },
                AskOption {
                    label: "B".into(),
                    description: None,
                },
            ],
            multi_select: multi,
            allow_other: other,
        }
    }

    fn done(o: AskKeyOutcome) -> AskResponse {
        match o {
            AskKeyOutcome::Done(r) => r,
            AskKeyOutcome::Pending => panic!("expected done"),
        }
    }

    #[test]
    fn single_select_moves_and_advances_across_questions() {
        let mut s = AskPickerState::new("r".into(), vec![q(false, false), q(false, false)]);
        s.handle_key(KeyCode::Down);
        assert!(matches!(
            s.handle_key(KeyCode::Enter),
            AskKeyOutcome::Pending
        ));
        assert_eq!(s.current, 1);
        assert!(s.view().title.contains("(2/2)"));
        let r = done(s.handle_key(KeyCode::Char('1')));
        let a = r.answers.unwrap();
        assert_eq!(a[0].selected, vec!["B"]);
        assert_eq!(a[1].selected, vec!["A"]);
    }

    #[test]
    fn multi_select_toggles_and_other_text() {
        let mut s = AskPickerState::new("r".into(), vec![q(true, true)]);
        s.handle_key(KeyCode::Char(' '));
        s.handle_key(KeyCode::Down);
        s.handle_key(KeyCode::Char(' '));
        s.handle_key(KeyCode::Down);
        assert!(s.view().lines.last().unwrap().starts_with("› 3. Other"));
        for c in "zz".chars() {
            s.handle_key(KeyCode::Char(c));
        }
        s.handle_key(KeyCode::Backspace);
        let a = done(s.handle_key(KeyCode::Enter)).answers.unwrap();
        assert_eq!(a[0].selected, vec!["A", "B"]);
        assert_eq!(a[0].other.as_deref(), Some("z"));
    }

    #[test]
    fn esc_declines_and_enter_on_empty_other_waits() {
        let mut s = AskPickerState::new("r".into(), vec![q(false, true)]);
        s.handle_key(KeyCode::Up);
        assert!(matches!(
            s.handle_key(KeyCode::Enter),
            AskKeyOutcome::Pending
        ));
        assert_eq!(done(s.handle_key(KeyCode::Esc)), AskResponse::declined());
    }
}

impl super::App {
    pub(super) fn open_ask_picker(&mut self, request_id: String, questions: Vec<AskQuestion>) {
        if questions.is_empty() {
            return;
        }
        let picker = AskPickerState::new(request_id, questions);
        self.inline_interactive_state = None;
        self.set_inline_view(picker.view());
        self.ask_picker = Some(picker);
        self.set_status_notice("The agent is asking you a question");
    }

    /// Route a key to the open ask picker. Returns the reply once finished.
    pub(super) fn handle_ask_picker_key(&mut self, code: KeyCode) -> Option<(String, AskResponse)> {
        let picker = self.ask_picker.as_mut()?;
        match picker.handle_key(code) {
            AskKeyOutcome::Pending => {
                let view = picker.view();
                self.set_inline_view(view);
                None
            }
            AskKeyOutcome::Done(response) => {
                let request_id = picker.request_id.clone();
                self.ask_picker = None;
                self.clear_inline_view();
                self.sync_btw_card();
                Some((request_id, response))
            }
        }
    }
}
