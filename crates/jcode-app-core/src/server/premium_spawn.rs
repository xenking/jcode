use super::comm_session::spawn_model_selection::SwarmSpawnSelection;
use super::*;
use crate::protocol::{AskOption, AskQuestion};
use crate::session::WorkerModelApproval;
use anyhow::Context;

const ALLOW: &str = "Allow this worker";
const DENY: &str = "Deny";

#[derive(Debug, serde::Serialize, serde::Deserialize)]
enum SpawnRecord {
    Started,
    Registered(String),
}

pub(super) struct SpawnReplay(std::path::PathBuf);

impl SpawnReplay {
    pub(super) fn load(key: &str) -> Result<(Self, Option<String>)> {
        let path = crate::storage::jcode_dir()?
            .join("premium-spawns")
            .join(format!("{key}.json"));
        let replay = Self(path);
        let record = match std::fs::read(&replay.0) {
            Ok(bytes) => Some(serde_json::from_slice::<SpawnRecord>(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        match record {
            Some(SpawnRecord::Started) => anyhow::bail!(
                "This premium spawn was interrupted after approval and will not be repeated. Check the swarm before requesting a new worker."
            ),
            Some(SpawnRecord::Registered(worker)) => Ok((replay, Some(worker))),
            None => Ok((replay, None)),
        }
    }

    pub(super) fn start(&self) -> Result<()> {
        crate::storage::write_json(&self.0, &SpawnRecord::Started)
    }

    pub(super) fn register(&self, worker: &str) -> Result<()> {
        crate::storage::write_json(&self.0, &SpawnRecord::Registered(worker.to_owned()))
    }
}

pub(super) struct PreparedSwarmWorker {
    pub provider: Arc<dyn Provider>,
    pub model: String,
    pub provider_key: Option<String>,
    pub route_api_method: Option<String>,
    pub effort: Option<String>,
    pub approval: Option<WorkerModelApproval>,
}

impl PreparedSwarmWorker {
    pub(super) fn resolve(
        template: &dyn Provider,
        selection: SwarmSpawnSelection,
        requested_effort: &str,
    ) -> Result<Self> {
        let provider = template.fork();
        let mut provider_key = selection.provider_key;
        let mut route_api_method = selection.route_api_method;
        if let Some(model) = selection.model {
            let restore = crate::provider::restore_session_model(
                provider.as_ref(),
                &model,
                provider_key.as_deref(),
                route_api_method.as_deref(),
            );
            if let Err(error) = &restore
                && (provider_key.is_some()
                    || route_api_method.is_some()
                    || WorkerModelApproval::required_for(&model))
            {
                anyhow::bail!("Cannot restore worker route for '{model}': {error:#}");
            }
            let actual = model_with_provider_pin(provider.as_ref());
            anyhow::ensure!(
                super::headless::models_are_equivalent(&actual, &model),
                "Cannot spawn on '{model}': it would run '{actual}' instead ({})",
                restore
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "the switch reported success".into())
            );
            if let Ok(restore) = restore {
                provider_key =
                    crate::provider::MultiProvider::session_provider_key_after_model_switch(
                        &restore.request,
                        provider.name(),
                        provider_key.as_deref(),
                    );
                if let Some(healed) = restore.healed_route {
                    route_api_method = healed.route_api_method;
                }
            }
        }
        if provider_key.is_none() {
            provider_key = crate::provider::MultiProvider::session_provider_key_after_model_switch(
                &provider.model(),
                provider.name(),
                None,
            );
        }
        let model = model_with_provider_pin(provider.as_ref());
        let provider_key_for_policy = provider_key.as_deref().unwrap_or(provider.name());
        Agent::validate_worker_route(provider_key_for_policy, &model)?;
        let constrained =
            Agent::worker_effort_is_constrained(provider.as_ref(), provider_key.as_deref(), &model);
        let required = WorkerModelApproval::required_for(&model);
        let requested_effort = if required {
            crate::prompt::swarm_root_reasoning_effort(requested_effort).unwrap_or(requested_effort)
        } else {
            requested_effort
        };
        let effective = Agent::worker_effort_for_provider(
            provider.as_ref(),
            provider_key.as_deref(),
            requested_effort,
        )?;
        if let Err(error) = provider.set_reasoning_effort(&effective) {
            if required || constrained {
                return Err(
                    error.context("Cannot resolve swarm worker effort before allocating a worker")
                );
            }
            crate::logging::warn(&format!(
                "Could not set swarm worker effort {effective}: {error}"
            ));
        }
        let effort = provider.reasoning_effort();
        if constrained {
            let current = effort
                .as_deref()
                .context("Provider did not report the worker effort")?;
            anyhow::ensure!(
                Agent::worker_effort_for_provider(
                    provider.as_ref(),
                    provider_key.as_deref(),
                    current
                )? == current,
                "Provider did not apply a permitted swarm worker effort"
            );
        }
        Ok(Self {
            provider,
            model,
            provider_key,
            route_api_method,
            effort,
            approval: None,
        })
    }

    pub(super) async fn authorize(
        &mut self,
        requester: &str,
        members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
        lane_or_label: Option<&str>,
    ) -> Result<()> {
        if !WorkerModelApproval::required_for(&self.model) {
            return Ok(());
        }
        let effort = self
            .effort
            .as_deref()
            .filter(|effort| jcode_provider_core::reasoning_effort_rank(effort).is_some())
            .context("Cannot confirm a premium worker without an exact effort")?;
        let provider = self.provider.name();
        let owner = owner_session(requester, members)
            .await
            .context("No trusted owner for this premium worker")?;
        let label = lane_or_label.unwrap_or(requester);
        let questions = [AskQuestion {
            question: format!(
                "Allow one premium swarm worker for {label} using {} via {provider} at effort {effort}?",
                self.model,
            ),
            header: Some("Premium run".into()),
            options: vec![
                AskOption {
                    label: DENY.into(),
                    description: Some("Do not allocate or run this worker".into()),
                },
                AskOption {
                    label: ALLOW.into(),
                    description: Some(
                        "Allow only this worker at the model and effort shown".into(),
                    ),
                },
            ],
            multi_select: false,
            allow_other: false,
        }];
        let request_id = format!("premium-spawn-{}", uuid::Uuid::new_v4());
        let owner_ui = crate::tool::interactive::owner_sender(&owner)
            .context("Premium worker requires explicit owner approval from an attached UI")?;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            crate::tool::ask_client(owner_ui, &questions, &request_id),
        )
        .await
        .ok()
        .and_then(Result::ok);
        let allowed = response.and_then(|response| response.answers).is_some_and(|answers| {
            matches!(answers.as_slice(), [answer] if answer.selected == [ALLOW] && answer.other.is_none())
        });
        anyhow::ensure!(
            allowed,
            "Premium worker {} at {effort} requires explicit owner approval",
            self.model
        );
        self.approval = Some(WorkerModelApproval {
            model: self.model.clone(),
            provider: provider.to_string(),
            effort: effort.to_string(),
        });
        Ok(())
    }

    pub(super) fn recheck_approved_route(&self) -> Result<()> {
        let Some(approval) = &self.approval else {
            return Ok(());
        };
        let key = self.provider_key.as_deref();
        Agent::validate_worker_route(key.unwrap_or(self.provider.name()), &self.model)?;
        anyhow::ensure!(
            Agent::worker_effort_for_provider(self.provider.as_ref(), key, &approval.effort)?
                == approval.effort
                && self.provider.reasoning_effort().as_deref() == Some(approval.effort.as_str()),
            "Worker effort changed while waiting for approval. Request a new spawn."
        );
        Ok(())
    }
}

async fn owner_session(
    requester: &str,
    members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> Option<String> {
    let members = members.read().await;
    let mut seen = HashSet::new();
    let mut current = members.get(requester)?;
    while let Some(parent) = current.report_back_to_session_id.as_ref() {
        if !seen.insert(&current.session_id) {
            return None;
        }
        current = members.get(parent)?;
    }
    (!current.is_headless).then(|| current.session_id.clone())
}

fn model_with_provider_pin(provider: &dyn Provider) -> String {
    let model = provider.model();
    provider
        .explicit_provider_pin_for_current_model()
        .map(|pin| format!("{model}@{pin}"))
        .unwrap_or(model)
}
