use super::*;

impl RemoteEventState for RemoteConnection {
    fn finish_local_tools_request(&mut self, id: u64, accepted: bool) -> bool {
        Self::finish_local_tools_request(self, id, accepted)
    }
    fn finish_runtime_command(&mut self, id: u64) -> bool {
        Self::finish_runtime_command(self, id)
    }

    fn request_context_meter(&mut self, session_id: &str) -> Option<u64> {
        let id = self.next_request_id;
        self.next_request_id += 1;
        self.pending_context_meters.insert(id);
        self.send_request_detached(
            Request::SessionContextBreakdown {
                id,
                session_id: Some(session_id.to_owned()),
                model: None,
                cwd: None,
                summary_only: true,
                include_snapcompact: false,
            },
            "context meter",
        );
        Some(id)
    }

    fn finish_context_meter(&mut self, id: u64) -> bool {
        self.pending_context_meters.remove(&id)
    }

    fn tool_events(&mut self) -> &mut RemoteDiffTracker {
        &mut self.tool_diff
    }

    fn call_output_tokens_seen(&mut self) -> &mut u64 {
        Self::call_output_tokens_seen(self)
    }

    fn reset_call_output_tokens_seen(&mut self) {
        Self::reset_call_output_tokens_seen(self);
    }

    fn set_session_id(&mut self, id: String) {
        Self::set_session_id(self, id);
    }

    fn has_loaded_history(&self) -> bool {
        Self::has_loaded_history(self)
    }

    fn mark_history_loaded(&mut self) {
        Self::mark_history_loaded(self);
    }
}

impl RemoteEventState for ReplayRemoteState {
    fn tool_events(&mut self) -> &mut RemoteDiffTracker {
        &mut self.tool_diff
    }

    fn call_output_tokens_seen(&mut self) -> &mut u64 {
        &mut self.call_output_tokens_seen
    }

    fn reset_call_output_tokens_seen(&mut self) {
        self.call_output_tokens_seen = 0;
    }

    fn set_session_id(&mut self, _id: String) {}

    fn has_loaded_history(&self) -> bool {
        true
    }

    fn mark_history_loaded(&mut self) {}
}
