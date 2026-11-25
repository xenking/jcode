//! Admission and cancellation of work resumed after a server reload, as the
//! agent sees it.
//!
//! The durable recovery journal belongs to the server, which installs itself
//! here when it starts owning recovery. A process that never ran a
//! server (a plain headless run) has no journal: every input is admitted and
//! nothing is pending to cancel, which is exactly what the journal answers
//! when it holds no record.

use crate::protocol::TurnIdentity;
use crate::session::Session;
use anyhow::Result;
use std::sync::OnceLock;

/// What the agent asks of the recovery journal.
pub(crate) trait RecoveryGate: Send + Sync {
    /// The user stopped the session: retire any pending recovery for it.
    fn cancel(&self, session_id: &str) -> Result<()>;

    /// Run `persist_input` when the journal admits this input.
    fn admit_input(
        &self,
        session_id: &str,
        identity: &TurnIdentity,
        input_ids: Option<&[String]>,
        persist_input: &mut dyn FnMut() -> Result<()>,
    ) -> Result<bool>;

    /// Whether the answered ask may continue the recovered turn.
    fn admit_ask_answer(
        &self,
        session: &Session,
        identity: &TurnIdentity,
        tool_id: &str,
    ) -> Result<bool>;
}

struct NoJournal;

impl RecoveryGate for NoJournal {
    fn cancel(&self, _session_id: &str) -> Result<()> {
        Ok(())
    }

    fn admit_input(
        &self,
        _session_id: &str,
        _identity: &TurnIdentity,
        input_ids: Option<&[String]>,
        persist_input: &mut dyn FnMut() -> Result<()>,
    ) -> Result<bool> {
        // Recovered input is always tagged `reload:`; without a journal there is
        // no record that could admit it.
        if input_ids.is_some_and(|ids| ids.iter().any(|id| id.starts_with("reload:"))) {
            return Ok(false);
        }
        persist_input()?;
        Ok(true)
    }

    fn admit_ask_answer(
        &self,
        _session: &Session,
        _identity: &TurnIdentity,
        _tool_id: &str,
    ) -> Result<bool> {
        Ok(true)
    }
}

static GATE: OnceLock<&'static dyn RecoveryGate> = OnceLock::new();

/// Install the journal-backed gate. The first installation wins.
pub(crate) fn install(gate: &'static dyn RecoveryGate) {
    let _ = GATE.set(gate);
}

fn gate() -> &'static dyn RecoveryGate {
    GATE.get().copied().unwrap_or(&NoJournal)
}

pub(crate) fn cancel(session_id: &str) -> Result<()> {
    gate().cancel(session_id)
}

pub(crate) fn admit_input(
    session_id: &str,
    identity: &TurnIdentity,
    input_ids: Option<&[String]>,
    persist_input: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    let mut persist_input = Some(persist_input);
    gate().admit_input(session_id, identity, input_ids, &mut || {
        persist_input.take().map_or(Ok(()), |persist| persist())
    })
}

pub(crate) fn admit_ask_answer(
    session: &Session,
    identity: &TurnIdentity,
    tool_id: &str,
) -> Result<bool> {
    gate().admit_ask_answer(session, identity, tool_id)
}
