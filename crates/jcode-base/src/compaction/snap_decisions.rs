//! Optional, grounded enrichment. Never part of the archive/apply critical path.
use super::*;

pub(super) struct DecisionsJob {
    pub provider: Arc<dyn Provider>,
    pub index: String,
}

pub(super) struct PendingDecisions {
    task: JoinHandle<()>,
    result: tokio::sync::oneshot::Receiver<(usize, String)>,
}

impl Drop for PendingDecisions {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn tag(line: &str) -> Option<&str> {
    let end = line.find(']')?;
    line.starts_with("[message ").then_some(&line[..=end])
}

pub(super) fn retain_supported(decisions: &str, index: &str) -> String {
    let tags: std::collections::HashSet<_> = index.lines().filter_map(tag).collect();
    let mut output = String::new();
    for line in decisions
        .lines()
        .filter(|line| tag(line).is_some_and(|tag| tags.contains(tag)))
    {
        if output.len() + line.len() < 24_000 {
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

pub(super) fn index_since(index: &str, first_message: usize) -> String {
    index
        .lines()
        .filter(|line| {
            line.strip_prefix("[message ")
                .and_then(|line| line.split_once(','))
                .and_then(|(n, _)| n.parse::<usize>().ok())
                .is_some_and(|n| n >= first_message)
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

fn cheap_family(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    !model.contains("opus")
        && ["luna", "haiku", "flash", "mini", "nano"]
            .iter()
            .any(|family| model.contains(family))
}

pub(super) fn choose_provider(
    provider: Arc<dyn Provider>,
    configured: &str,
) -> Option<Arc<dyn Provider>> {
    let configured = configured.trim();
    if configured.eq_ignore_ascii_case("off") || configured.to_ascii_lowercase().contains("opus") {
        return None;
    }
    let fork = provider.fork();
    if Arc::ptr_eq(&fork, &provider) {
        return None;
    }
    if !configured.is_empty() && !configured.eq_ignore_ascii_case("auto") {
        if fork.set_model(configured).is_ok() && !fork.model().to_ascii_lowercase().contains("opus")
        {
            return Some(fork);
        }
        crate::logging::warn(
            "[compaction] decisions model unavailable; keeping verbatim index only",
        );
        return None;
    }
    let mut routes: Vec<_> = provider
        .model_routes()
        .into_iter()
        .filter(|route| route.available && cheap_family(&route.model))
        .collect();
    routes.sort_by_key(|route| {
        (
            route
                .cheapness
                .as_ref()
                .and_then(|price| price.estimated_reference_cost_micros)
                .unwrap_or(u64::MAX),
            route.model.clone(),
        )
    });
    for route in routes {
        if fork
            .set_route_selection(&jcode_provider_core::RouteSelection::from_model_route(
                &route,
            ))
            .is_ok()
            && cheap_family(&fork.model())
        {
            return Some(fork);
        }
    }
    for model in provider
        .available_models_display()
        .into_iter()
        .filter(|model| cheap_family(model))
    {
        if fork.set_model(&model).is_ok() && cheap_family(&fork.model()) {
            return Some(fork);
        }
    }
    crate::logging::info(
        "[compaction] no available cheap decisions route; keeping verbatim index only",
    );
    None
}

pub(super) fn start(job: DecisionsJob, model: String, compacted_count: usize) -> PendingDecisions {
    let (send, result) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let provider =
            tokio::task::spawn_blocking(move || choose_provider(job.provider, &model)).await;
        let decisions = match provider {
            Ok(Some(provider)) => {
                summary_generation::extract_snapcompact_decisions(&*provider, &job.index).await
            }
            _ => String::new(),
        };
        let _ = send.send((compacted_count, decisions));
        crate::bus::Bus::global().publish(crate::bus::BusEvent::CompactionFinished);
    });
    PendingDecisions { task, result }
}

impl CompactionManager {
    pub fn has_pending_decisions(&self) -> bool {
        self.pending_decisions.is_some()
    }

    /// Allows the session owner to persist enrichment without another divider.
    pub fn take_decisions_updated(&mut self) -> bool {
        std::mem::take(&mut self.decisions_updated)
    }

    pub(super) fn poll_decisions(&mut self) {
        let Some(pending) = self.pending_decisions.as_mut() else {
            return;
        };
        let (count, decisions) = match pending.result.try_recv() {
            Ok(result) => result,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                self.pending_decisions = None;
                return;
            }
        };
        self.pending_decisions = None;
        if count != self.compacted_count || decisions.is_empty() {
            return;
        }
        if let Some(archive) = self
            .active_summary
            .as_mut()
            .and_then(|summary| summary.snapcompact.as_mut())
        {
            let supported = retain_supported(&decisions, &archive.index);
            if !supported.is_empty() {
                archive.decisions.push_str(&supported);
                archive.decisions = retain_supported(&archive.decisions, &archive.index);
                self.decisions_updated = true;
                crate::logging::info(&format!(
                    "[TIMING] snapcompact_decisions_attached: compacted_count={}, chars={}",
                    count,
                    supported.chars().count()
                ));
            }
        }
    }
}
