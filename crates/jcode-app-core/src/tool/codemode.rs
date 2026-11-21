//! `codemode` tool: run model-written JavaScript in an embedded QuickJS
//! sandbox that can call any tool available to the session.
//!
//! Enabled by default (`[features] code_mode` and `[tools] codemode`, both
//! true unless turned off; `JCODE_CODEMODE=0` also disables it). See
//! `jcode-codemode` for the sandbox itself. This module is the harness side:
//! it lists the session's tools, routes calls through [`Registry::execute`]
//! (so session policy, hooks, and MCP routing all apply), and persists the
//! script `store()` per session between runs.

use super::{Registry, Tool, ToolContext, ToolOutput, WeakRegistry};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::future::BoxFuture;
use jcode_codemode::{Host, Limits, ToolInfo};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Tools never reachable from inside a script: itself (no recursion) and
/// `batch` (pointless inside a script that already has `Promise.all`).
const EXCLUDED: &[&str] = &["codemode", "batch"];

/// Maximum characters of the final report returned to the model.
const MAX_REPORT_CHARS: usize = 60_000;
const MIN_OUTPUT_TOKENS: usize = 512;
const MAX_OUTPUT_TOKENS: usize = 15_000;

const DESCRIPTION: &str = "Run one JavaScript or TypeScript program that calls tools itself. Reach for it when work needs several calls: dependent chains, loops over a previous result, 5+ calls, or big outputs you only want summarized. Only your return value and console.log output reach the conversation, so intermediate data costs no context. Prefer it over repeated single calls or `batch`; use `batch` for 1-10 independent calls whose results you want verbatim, and `bash` for shell work.\n\
Supply an async function body using await/return; tools.<name>(args) or callTool(name, args) takes normal tool arguments, JSON output becomes objects, and failures throw. Use Promise.all for independent calls. Write JS or TS: type annotations, interfaces, `as`, generics and `satisfies` are erased first, so `types()` output is safe to copy.\n\
listTools(filter?) lists names; types(filter?) and describeTool(name) give TypeScript declarations and schemas. store(key, value), load(key), storedKeys() persist JSON for the session.\n\
No direct filesystem, network, timers, or modules. Limits: 64 MB heap, 300 s, 2000 tool calls.";

pub struct CodemodeTool {
    registry: WeakRegistry,
}

impl CodemodeTool {
    pub(super) fn new(registry: WeakRegistry) -> Self {
        Self { registry }
    }
}

#[derive(Deserialize)]
struct CodemodeInput {
    #[serde(alias = "script", alias = "source", alias = "js")]
    code: String,
    max_output_tokens: Option<usize>,
}

struct RegistryHost {
    registry: Registry,
    ctx: ToolContext,
    counter: AtomicUsize,
}

impl Host for RegistryHost {
    fn call_tool(&self, name: String, args: Value) -> BoxFuture<'static, Result<Value, String>> {
        let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let registry = self.registry.clone();
        let sub_ctx = self.ctx.for_subcall(format!("codemode-{n}-{name}"));
        Box::pin(async move {
            let args = match args {
                Value::Object(mut map) => {
                    // Intermediate data stays in the VM and the final report is
                    // bounded separately, so nested results skip the per-call
                    // model-context budget (MCP fit_to_budget, context guard).
                    // Output size only: no extra authority is granted.
                    map.entry(jcode_tool_core::ACCEPT_LARGE_OUTPUT_KEY)
                        .or_insert(Value::Bool(true));
                    Value::Object(map)
                }
                other => return Err(format!("arguments must be an object, got {other}")),
            };
            match registry.execute(&name, args, sub_ctx).await {
                Ok(output) => Ok(jcode_codemode::decode_tool_output(&output.output)),
                Err(err) => Err(crate::util::format_error_chain(&err)),
            }
        })
    }
}

/// Whether the opt-in codemode tool is enabled in config.
pub(crate) fn enabled() -> bool {
    let config = crate::config::config();
    jcode_codemode::AVAILABLE && config.features.code_mode && config.tools.codemode
}

fn store_path(session_id: &str) -> Result<PathBuf> {
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        anyhow::bail!("codemode store requires a nonempty session id");
    }
    Ok(crate::storage::jcode_dir()?
        .join("codemode")
        .join(format!("{safe}.json")))
}

fn load_store(session_id: &str) -> Result<Map<String, Value>> {
    let path = store_path(session_id)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(error).context("cannot read codemode store"),
    };
    serde_json::from_str(&text).context("invalid codemode store; refusing to overwrite it")
}

fn save_store(session_id: &str, store: &Map<String, Value>) -> Result<()> {
    let path = store_path(session_id)?;
    if store.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("cannot clear codemode store"),
        };
    }
    crate::storage::write_json_fast(&path, store).context("cannot persist codemode store")
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

/// Bound the report. When it is over budget, the full text is spilled to a
/// file under the jcode dir so nothing is silently lost.
fn spill_report(text: &str, spill: Option<PathBuf>) -> Option<PathBuf> {
    spill.and_then(|path| {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .inspect_err(|error| {
                    crate::logging::warn(&format!("Codemode spill directory unavailable: {error}"))
                })
                .ok()?;
        }
        std::fs::write(&path, text)
            .inspect_err(|error| {
                crate::logging::warn(&format!("Codemode report spill failed: {error}"))
            })
            .ok()?;
        Some(path)
    })
}

fn bound_report(text: String, max: usize, spill: Option<PathBuf>) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let total = text.len();
    let saved = spill_report(&text, spill);
    let cut: String = text.chars().take(max).collect();
    match saved {
        Some(path) => format!(
            "{cut}\n...(truncated {total} bytes; full report at {}. Return a smaller value or store() the rest)",
            path.display()
        ),
        None => format!("{cut}\n...(truncated; return a smaller value or store() the rest)"),
    }
}

fn budget_report(
    out: &jcode_codemode::RunOutput,
    max_tokens: usize,
    spill: Option<PathBuf>,
    store_saved: bool,
) -> String {
    let (report, body_end) = render_report_with_body(out);
    let max_bytes = max_tokens * jcode_core::util::APPROX_CHARS_PER_TOKEN;
    if report.len() <= max_bytes {
        return report;
    }

    let mut notice = format!("\n...(truncated to ~{max_tokens} tokens)");
    if let Some(path) = spill_report(&report, spill) {
        let path = path.to_string_lossy();
        if path.len() <= 512 {
            notice.push_str(&format!("\nFull report: {path}"));
        }
    }
    if store_saved {
        if let Some(key) = out.store.keys().find(|key| key.len() <= 64)
            && let Ok(key) = serde_json::to_string(key)
            && key.len() <= 80
        {
            notice.push_str(&format!(
                "\nFor stored data, use storedKeys() and load({key}) in a new script."
            ));
        } else {
            notice.push_str("\nTo page script data, save it with store(key, value) before returning, then slice load(key) in another run.");
        }
    }

    let mut diagnostics = String::new();
    if let Some(error) = &out.error {
        diagnostics.push_str("error: ");
        diagnostics.push_str(crate::util::truncate_str(error, 850));
        if error.len() > 850 {
            diagnostics.push('…');
        }
        diagnostics.push('\n');
    }
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for receipt in &out.receipts {
        *counts.entry(receipt.status.as_str()).or_default() += 1;
    }
    let status = counts
        .iter()
        .map(|(status, count)| format!("{count} {status}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut footer = String::new();
    if !status.is_empty() {
        footer.push_str(&format!(
            "\ncalls:\n  ...{} more ({status})\n",
            out.receipts.len()
        ));
    }
    footer.push_str(&call_count_footer(out));
    let remaining = max_bytes.saturating_sub(diagnostics.len() + notice.len() + footer.len());
    let body = crate::util::truncate_str(&report[..body_end], remaining);
    format!("{diagnostics}{body}{notice}{footer}")
}

fn spill_path(session_id: &str, call_id: &str) -> Option<PathBuf> {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    Some(
        crate::storage::jcode_dir()
            .inspect_err(|error| {
                crate::logging::warn(&format!("Codemode spill path unavailable: {error}"))
            })
            .ok()?
            .join("codemode")
            .join("spill")
            .join(format!("{}-{}.txt", clean(session_id), clean(call_id))),
    )
}

fn render_report_with_body(out: &jcode_codemode::RunOutput) -> (String, usize) {
    let mut report = String::new();
    if !out.logs.is_empty() || out.logs_truncated {
        report.push_str("console:\n");
        for line in &out.logs {
            report.push_str(line);
            report.push('\n');
        }
        if out.logs_truncated {
            report.push_str(&format!(
                "...(console output exceeded the {} byte per-run limit; log less or return/store smaller results in a new run)\n",
                Limits::default().max_log_bytes
            ));
        }
        report.push('\n');
    }
    let mut body_end = report.len();
    match (&out.error, &out.result) {
        (Some(err), _) => {
            report.push_str("error: ");
            report.push_str(err);
            report.push('\n');
        }
        (None, Some(value)) => {
            report.push_str("result:\n");
            let rendered = match value {
                Value::String(s) => s.clone(),
                other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
            };
            report.push_str(&rendered);
            report.push('\n');
        }
        (None, None) => report.push_str("result: undefined\n"),
    }
    if out.error.is_none() {
        body_end = report.len();
    }
    if !out.receipts.is_empty() {
        report.push_str("\ncalls:\n");
        const SHOWN: usize = 40;
        for r in out.receipts.iter().take(SHOWN) {
            report.push_str(&format!(
                "  #{} {} {} {}ms {}B",
                r.seq,
                r.tool,
                r.status.as_str(),
                r.elapsed_ms,
                r.bytes
            ));
            if let Some(note) = &r.note {
                report.push_str(" - ");
                report.push_str(&truncate_chars(note, 160));
            }
            report.push('\n');
        }
        if out.receipts.len() > SHOWN {
            let mut counts = std::collections::BTreeMap::<&str, usize>::new();
            for r in &out.receipts[SHOWN..] {
                *counts.entry(r.status.as_str()).or_default() += 1;
            }
            let summary: Vec<String> = counts.iter().map(|(k, v)| format!("{v} {k}")).collect();
            report.push_str(&format!(
                "  ...{} more ({})\n",
                out.receipts.len() - SHOWN,
                summary.join(", ")
            ));
        }
    }
    let mut footer = call_count_footer(out);
    footer.pop(); // Append the store keys before the closing bracket.
    if !out.store.is_empty() {
        let keys: Vec<&str> = out.store.keys().map(String::as_str).take(20).collect();
        footer.push_str(&format!(", stored: {}", keys.join(", ")));
    }
    footer.push(']');
    report.push_str(&footer);
    (report, body_end)
}

fn call_count_footer(out: &jcode_codemode::RunOutput) -> String {
    format!(
        "\n[{} tool call{}, {:.1}s]",
        out.tool_calls,
        if out.tool_calls == 1 { "" } else { "s" },
        out.elapsed.as_secs_f64()
    )
}

fn render_report(out: &jcode_codemode::RunOutput) -> String {
    render_report_with_body(out).0
}

#[async_trait]
impl Tool for CodemodeTool {
    fn name(&self) -> &str {
        "codemode"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["code"],
            "properties": {
                "intent": super::intent_schema_property(),
                "code": {
                    "type": "string",
                    "description": "Body of an async JavaScript function. Use await and return."
                },
                "max_output_tokens": {
                    "type": "integer",
                    "minimum": MIN_OUTPUT_TOKENS,
                    "maximum": MAX_OUTPUT_TOKENS,
                    "description": "Optional approximate report budget. Use 4000 for a compact preview. Save full script data with store(key, value), then page it with load(key). Intermediate tool output is unaffected."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("codemode registry is no longer available"))?;
        let params: CodemodeInput = serde_json::from_value(input)?;
        if params.code.trim().is_empty() {
            anyhow::bail!("code is empty");
        }
        if let Some(max_tokens) = params.max_output_tokens
            && !(MIN_OUTPUT_TOKENS..=MAX_OUTPUT_TOKENS).contains(&max_tokens)
        {
            anyhow::bail!(
                "max_output_tokens must be between {MIN_OUTPUT_TOKENS} and {MAX_OUTPUT_TOKENS}"
            );
        }

        let tools: Vec<ToolInfo> = registry
            .definitions_for_session(&ctx.session_id)
            .await
            .into_iter()
            .filter(|def| !EXCLUDED.contains(&def.name.as_str()))
            .map(|def| ToolInfo {
                name: def.name,
                description: def.description,
                input_schema: def.input_schema,
            })
            .collect();

        let host = Arc::new(RegistryHost {
            registry,
            ctx: ctx.clone(),
            counter: AtomicUsize::new(0),
        });
        let store = load_store(&ctx.session_id)?;
        let limits = Limits {
            timeout: Duration::from_secs(300),
            ..Limits::default()
        };
        let out = jcode_codemode::run(params.code, host, tools, store, limits).await;
        let store_result = save_store(&ctx.session_id, &out.store);

        crate::logging::info(&format!(
            "[tool:codemode] session={} calls={} elapsed_ms={} error={}",
            ctx.session_id,
            out.tool_calls,
            out.elapsed.as_millis(),
            out.error.as_deref().unwrap_or("none")
        ));

        let spill = spill_path(&ctx.session_id, &ctx.tool_call_id);
        let report = match params.max_output_tokens {
            Some(tokens) => budget_report(&out, tokens, spill, store_result.is_ok()),
            None => bound_report(render_report(&out), MAX_REPORT_CHARS, spill),
        };
        store_result.with_context(|| {
            format!("codemode executed but its store was NOT persisted:\n{report}")
        })?;
        if out.error.is_some() && out.tool_calls == 0 && out.logs.is_empty() {
            // Nothing happened: surface as a tool error so the model retries.
            anyhow::bail!("{report}");
        }
        Ok(ToolOutput::new(report).with_title(format!(
            "codemode: {} call{}",
            out.tool_calls,
            if out.tool_calls == 1 { "" } else { "s" }
        )))
    }
}

#[cfg(test)]
#[path = "codemode_tests.rs"]
mod codemode_tests;
