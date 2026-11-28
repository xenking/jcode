//! Entry point of the detached worker process.
//!
//! The server spawns `jcode __background-worker` with one job on stdin (see
//! [`super::background_worker`]) so the work survives a server reload. This is
//! the only place that knows every job kind. The jobs themselves depend on the
//! spawning protocol, never on this dispatcher.

use super::agentgrep::AgentGrepJob;
use super::background_worker::DetachedJob;
use super::bridge_reload::BridgeLaunch;
use super::remote_exec::RemoteBashJob;
use super::selfdev::SelfDevJob;
use crate::background::{self, TaskResult};
use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

/// Variant names are the `DetachedJob::KIND` tags used on the wire.
#[derive(Deserialize)]
enum Job {
    SelfDev(SelfDevJob),
    BridgeReload(BridgeLaunch),
    AgentGrep(AgentGrepJob),
    RemoteBash(RemoteBashJob),
}

impl Job {
    async fn run(self, output: PathBuf) -> Result<TaskResult> {
        match self {
            Self::SelfDev(job) => job.run(output).await,
            Self::BridgeReload(job) => job.run(output).await,
            Self::AgentGrep(job) => job.run(output).await,
            Self::RemoteBash(job) => job.run(output).await,
        }
    }
}

pub fn run() -> Result<()> {
    let (task_id, job): (String, Job) = serde_json::from_reader(std::io::stdin().lock())?;
    anyhow::ensure!(
        !task_id.is_empty() && task_id.bytes().all(|ch| ch.is_ascii_alphanumeric()),
        "invalid background task ID"
    );
    let output = background::global().output_path_for(&task_id);
    let receipt = output.with_extension("exit");
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(job.run(output))
        .unwrap_or_else(|error| TaskResult::failed(None, error.to_string()));
    crate::storage::write_json(&receipt, &result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::background_worker::envelope;

    fn round_trip<J: DetachedJob>(job: &J) -> Job {
        let bytes = envelope("task1", job).expect("envelope");
        let (task_id, job): (String, Job) = serde_json::from_slice(&bytes).expect("worker decodes");
        assert_eq!(task_id, "task1");
        job
    }

    #[test]
    fn every_job_kind_reaches_its_variant() {
        use crate::tool::background_worker::CallContext;
        let context = || CallContext::for_session("s");
        assert!(matches!(
            round_trip(&AgentGrepJob::new(serde_json::json!({}), context())),
            Job::AgentGrep(_)
        ));
        assert!(matches!(
            round_trip(&RemoteBashJob::new(serde_json::json!({}), context())),
            Job::RemoteBash(_)
        ));
        let launch = BridgeLaunch {
            program: "jcode".into(),
            args: Vec::new(),
        };
        assert!(matches!(round_trip(&launch), Job::BridgeReload(_)));
        assert!(matches!(
            round_trip(&SelfDevJob::watching("r".into(), "w".into())),
            Job::SelfDev(_)
        ));
    }
}
