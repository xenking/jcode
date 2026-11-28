use super::*;
use jcode_harness_api::{RoutingGateChoice, RoutingIntentAction, RoutingView};

impl JcodeClient {
    fn require_routing(&self) -> Result<()> {
        if self.supports(crate::ROUTING_CAPABILITY) {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::UnsupportedCapability,
                "This runtime does not support account routing.",
            ))
        }
    }

    /// Answer a `RoutingGate` event. Fails when the gate is no longer waiting,
    /// for example because another client answered first.
    pub fn answer_routing_gate(
        &self,
        session_id: &str,
        gate_id: &str,
        choice: RoutingGateChoice,
    ) -> Result<()> {
        self.require_routing()?;
        self.request_ok(ApiRequest::RoutingGateAnswer {
            session_id: session_id.to_string(),
            gate_id: gate_id.to_string(),
            choice,
        })
        .map(drop)
    }

    /// Set or read a routing intent for the session. Returns the status text
    /// the daemon prints for the action.
    pub fn routing_intent(&self, session_id: &str, action: RoutingIntentAction) -> Result<String> {
        self.require_routing()?;
        match self
            .request_ok(ApiRequest::RoutingIntent {
                session_id: session_id.to_string(),
                action,
            })?
            .event
        {
            ApiEvent::RoutingStatus { text } => Ok(text),
            other => Err(unexpected("routing_status", &other)),
        }
    }

    /// The routing configuration with live usage. Needs no attached session.
    pub fn routing_view(&self) -> Result<RoutingView> {
        self.require_routing()?;
        match self.request_ok(ApiRequest::GetRoutingView)?.event {
            ApiEvent::RoutingView { view } => Ok(view),
            other => Err(unexpected("routing_view", &other)),
        }
    }
}
