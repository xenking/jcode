use std::collections::HashSet;

#[derive(Default)]
pub(super) struct CloudRecovery {
    reminder: Option<(String, String)>,
    diagnosis_pending: bool,
    attempted: HashSet<(String, String)>,
}

impl CloudRecovery {
    pub(super) fn reminder(&self, session: &str) -> Option<&str> {
        self.reminder
            .as_ref()
            .filter(|(owner, _)| owner == session)
            .map(|(_, text)| text.as_str())
    }

    pub(super) fn diagnosis_pending(&self, session: &str) -> bool {
        self.diagnosis_pending && self.reminder(session).is_some()
    }

    pub(super) fn cancel_diagnosis(&mut self, session: &str) -> bool {
        if !self.diagnosis_pending(session) {
            return false;
        }
        self.diagnosis_pending = false;
        true
    }

    pub(super) fn take_reminder(&mut self, session: &str) -> Option<String> {
        self.reminder(session)?;
        self.diagnosis_pending = false;
        self.reminder.take().map(|(_, text)| text)
    }

    pub(super) fn clear_after_delivery(&mut self) {
        self.reminder = None;
        self.diagnosis_pending = false;
    }

    pub(super) fn allow_user_retry(&mut self, session: &str, command: &str) {
        self.attempted.remove(&(session.into(), command.into()));
    }

    pub(super) fn note_failure(&mut self, session: &str, command: &str, reminder: String) {
        self.diagnosis_pending = self.attempted.insert((session.into(), command.into()))
            || self.diagnosis_pending(session);
        self.reminder = Some((session.into(), reminder));
    }

    pub(super) fn note_replacement(&mut self, session: String, reminder: String) {
        self.reminder = Some((session, reminder));
    }
}
