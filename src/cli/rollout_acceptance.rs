use super::{Layout, Pair};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SCHEMA: u32 = 1;
const CORE_FLOWS: [&str; 7] = [
    "send",
    "queue_while_busy",
    "btw",
    "ask_answer",
    "image_paste",
    "transcript_scroll",
    "restart_restore",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "UPPERCASE")]
enum ResultStatus {
    Pass,
    Issues,
    Blocked,
}

#[derive(Debug, Deserialize, Serialize)]
struct Receipt {
    schema: u32,
    pair: Pair,
    flows: BTreeMap<String, ResultStatus>,
    result: ResultStatus,
    finished_at: chrono::DateTime<chrono::Utc>,
}

fn path(dir: &Path, pair: &Pair) -> Result<PathBuf> {
    for sha in [&pair.cli, &pair.desktop] {
        ensure!(
            matches!(sha.len(), 40 | 64)
                && sha
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "expected a full Git object ID, got {sha:?}"
        );
    }
    Ok(dir.join(format!("{}-{}.json", pair.cli, pair.desktop)))
}

pub(super) fn require(layout: &Layout, pair: &Pair) -> Result<PathBuf> {
    let path = path(&layout.acceptance_dir, pair)?;
    validate(&path, pair).with_context(|| {
        let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
        let script = layout.jcode_repo.join("scripts/accept-smoke.py");
        let runner = if script.is_file() {
            format!("python3 {}", quote(&script))
        } else {
            format!(
                "git -C {} show origin/master:scripts/accept-smoke.py | python3 -",
                quote(&layout.jcode_repo)
            )
        };
        format!(
            "rollout refused: no passing acceptance smoke receipt at {}\nRun: {runner} --jcode-repo {} --desktop-repo {} --jcode-sha {} --desktop-sha {} --receipt-dir {}",
            path.display(),
            quote(&layout.jcode_repo),
            quote(&layout.desktop_repo),
            pair.cli,
            pair.desktop,
            quote(&layout.acceptance_dir),
        )
    })?;
    Ok(path)
}

fn validate(path: &Path, pair: &Pair) -> Result<()> {
    let receipt: Receipt = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(receipt.schema == SCHEMA, "unsupported receipt schema");
    ensure!(receipt.pair == *pair, "receipt SHA pair does not match");
    ensure!(
        receipt.result == ResultStatus::Pass,
        "smoke result is {:?}",
        receipt.result
    );
    for flow in CORE_FLOWS {
        ensure!(
            receipt.flows.get(flow) == Some(&ResultStatus::Pass),
            "required flow {flow} did not pass"
        );
    }
    ensure!(
        receipt.flows.len() == CORE_FLOWS.len(),
        "unexpected smoke flows"
    );
    Ok(())
}

#[cfg(test)]
pub(super) fn write_fixture(dir: &Path, pair: &Pair, not_passed: Option<&str>) -> PathBuf {
    let mut receipt = Receipt {
        schema: SCHEMA,
        pair: pair.clone(),
        flows: CORE_FLOWS
            .into_iter()
            .map(|flow| (flow.to_owned(), ResultStatus::Pass))
            .collect(),
        result: ResultStatus::Pass,
        finished_at: chrono::Utc::now(),
    };
    if let Some(flow) = not_passed {
        receipt.flows.insert(flow.to_owned(), ResultStatus::Blocked);
    }
    let path = path(dir, pair).unwrap();
    std::fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
    path
}
