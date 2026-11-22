use super::*;
use crate::tool::ToolOutput;
use tokio::task::JoinHandle;

const MAX_PARALLEL_READS: usize = 4;
type Handle = (JoinHandle<Result<ToolOutput>>, Instant);

#[derive(Default)]
pub(super) struct ParallelReads(HashMap<String, Handle>);

impl ParallelReads {
    pub fn take(&mut self, id: &str) -> Option<Handle> {
        self.0.remove(id)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for ParallelReads {
    fn drop(&mut self) {
        for handle in self.0.values() {
            handle.0.abort();
        }
    }
}

impl Agent {
    pub(super) async fn start_parallel_reads(
        &self,
        calls: &[ToolCall],
        sdk_results: &HashMap<String, (String, bool)>,
        message_id: &str,
    ) -> ParallelReads {
        let mut eligible = Vec::new();
        let mut seen = HashSet::new();
        for call in calls.iter().take(MAX_PARALLEL_READS) {
            if call.validation_error().is_some()
                || sdk_results.contains_key(&call.id)
                || self.validate_tool_allowed(&call.name).is_err()
                || !seen.insert(&call.id)
            {
                break;
            }
            let ctx = ToolContext {
                session_id: self.session.id.clone(),
                message_id: message_id.to_string(),
                tool_call_id: call.id.clone(),
                working_dir: self.working_dir().map(PathBuf::from),
                stdin_request_tx: self.stdin_request_tx.clone(),
                graceful_shutdown_signal: Some(self.graceful_shutdown.clone()),
                execution_mode: ToolExecutionMode::AgentTurn,
            };
            if !self
                .registry
                .parallel_read_allowed(&call.name, &call.input, &ctx)
                .await
            {
                break;
            }
            eligible.push((call, ctx));
        }
        if eligible.len() < 2 {
            return ParallelReads::default();
        }
        let mut pending = ParallelReads::default();
        for (call, ctx) in eligible {
            let registry = self.registry.clone();
            let name = call.name.clone();
            let input = call.input.clone();
            let started = Instant::now();
            let Some(handle) =
                crate::runtime_activity::spawn_session(&self.session.id, async move {
                    registry.execute_parallel_read(&name, input, ctx).await
                })
            else {
                return ParallelReads::default();
            };
            pending.0.insert(call.id.clone(), (handle, started));
        }
        pending
    }
}
