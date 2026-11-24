//! The gate: a turn held at a reached limit until the human answers.

use std::time::Duration;

use async_trait::async_trait;
use jcode_routing_policy::{AccountId, GateOption};
use tokio::sync::mpsc;

use super::{GateRequest, RoutingService, TurnInput, wire_option};
use crate::protocol::{RoutingGateChoice, ServerEvent};

/// A gate nobody answers fails the turn after this long.
const GATE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// A `wait` answer never holds a turn longer than this.
const MAX_WAIT_SECS: i64 = 12 * 3_600;
/// Slack after the reset time, so the provider has rolled the window over.
const WAIT_SLACK_SECS: i64 = 5;
const MAX_ACK_MINUTES: u32 = 24 * 60;

/// Where a gate shows up and whether anyone is still there to answer.
#[async_trait]
pub trait GateSink: Send + Sync {
    fn send(&self, event: ServerEvent);
    /// Resolves when the client that would answer is gone.
    async fn closed(&self);
}

pub(super) struct Slot {
    pub session: String,
    pub answers: mpsc::UnboundedSender<RoutingGateChoice>,
}

struct Registered<'a> {
    service: &'a RoutingService,
    gate_id: String,
    sink: &'a dyn GateSink,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.service
            .gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.gate_id);
        self.sink.send(ServerEvent::RoutingGateClosed {
            gate_id: self.gate_id.clone(),
        });
    }
}

enum Applied {
    /// An intent was written. Decide again.
    Decided,
    /// The human waits for the window to reset.
    Waiting { until_unix: i64 },
    Cancelled,
    /// Not one of the offered options. The gate stays open.
    Ignored,
}

impl RoutingService {
    /// Opens a gate and returns once the answer has been applied to the intents
    /// (decide again), the wait elapsed, or the turn must be dropped (`Err`).
    pub(super) async fn ask(
        &self,
        input: &TurnInput,
        request: GateRequest,
        sink: &dyn GateSink,
    ) -> Result<(), String> {
        let gate_id = self.next_gate_id();
        let (tx, mut answers) = mpsc::unbounded_channel();
        self.gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                gate_id.clone(),
                Slot {
                    session: input.session_id.clone(),
                    answers: tx,
                },
            );
        let _registered = Registered {
            service: self,
            gate_id: gate_id.clone(),
            sink,
        };
        sink.send(ServerEvent::RoutingGate {
            gate_id: gate_id.clone(),
            reason: request.reason.clone(),
            options: request.options.iter().map(wire_option).collect(),
        });
        let mut deadline = tokio::time::Instant::now() + GATE_TIMEOUT;
        let mut waiting = false;
        loop {
            tokio::select! {
                answer = answers.recv() => {
                    let Some(choice) = answer else {
                        return Err("routing gate closed without an answer".into());
                    };
                    match self.apply_choice(input, &request, choice) {
                        Applied::Decided => return Ok(()),
                        Applied::Cancelled => return Err("turn cancelled at the routing gate".into()),
                        Applied::Ignored => {}
                        Applied::Waiting { until_unix } => {
                            let secs = (until_unix - self.env.now() + WAIT_SLACK_SECS).clamp(0, MAX_WAIT_SECS);
                            deadline = tokio::time::Instant::now() + Duration::from_secs(secs as u64);
                            waiting = true;
                        }
                    }
                }
                () = sink.closed() => {
                    return Err("client left while the turn waited at a routing gate".into());
                }
                () = tokio::time::sleep_until(deadline) => {
                    return if waiting {
                        Ok(())
                    } else {
                        Err("routing gate timed out without an answer".into())
                    };
                }
            }
        }
    }

    fn apply_choice(
        &self,
        input: &TurnInput,
        request: &GateRequest,
        choice: RoutingGateChoice,
    ) -> Applied {
        let now = self.env.now();
        let session = input.session_id.as_str();
        match choice {
            RoutingGateChoice::Cancel => Applied::Cancelled,
            RoutingGateChoice::Wait => request
                .options
                .iter()
                .find_map(|option| match option {
                    GateOption::WaitUntil { resets_at } => Some(*resets_at),
                    _ => None,
                })
                .map_or(Applied::Ignored, |until_unix| Applied::Waiting { until_unix }),
            RoutingGateChoice::Switch { account } => {
                let Some(account) = AccountId::parse(&account).filter(|account| {
                    request.options.iter().any(
                        |option| matches!(option, GateOption::Switch { account: offered, .. } if offered == account),
                    )
                }) else {
                    return Applied::Ignored;
                };
                self.write_intent(now, |state| state.set_switch(session, Some(&account), now))
            }
            RoutingGateChoice::Ack { minutes } => {
                let Some(account) = request.options.iter().find_map(|option| match option {
                    GateOption::Ack { account } => Some(account.clone()),
                    _ => None,
                }) else {
                    return Applied::Ignored;
                };
                let minutes = i64::from(minutes.clamp(1, MAX_ACK_MINUTES));
                self.write_intent(now, |state| state.ack(&account, now + minutes * 60))
            }
        }
    }

    fn write_intent(
        &self,
        now: i64,
        mut change: impl FnMut(&mut jcode_routing_policy::state::RoutingState) + Send,
    ) -> Applied {
        match self.env.update_state(now, &mut change) {
            Ok(()) => Applied::Decided,
            Err(error) => {
                crate::logging::warn(&format!("routing: could not store the gate answer: {error}"));
                Applied::Ignored
            }
        }
    }
}
