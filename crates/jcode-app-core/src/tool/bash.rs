use super::{BACKGROUND_WAIT_GUIDANCE, Tool, ToolContext, ToolOutput};
#[cfg(windows)]
use crate::background::TaskResult;
use crate::bus::{
    BackgroundTaskProgress, BackgroundTaskProgressKind, BackgroundTaskProgressSource,
};
use crate::util::truncate_str;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(unix)]
use std::fs::OpenOptions;
use std::path::Path;
#[cfg(unix)]
use std::process::Command as StdCommand;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;
#[cfg(unix)]
use tokio::io::AsyncReadExt;
#[cfg(windows)]
use tokio::io::AsyncWriteExt;
#[cfg(any(windows, test))]
use tokio::io::{AsyncBufReadExt, BufReader};
#[cfg(any(windows, test))]
use tokio::process::Command as TokioCommand;

const MAX_OUTPUT_LEN: usize = 30000;
const DEFAULT_TIMEOUT_MS: u64 = 120000;
const PROGRESS_MARKER_PREFIX: &str = "JCODE_PROGRESS ";
const CHECKPOINT_MARKER_PREFIX: &str = "JCODE_CHECKPOINT ";
const BASH_TOOL_DESCRIPTION: &str =
    "Run bash. Prefer agentgrep for code search and read for file views.";
const WINDOWS_SHELL_TOOL_DESCRIPTION: &str =
    "Run a Windows cmd.exe command (compatibility name `bash`). Use cmd.exe syntax, not Bash.";

#[cfg(unix)]
tokio::task_local! {
    pub(crate) static BASH_HANDOFF: jcode_agent_runtime::InterruptSignal;
    pub(crate) static BASH_SUPERVISED: ();
}

#[cfg(unix)]
struct BashCancellation {
    signal: jcode_agent_runtime::InterruptSignal,
    stops_work: Box<dyn Fn() -> bool + Send + Sync>,
}

#[cfg(unix)]
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Route ordinary `cargo` invocations (including those inside child scripts)
/// through the repository wrapper. Besides applying the project's build policy,
/// that wrapper appends real action timings to rust-actions.jsonl.
#[cfg(unix)]
fn wrap_repo_cargo_commands(command: &str, working_dir: Option<&Path>) -> Option<String> {
    let working_dir = working_dir?;
    let repo = crate::build::find_repo_in_ancestors(working_dir)?;
    let wrapper = repo.join("scripts").join("dev_cargo.sh");
    if !wrapper.is_file() {
        return None;
    }

    Some(format!(
        r#"export JCODE_DEV_CARGO_SCRIPT={wrapper}
cargo() {{
  if [[ "${{JCODE_IN_DEV_CARGO:-0}}" == "1" ]]; then
    command cargo "$@"
  else
    JCODE_IN_DEV_CARGO=1 "$JCODE_DEV_CARGO_SCRIPT" "$@"
  fi
}}
export -f cargo
{command}"#,
        wrapper = shell_single_quote(&wrapper.to_string_lossy()),
    ))
}

/// Build a clear timeout message. The `timeout` param is in milliseconds, which
/// agents frequently mistake for seconds (e.g. passing 1000 thinking it means
/// 1000s when it is 1s). Spell out the seconds equivalent and, for suspiciously
/// short timeouts, hint that the unit is milliseconds so the next attempt uses a
/// sane value instead of repeating the same mistake.
fn timeout_message(timeout_ms: u64) -> String {
    let secs = timeout_ms as f64 / 1000.0;
    let mut msg = format!("Command timed out after {}ms ({:.1}s)", timeout_ms, secs);
    if timeout_ms <= 5000 {
        msg.push_str(
            ". Note: the `timeout` parameter is in MILLISECONDS, not seconds. \
             If you meant a longer limit, pass a larger value (e.g. 600000 = 10min) or omit `timeout`.",
        );
    }
    msg
}

/// Timeout applied to a background command.
///
/// An explicit timeout is honored as given. The 10 minute cap only bounds how
/// long a foreground call may hold the turn; a background task has no such
/// limit (and none at all when `timeout` is omitted), so clamping a requested
/// 30 minutes down to 10 killed long test runs early with a misleading
/// "timed out after 600000ms".
fn background_timeout_ms(requested: Option<u64>) -> Option<u64> {
    requested
}

fn progress_ratio_regex() -> Result<&'static regex::Regex> {
    static REGEX: LazyLock<Result<regex::Regex, regex::Error>> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)\b(?P<current>\d{1,6})\s*/\s*(?P<total>\d{1,6})\b(?:\s*(?P<unit>tests?|steps?|files?|items?|cases?|tasks?|targets?|chunks?|batches?|examples?|crates?|modules?|packages?|workers?))?",
        )
    });
    REGEX
        .as_ref()
        .map_err(|err| anyhow::anyhow!("invalid progress ratio regex: {err}"))
}

fn progress_of_regex() -> Result<&'static regex::Regex> {
    static REGEX: LazyLock<Result<regex::Regex, regex::Error>> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)\b(?P<current>\d{1,6})\s+of\s+(?P<total>\d{1,6})\b(?:\s+(?P<unit>tests?|steps?|files?|items?|cases?|tasks?|targets?|chunks?|batches?|examples?|crates?|modules?|packages?|workers?))?",
        )
    });
    REGEX
        .as_ref()
        .map_err(|err| anyhow::anyhow!("invalid progress-of regex: {err}"))
}

fn progress_byte_ratio_regex() -> Result<&'static regex::Regex> {
    static REGEX: LazyLock<Result<regex::Regex, regex::Error>> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)\b(?P<current>\d+(?:\.\d+)?)\s*/\s*(?P<total>\d+(?:\.\d+)?)\s*(?P<unit>bytes?|[kmgt]i?b)\b",
        )
    });
    REGEX
        .as_ref()
        .map_err(|err| anyhow::anyhow!("invalid progress byte-ratio regex: {err}"))
}

fn progress_percent_regex() -> Result<&'static regex::Regex> {
    static REGEX: LazyLock<Result<regex::Regex, regex::Error>> =
        LazyLock::new(|| regex::Regex::new(r"(?i)\b(?P<percent>100|[1-9]?\d)\s*%"));
    REGEX
        .as_ref()
        .map_err(|err| anyhow::anyhow!("invalid progress percent regex: {err}"))
}

#[derive(Deserialize)]
struct ProgressMarker {
    #[serde(default)]
    percent: Option<f32>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    current: Option<u64>,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    eta_seconds: Option<u64>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    checkpoint: Option<bool>,
}

#[cfg(windows)]
fn task_id_from_output_path(path: &Path) -> Option<&str> {
    path.file_stem()?.to_str()
}

fn parse_progress_kind(kind: Option<&str>) -> BackgroundTaskProgressKind {
    match kind {
        Some("indeterminate") => BackgroundTaskProgressKind::Indeterminate,
        _ => BackgroundTaskProgressKind::Determinate,
    }
}

fn summarize_background_command(description: Option<&str>, command: &str) -> String {
    if let Some(description) = description
        .map(str::trim)
        .filter(|description| !description.is_empty())
    {
        let title = description.split_whitespace().collect::<Vec<_>>().join(" ");
        const MAX_TITLE_BYTES: usize = 240;
        return if title.len() > MAX_TITLE_BYTES {
            format!("{}…", truncate_str(&title, MAX_TITLE_BYTES - "…".len()))
        } else {
            title
        };
    }

    let trimmed = command.trim();
    if trimmed.is_empty() {
        return "bash".to_string();
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let start = tokens
        .iter()
        .position(|token| !token.contains('='))
        .unwrap_or(0);
    let tokens = &tokens[start..];
    if tokens.is_empty() {
        return truncate_str(trimmed, 28).to_string();
    }

    let label = match tokens {
        ["python" | "python3" | "bash" | "sh" | "node", script, ..] => std::path::Path::new(script)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(script)
            .to_string(),
        ["cargo", subcommand, ..] => format!("cargo {}", subcommand),
        ["npm" | "pnpm" | "yarn", command, script, ..] if *command == "run" => {
            format!("{} {} {}", tokens[0], command, script)
        }
        [first, second, ..] => format!("{} {}", first, second),
        [first] => first.to_string(),
        [] => "bash".to_string(),
    };

    truncate_str(&label, 28).to_string()
}

fn parse_progress_marker_with_checkpoint(line: &str) -> Option<(BackgroundTaskProgress, bool)> {
    let payload = line.trim().strip_prefix(PROGRESS_MARKER_PREFIX)?.trim();
    let marker: ProgressMarker = serde_json::from_str(payload).ok()?;
    let is_checkpoint =
        marker.checkpoint.unwrap_or(false) || matches!(marker.kind.as_deref(), Some("checkpoint"));
    let kind = if marker.percent.is_some()
        || matches!((marker.current, marker.total), (_, Some(total)) if total > 0)
    {
        BackgroundTaskProgressKind::Determinate
    } else {
        parse_progress_kind(marker.kind.as_deref())
    };

    Some((
        BackgroundTaskProgress {
            kind,
            percent: marker.percent,
            message: marker.message,
            current: marker.current,
            total: marker.total,
            unit: marker.unit,
            eta_seconds: marker.eta_seconds,
            updated_at: Utc::now().to_rfc3339(),
            source: BackgroundTaskProgressSource::Reported,
        }
        .normalize(),
        is_checkpoint,
    ))
}

#[cfg(all(test, unix))]
fn parse_progress_marker(line: &str) -> Option<BackgroundTaskProgress> {
    parse_progress_marker_with_checkpoint(line).map(|(progress, _)| progress)
}

fn parse_checkpoint_marker(line: &str) -> Option<BackgroundTaskProgress> {
    let payload = line.trim().strip_prefix(CHECKPOINT_MARKER_PREFIX)?.trim();
    let marker: ProgressMarker = serde_json::from_str(payload).unwrap_or_else(|_| ProgressMarker {
        percent: None,
        message: Some(payload.to_string()),
        current: None,
        total: None,
        unit: None,
        eta_seconds: None,
        kind: Some("checkpoint".to_string()),
        checkpoint: Some(true),
    });

    Some(
        BackgroundTaskProgress {
            kind: BackgroundTaskProgressKind::Indeterminate,
            percent: marker.percent,
            message: marker.message,
            current: marker.current,
            total: marker.total,
            unit: marker.unit,
            eta_seconds: marker.eta_seconds,
            updated_at: Utc::now().to_rfc3339(),
            source: BackgroundTaskProgressSource::Reported,
        }
        .normalize(),
    )
}

fn progress_message_from_line(line: &str, matched_fragment: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(matched_fragment.trim()) {
        None
    } else {
        Some(trimmed.split_whitespace().collect::<Vec<_>>().join(" "))
    }
}

fn progress_from_counts(
    trimmed: &str,
    matched: &str,
    current: u64,
    total: u64,
    unit: Option<String>,
) -> Option<BackgroundTaskProgress> {
    if total < 2 || current > total {
        return None;
    }

    Some(
        BackgroundTaskProgress {
            kind: BackgroundTaskProgressKind::Determinate,
            percent: None,
            message: progress_message_from_line(trimmed, matched),
            current: Some(current),
            total: Some(total),
            unit,
            eta_seconds: None,
            updated_at: Utc::now().to_rfc3339(),
            source: BackgroundTaskProgressSource::ParsedOutput,
        }
        .normalize(),
    )
}

pub(super) fn parse_heuristic_progress(line: &str) -> Result<Option<BackgroundTaskProgress>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let ratio = progress_ratio_regex()?.captures(trimmed);
    // A leading [N/M] can number static output (for example a dry-run plan).
    // It is not a progress signal, even if the row describes a percent or ratio.
    if let Some(counter) = ratio.as_ref().and_then(|captures| captures.get(0))
        && trimmed[..counter.start()].trim_end() == "["
        && trimmed[counter.end()..].trim_start().starts_with(']')
    {
        return Ok(None);
    }

    if let Some(captures) = progress_byte_ratio_regex()?.captures(trimmed) {
        let current = captures
            .name("current")
            .and_then(|m| m.as_str().parse::<f64>().ok());
        let total = captures
            .name("total")
            .and_then(|m| m.as_str().parse::<f64>().ok());
        if let (Some(current), Some(total), Some(matched)) = (current, total, captures.get(0))
            && total > 0.0
            && current <= total
        {
            return Ok(Some(
                BackgroundTaskProgress {
                    kind: BackgroundTaskProgressKind::Determinate,
                    percent: Some(((current / total) * 100.0) as f32),
                    message: progress_message_from_line(trimmed, matched.as_str()),
                    current: None,
                    total: None,
                    unit: captures
                        .name("unit")
                        .map(|unit| unit.as_str().to_ascii_lowercase()),
                    eta_seconds: None,
                    updated_at: Utc::now().to_rfc3339(),
                    source: BackgroundTaskProgressSource::ParsedOutput,
                }
                .normalize(),
            ));
        }
    }

    if let Some(captures) = ratio {
        let current = captures
            .name("current")
            .and_then(|m| m.as_str().parse::<u64>().ok());
        let total = captures
            .name("total")
            .and_then(|m| m.as_str().parse::<u64>().ok());
        if let (Some(current), Some(total), Some(matched)) = (current, total, captures.get(0)) {
            return Ok(progress_from_counts(
                trimmed,
                matched.as_str(),
                current,
                total,
                captures
                    .name("unit")
                    .map(|unit| unit.as_str().to_ascii_lowercase()),
            ));
        }
    }

    if let Some(captures) = progress_of_regex()?.captures(trimmed) {
        let current = captures
            .name("current")
            .and_then(|m| m.as_str().parse::<u64>().ok());
        let total = captures
            .name("total")
            .and_then(|m| m.as_str().parse::<u64>().ok());
        if let (Some(current), Some(total), Some(matched)) = (current, total, captures.get(0)) {
            return Ok(progress_from_counts(
                trimmed,
                matched.as_str(),
                current,
                total,
                captures
                    .name("unit")
                    .map(|unit| unit.as_str().to_ascii_lowercase()),
            ));
        }
    }

    if let Some(captures) = progress_percent_regex()?.captures(trimmed)
        && let (Some(percent), Some(matched)) = (
            captures
                .name("percent")
                .and_then(|m| m.as_str().parse::<f32>().ok()),
            captures.get(0),
        )
    {
        return Ok(Some(
            BackgroundTaskProgress {
                kind: BackgroundTaskProgressKind::Determinate,
                percent: Some(percent),
                message: progress_message_from_line(trimmed, matched.as_str()),
                current: None,
                total: None,
                unit: None,
                eta_seconds: None,
                updated_at: Utc::now().to_rfc3339(),
                source: BackgroundTaskProgressSource::ParsedOutput,
            }
            .normalize(),
        ));
    }

    const PHASE_PREFIXES: &[&str] = &[
        "Compiling ",
        "Downloading ",
        "Running ",
        "Building ",
        "Linking ",
        "Resolving ",
        "Fetching ",
        "Installing ",
    ];
    if PHASE_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return Ok(Some(
            BackgroundTaskProgress {
                kind: BackgroundTaskProgressKind::Indeterminate,
                percent: None,
                message: progress_message_from_line(trimmed, ""),
                current: None,
                total: None,
                unit: None,
                eta_seconds: None,
                updated_at: Utc::now().to_rfc3339(),
                source: BackgroundTaskProgressSource::ParsedOutput,
            }
            .normalize(),
        ));
    }

    Ok(None)
}

#[cfg(windows)]
async fn handle_background_output_line(
    output_path: &Path,
    file: &mut tokio::fs::File,
    raw_line: &str,
    stderr: bool,
) {
    match parse_progress_line(raw_line) {
        Ok(Some(update)) => {
            if let Some(task_id) = task_id_from_output_path(output_path) {
                apply_progress_update(crate::background::global(), task_id, update).await;
            }
            return;
        }
        Ok(None) => {}
        Err(err) => {
            let warning = format!("[jcode warning] failed to parse background progress: {err}\n");
            file.write_all(warning.as_bytes()).await.ok();
            file.flush().await.ok();
        }
    }

    let rendered = if stderr {
        format!("[stderr] {}\n", raw_line)
    } else {
        format!("{}\n", raw_line)
    };
    file.write_all(rendered.as_bytes()).await.ok();
    file.flush().await.ok();
}

/// A progress or checkpoint update parsed from one line of command output.
#[derive(Debug)]
pub(super) enum ProgressLineUpdate {
    Progress(BackgroundTaskProgress),
    Checkpoint(BackgroundTaskProgress),
}

/// Parse one output line for any supported progress signal: explicit
/// `JCODE_CHECKPOINT`/`JCODE_PROGRESS` markers first, then heuristic patterns
/// (ratios, percentages, byte ratios, phase prefixes).
pub(super) fn parse_progress_line(line: &str) -> Result<Option<ProgressLineUpdate>> {
    if let Some(progress) = parse_checkpoint_marker(line) {
        return Ok(Some(ProgressLineUpdate::Checkpoint(progress)));
    }

    if let Some((progress, is_checkpoint)) = parse_progress_marker_with_checkpoint(line) {
        return Ok(Some(if is_checkpoint {
            ProgressLineUpdate::Checkpoint(progress)
        } else {
            ProgressLineUpdate::Progress(progress)
        }));
    }

    Ok(parse_heuristic_progress(line)?.map(ProgressLineUpdate::Progress))
}

async fn apply_progress_update(
    manager: &crate::background::BackgroundTaskManager,
    task_id: &str,
    update: ProgressLineUpdate,
) {
    let _ = match update {
        ProgressLineUpdate::Progress(progress) => manager.update_progress(task_id, progress).await,
        ProgressLineUpdate::Checkpoint(progress) => {
            manager.update_checkpoint(task_id, progress).await
        }
    };
}

/// Progress state for a foreground command that may be promoted to a
/// background task if it exceeds the foreground timeout.
///
/// Before promotion there is no task to attach progress to, so only the most
/// recent update is kept. When the command is promoted, `attach_task` flushes
/// that pending update so the task row starts at the real percentage instead
/// of 0%, and later updates stream directly to the background manager.
#[cfg(windows)]
#[derive(Default)]
struct PromotedCommandProgress {
    task_id: std::sync::OnceLock<String>,
    pending: std::sync::Mutex<Option<ProgressLineUpdate>>,
}

#[cfg(windows)]
impl PromotedCommandProgress {
    async fn record(&self, update: ProgressLineUpdate) {
        let direct = {
            let mut pending = self.pending.lock().expect("progress mutex poisoned");
            if self.task_id.get().is_none() {
                *pending = Some(update);
                None
            } else {
                Some(update)
            }
        };
        if let Some(update) = direct
            && let Some(task_id) = self.task_id.get()
        {
            apply_progress_update(crate::background::global(), task_id, update).await;
        }
    }

    async fn attach_task(&self, task_id: &str) {
        let _ = self.task_id.set(task_id.to_string());
        let pending = self.pending.lock().expect("progress mutex poisoned").take();
        if let Some(update) = pending {
            apply_progress_update(crate::background::global(), task_id, update).await;
        }
    }
}

/// Collect a command's output stream line by line, reporting any parsed
/// progress so a later background promotion has live progress instead of
/// sitting at 0% until completion.
#[cfg(windows)]
async fn collect_output_reporting_progress<R>(
    reader: Option<R>,
    progress: std::sync::Arc<PromotedCommandProgress>,
) -> String
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = String::new();
    let Some(reader) = reader else {
        return buf;
    };
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Ok(Some(update)) = parse_progress_line(&line) {
            progress.record(update).await;
        }
        buf.push_str(&line);
        buf.push('\n');
    }
    buf
}

/// Tail a detached background task's output file and translate progress lines
/// into background-manager progress updates.
///
/// Detached commands write directly to their output file, so nothing in-process
/// sees their output as it streams. This follower polls the file while the task
/// is `Running`, parsing complete lines from where it left off. It performs one
/// final drain after the task leaves `Running` and then exits.
#[cfg(unix)]
fn spawn_detached_progress_follower(
    manager: crate::background::BackgroundTaskManager,
    task_id: String,
    output_file: std::path::PathBuf,
    mut cancellation: Option<BashCancellation>,
) {
    tokio::spawn(async move {
        let mut pos: u64 = 0;
        let mut partial: Vec<u8> = Vec::new();
        loop {
            let running = manager
                .status(&task_id)
                .await
                .map(|status| status.status == crate::bus::BackgroundTaskStatus::Running)
                .unwrap_or(false);

            if let Ok(mut file) = tokio::fs::File::open(&output_file).await {
                use tokio::io::AsyncSeekExt;
                if file.seek(std::io::SeekFrom::Start(pos)).await.is_ok() {
                    let mut chunk = Vec::new();
                    if file.read_to_end(&mut chunk).await.is_ok() && !chunk.is_empty() {
                        pos += chunk.len() as u64;
                        partial.extend_from_slice(&chunk);
                        while let Some(newline) = partial.iter().position(|byte| *byte == b'\n') {
                            let line: Vec<u8> = partial.drain(..=newline).collect();
                            let line = String::from_utf8_lossy(&line);
                            if let Ok(Some(update)) = parse_progress_line(line.trim_end()) {
                                apply_progress_update(&manager, &task_id, update).await;
                            }
                        }
                    }
                }
            }

            if !running {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                _ = async {
                    if let Some(cancel) = &cancellation {
                        cancel.signal.notified().await;
                    }
                }, if cancellation.is_some() => {
                    if cancellation.take().is_some_and(|cancel| (cancel.stops_work)()) {
                        let _ = manager.cancel(&task_id).await;
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(unix)]
pub(crate) async fn restore_detached_task_monitoring() {
    let manager = crate::background::global();
    for task in manager.detached_tasks_for_monitoring().await {
        // The output directory is shared across local daemon sockets. Only
        // this home's saved sessions can have lost their watcher on this exec.
        if !crate::session::session_exists(&task.session_id) {
            continue;
        }
        let output = manager.output_path_for(&task.task_id);
        if let Some(seconds) = task.stall_wake_seconds {
            manager.arm_stall_watchdog(&task.task_id, seconds).await;
        }
        spawn_detached_progress_follower(manager.clone(), task.task_id, output, None);
    }
}

#[cfg(unix)]
pub(crate) fn follow_detached_task(info: &crate::background::BackgroundTaskInfo) {
    spawn_detached_progress_follower(
        crate::background::global().clone(),
        info.task_id.clone(),
        info.output_file.clone(),
        None,
    );
}

#[cfg(not(windows))]
fn tool_scratch_dir() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("JCODE_SCRATCH_DIR")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            crate::storage::jcode_dir()
                .ok()
                .map(|dir| dir.join("scratch"))
        })?;
    crate::storage::ensure_dir(&dir).ok()?;
    Some(dir)
}

/// Every bash command is non-interactive: stdin is /dev/null (callers that
/// pipe stdin drop it immediately), common tools are told not to prompt, and
/// on unix the command runs in its own session with no controlling terminal so
/// prompts that open /dev/tty directly (sudo, ssh, git credentials) fail fast
/// instead of hanging. The agent sees the failure and reruns non-interactively.
const NON_INTERACTIVE_ENV: &[(&str, &str)] = &[
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_ASKPASS", "true"),
    ("SSH_ASKPASS_REQUIRE", "never"),
    ("GCM_INTERACTIVE", "never"),
    ("DEBIAN_FRONTEND", "noninteractive"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("SYSTEMD_PAGER", ""),
    ("NPM_CONFIG_YES", "true"),
    ("PIP_NO_INPUT", "1"),
];

#[cfg(all(not(windows), test))]
fn configure_non_interactive(command: &mut TokioCommand) {
    command.stdin(Stdio::null());
    command.envs(NON_INTERACTIVE_ENV.iter().copied());
    command.env_remove("SSH_ASKPASS");
    unsafe {
        command.pre_exec(|| {
            // New session: no controlling tty, own process group (so group
            // kills still work). Ignore failure if already a group leader.
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(all(not(windows), test))]
fn configure_tool_scratch(command: &mut TokioCommand) {
    if let Some(dir) = tool_scratch_dir() {
        command.env("TMPDIR", &dir).env("JCODE_SCRATCH_DIR", dir);
    }
}

#[cfg(unix)]
struct ProcessGroupKillGuard {
    pid: Option<u32>,
}

#[cfg(unix)]
impl ProcessGroupKillGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }

    fn disarm(&mut self) {
        self.pid = None;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupKillGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            let _ = crate::platform::signal_detached_process_group(pid, libc::SIGKILL);
        }
    }
}

#[cfg(any(windows, test))]
fn build_shell_command(cmd_str: &str) -> TokioCommand {
    #[cfg(windows)]
    {
        let mut cmd = TokioCommand::new("cmd.exe");
        // cmd.exe does not use the standard C runtime argument-decoding rules.
        // Passing the command through `arg` makes Rust escape nested quotes for
        // CommandLineToArgvW, which can corrupt commands such as:
        //
        //     gh issue create --title "text with spaces"
        //
        // Tokio's `raw_arg` is specifically provided for `cmd.exe /C`. Wrap the
        // full command in the outer quotes expected by cmd so its inner quotes
        // reach child programs intact. `/D` disables AutoRun hooks and `/S`
        // selects the documented quote handling used with this form.
        cmd.args(["/D", "/S", "/C"])
            .raw_arg(format!("\"{cmd_str}\""));
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = TokioCommand::new("bash");
        cmd.arg("-c").arg(cmd_str);
        configure_tool_scratch(&mut cmd);
        configure_non_interactive(&mut cmd);
        cmd
    }
}

#[cfg(any(windows, test))]
fn configure_background_command_stdio(command: &mut TokioCommand) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

/// Keep cloud environment values out of tool arguments and transcript output.
/// Read on each spawn so a just-completed opaque transfer is available to the
/// next command without changing the long-lived daemon's global environment.
fn cloud_command_environment(
    working_dir: Option<&Path>,
) -> Result<std::collections::BTreeMap<String, String>> {
    let root = match working_dir {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()?,
    };
    crate::cloud_environment::load_environment(&root)
        .map_err(|_| anyhow::anyhow!("could not load saved cloud environment"))
}

#[cfg(unix)]
fn build_detached_shell_wrapper(command: &str, receipt: &Path, timeout: Option<u64>) -> StdCommand {
    let mut cmd = StdCommand::new("bash");
    cmd.arg("-c")
        .arg(include_str!("bash_supervisor.sh"))
        .arg("jcode-bash")
        .arg(command)
        .arg(receipt)
        .arg(
            timeout
                .map(|ms| format!("{}.{:03}", ms / 1000, ms % 1000))
                .unwrap_or_default(),
        )
        .arg(timeout.map(timeout_message).unwrap_or_default())
        .envs(NON_INTERACTIVE_ENV.iter().copied())
        .env_remove("SSH_ASKPASS");
    if let Some(dir) = tool_scratch_dir() {
        cmd.env("TMPDIR", &dir).env("JCODE_SCRATCH_DIR", dir);
    }
    cmd
}

#[cfg(unix)]
fn launch_detached_bash(
    params: &BashInput,
    ctx: &ToolContext,
    info: &crate::background::BackgroundTaskInfo,
    session_environment: std::collections::HashMap<String, String>,
    timeout: Option<u64>,
) -> Result<std::process::Child> {
    let mut cmd = build_detached_shell_wrapper(
        &params.command,
        &info.output_file.with_extension("exit"),
        timeout,
    );
    cmd.envs(cloud_command_environment(ctx.working_dir.as_deref())?);
    cmd.envs(session_environment);
    cmd.env("JCODE_SESSION_ID", &ctx.session_id);
    let stdout = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&info.output_file)?;
    let stderr = stdout.try_clone()?;
    cmd.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
    if let Some(ref dir) = ctx.working_dir {
        cmd.current_dir(dir);
    }
    match crate::platform::spawn_detached(&mut cmd) {
        Ok(child) => Ok(child),
        Err(error) => {
            let _ = std::fs::remove_file(&info.output_file);
            Err(error.into())
        }
    }
}

fn format_command_output(mut output: String, exit_code: Option<i32>) -> String {
    if output.len() > MAX_OUTPUT_LEN {
        output = truncate_str(&output, MAX_OUTPUT_LEN).to_string();
        output.push_str("\n... (output truncated)");
    }

    if let Some(code) = exit_code.filter(|code| *code != 0) {
        output.push_str(&format!("\n\nExit code: {}", code));
    }

    if output.trim().is_empty() {
        "Command completed successfully (no output)".to_string()
    } else {
        output
    }
}

#[cfg(unix)]
async fn read_foreground_bash_output(info: &crate::background::BackgroundTaskInfo) -> String {
    let output = tokio::fs::read(&info.output_file).await.unwrap_or_default();
    let _ = tokio::fs::remove_file(&info.output_file).await;
    let _ = tokio::fs::remove_file(info.output_file.with_extension("exit")).await;
    String::from_utf8_lossy(&output).into_owned()
}

#[cfg(test)]
#[path = "bash_utf8_truncation_tests.rs"]
mod utf8_truncation_tests;

pub struct BashTool {
    #[cfg(test)]
    manager: Option<std::sync::Arc<crate::background::BackgroundTaskManager>>,
}

impl BashTool {
    pub fn new() -> Self {
        Self {
            #[cfg(test)]
            manager: None,
        }
    }

    fn manager(&self) -> &crate::background::BackgroundTaskManager {
        #[cfg(test)]
        if let Some(manager) = self.manager.as_deref() {
            return manager;
        }
        crate::background::global()
    }
}

#[derive(Deserialize)]
struct BashInput {
    command: String,
    #[serde(default)]
    intent: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    run_in_background: Option<bool>,
    #[serde(
        default = "default_true",
        deserialize_with = "deserialize_bool_or_default::<_, true>"
    )]
    notify: bool,
    #[serde(default, deserialize_with = "deserialize_bool_or_default::<_, false>")]
    wake: bool,
    /// For background runs: wake the agent after this many seconds with no
    /// new output and no progress events. Resets on activity.
    #[serde(default)]
    stall_wake_seconds: Option<u64>,
    /// Set only when re-issuing a call the gate refused (#604).
    #[serde(default)]
    justification: Option<String>,
}

fn default_true() -> bool {
    true
}

// OpenAI strict schemas represent omitted optional arguments as explicit null.
// Serde's `default` only handles missing keys, so accept null separately while
// retaining the same defaults and rejecting non-boolean values.
fn deserialize_bool_or_default<'de, D, const DEFAULT: bool>(
    deserializer: D,
) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<bool>::deserialize(deserializer)?.unwrap_or(DEFAULT))
}

#[path = "bash_destructive_gate.rs"]
mod destructive_gate;
use destructive_gate::destructive_command_refusal;
#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        if cfg!(windows) {
            WINDOWS_SHELL_TOOL_DESCRIPTION
        } else {
            BASH_TOOL_DESCRIPTION
        }
    }

    fn parameters_schema(&self) -> Value {
        destructive_gate::bash_parameters_schema()
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let mut params: BashInput = serde_json::from_value(input)?;
        let run_in_background = params.run_in_background.unwrap_or(false);
        let read_hint = if cfg!(unix) && !run_in_background {
            search_read_hint(&ctx.session_id, &params.command)
        } else {
            None
        };

        // Destructive-command gate (#604), before background dispatch.
        if let Some(refusal) = destructive_command_refusal(
            &params.command,
            params.justification.as_deref(),
            ctx.working_dir.clone(),
        ) {
            return Err(anyhow::anyhow!(refusal));
        }

        #[cfg(unix)]
        if let Some(wrapped) = wrap_repo_cargo_commands(&params.command, ctx.working_dir.as_deref())
        {
            params.command = wrapped;
        }

        if run_in_background {
            return self.execute_background(params, ctx).await;
        }

        // Auto-detect browser bridge commands and rewrite them to the installed
        // binary when available, but do not run setup automatically. Browser
        // setup should stay an explicit status/setup flow rather than a default
        // side effect of trying to use the browser.
        if crate::browser::is_browser_command(&params.command) {
            params.command = crate::browser::rewrite_command_with_full_path(&params.command);

            // Give each agent its own browser session without blocking async workers.
            if !cfg!(windows)
                && std::env::var("BROWSER_SESSION").is_err()
                && let Some(session_name) =
                    crate::browser::ensure_browser_session_for_async(&ctx.session_id, None).await
            {
                params.command = format!("BROWSER_SESSION={} {}", session_name, params.command);
            }
        }

        // Foreground execution with stdin detection
        let hint = file_edit_hint(&params.command);
        let mut output = self.execute_foreground(&params, &ctx).await?;
        if let Some(hint) = hint {
            output.output.push_str("\n\n");
            output.output.push_str(hint);
        }
        if let Some(hint) = read_hint {
            output.output.push_str("\n\n");
            output.output.push_str(hint);
        }
        Ok(output)
    }
}

const READ_HINT_INTERVAL: usize = 10;
type ReadHintCounts = std::collections::HashMap<String, (usize, Option<usize>)>;
static READ_HINT_COUNTS: LazyLock<std::sync::Mutex<ReadHintCounts>> =
    LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

pub(super) fn clear_read_hint_session(session_id: &str) {
    READ_HINT_COUNTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session_id);
}

fn search_read_hint(session_id: &str, command: &str) -> Option<&'static str> {
    let hint = pure_read_hint(command);
    let policy = super::session_tool_policy(session_id);
    let sdk = super::sdk::config(session_id);
    let mut counts = READ_HINT_COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    // Bound memory for direct invocations without Agent lifecycle registration.
    if counts.len() >= 4096 && !counts.contains_key(session_id) {
        return None;
    }
    let state = counts.entry(session_id.to_owned()).or_default();
    state.0 = state.0.saturating_add(1);
    let hint = hint?;
    let tool = if hint.contains("agentgrep") {
        "agentgrep"
    } else {
        "read"
    };
    let sdk_enabled = sdk.as_ref().and_then(|config| config.enabled.as_ref());
    if sdk_enabled.is_some_and(|enabled| !enabled.iter().any(|name| name == tool))
        || sdk
            .as_ref()
            .is_some_and(|config| config.disabled.iter().any(|name| name == tool))
        || (sdk_enabled.is_none()
            && policy.as_ref().is_some_and(|policy| {
                policy
                    .allowed_tools
                    .as_ref()
                    .is_some_and(|allowed| !super::tool_name_is_allowed(allowed, tool))
                    || super::tool_name_is_disabled(&policy.disabled_tools, tool)
            }))
    {
        return None;
    }
    if state
        .1
        .is_some_and(|last| state.0.saturating_sub(last) < READ_HINT_INTERVAL)
    {
        return None;
    }
    state.1 = Some(state.0);
    Some(hint)
}

fn pure_read_hint(command: &str) -> Option<&'static str> {
    // Be conservative: compound commands, redirection, substitutions, and
    // shell expansion are not pure reads, even if their first word looks like one.
    if command
        .chars()
        .any(|c| matches!(c, '|' | '&' | ';' | '>' | '<' | '`' | '$' | '\n' | '\r'))
    {
        return None;
    }
    let words: Vec<_> = command.split_whitespace().collect();
    let (program, args) = words.split_first()?;
    if args.is_empty() {
        return None;
    }
    let search = "Tip: use `agentgrep` for code search (regex, glob, paths_only).";
    let read = "Tip: use `read` with start_line and limit to inspect file contents.";
    match *program {
        "rg" | "grep"
            if !args
                .iter()
                .any(|a| a.starts_with("--pre") || *a == "--search-zip") =>
        {
            Some(search)
        }
        "find"
            if !args.iter().any(|a| {
                matches!(
                    *a,
                    "-exec"
                        | "-execdir"
                        | "-ok"
                        | "-okdir"
                        | "-delete"
                        | "-fprint"
                        | "-fprintf"
                        | "-fls"
                )
            }) =>
        {
            Some(search)
        }
        "cat" => Some(read),
        "sed"
            if args.contains(&"-n")
                && !args.iter().any(|a| {
                    a.starts_with("-i") || a.starts_with("--in-place") || a.contains('w')
                }) =>
        {
            Some(read)
        }
        _ => None,
    }
}

const FILE_EDIT_HINT: &str = "Note: this command edits files in place. Next time use `edit` \
(exact replacements, all-or-nothing, pass `edits` for several) or `apply_patch` \
(multi-file patches). They fail loudly on a missed match \
instead of silently writing nothing, and show a reviewable diff.";

/// Detect shell commands that rewrite source files in place, where a missed
/// match silently does nothing. The command still runs, the agent is nudged.
fn file_edit_hint(command: &str) -> Option<&'static str> {
    let compact: String = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let sed_in_place = compact.split(['|', ';', '&']).any(|segment| {
        let mut words = segment.split_whitespace();
        words.next() == Some("sed")
            && words.any(|word| {
                word == "--in-place"
                    || word.starts_with("--in-place=")
                    || (word.starts_with('-') && !word.starts_with("--") && word[1..].contains('i'))
            })
    });
    let perl_in_place = compact
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|pair| pair[0] == "perl" && pair[1].starts_with('-') && pair[1].contains('i'));
    let script_rewrite = (compact.contains("python") || compact.contains("node "))
        && (compact.contains(".replace(") || compact.contains("re.sub("))
        && (compact.contains(",'w')")
            || compact.contains(",\"w\")")
            || compact.contains(", 'w')")
            || compact.contains(", \"w\")")
            || compact.contains("write_text(")
            || compact.contains("writeFileSync("));
    (sed_in_place || perl_in_place || script_rewrite).then_some(FILE_EDIT_HINT)
}

#[cfg(test)]
#[path = "bash_file_edit_hint_tests.rs"]
mod file_edit_hint_tests;

impl BashTool {
    #[cfg(windows)]
    async fn execute_foreground(
        &self,
        params: &BashInput,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let timeout_ms = params.timeout.unwrap_or(DEFAULT_TIMEOUT_MS).min(600000);
        let timeout_duration = Duration::from_millis(timeout_ms);

        let mut command = build_shell_command(&params.command);
        command.envs(cloud_command_environment(ctx.working_dir.as_deref())?);
        command.envs(crate::session_env::session_env(&ctx.session_id));
        command
            .env("JCODE_SESSION_ID", &ctx.session_id)
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(ref dir) = ctx.working_dir {
            command.current_dir(dir);
        }
        let mut child = command.spawn()?;
        #[cfg_attr(not(unix), allow(unused_variables))]
        let command_pid = child.id();
        // The command runs in its own session, so only a group kill reaches the
        // children of a shell loop. Dropping this future (turn cancel, or force
        // stop aborting the turn) must kill the group too: `kill_on_drop` only
        // reaps the direct child, which the detached work task still owns.
        #[cfg(unix)]
        let mut process_group_guard = ProcessGroupKillGuard::new(command_pid);

        let stdin_handle = child.stdin.take();
        let stdout_handle = child.stdout.take();
        let stderr_handle = child.stderr.take();

        // Owned copies so the work can outlive this call if it is promoted to a
        // background task on timeout.
        let title = params
            .intent
            .clone()
            .unwrap_or_else(|| params.command.clone());
        let title_for_work = title.clone();
        // Track progress parsed from output so a timeout promotion starts the
        // background task at the real percentage instead of 0%.
        let promoted_progress = std::sync::Arc::new(PromotedCommandProgress::default());
        let stdout_progress = std::sync::Arc::clone(&promoted_progress);
        let stderr_progress = std::sync::Arc::clone(&promoted_progress);

        // Run the command (read stdout/stderr, service stdin, wait for exit) in a
        // dedicated task so that, if it exceeds the foreground timeout, we can hand
        // the still-running task off to the background manager instead of killing it.
        //
        // The turn-cancel watch lives inside that task, not in the caller:
        // promotion moves the task to the background manager, and a cancel must
        // still reach the command it was promoted with.
        let mut shutdown_in_work = ctx.graceful_shutdown_signal.clone();
        let work_pid = command_pid;
        let work_session = ctx.session_id.clone();
        let mut work_handle: tokio::task::JoinHandle<Result<ToolOutput>> =
            tokio::spawn(async move {
                let stdout_task = tokio::spawn(collect_output_reporting_progress(
                    stdout_handle,
                    stdout_progress,
                ));

                let stderr_task = tokio::spawn(collect_output_reporting_progress(
                    stderr_handle,
                    stderr_progress,
                ));

                // Commands are always non-interactive: stdin is /dev/null, so
                // anything that reads it gets EOF immediately.
                drop(stdin_handle);

                let shutdown_requested = async {
                    match shutdown_in_work.as_mut() {
                        Some(signal) => signal.notified().await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::pin!(shutdown_requested);
                // Only a group kill reaches a shell loop's children. A reload
                // keeps the command running so its work survives the restart,
                // so the kill only happens for a user cancel.
                let (status, interrupted) = tokio::select! {
                    biased;
                    _ = &mut shutdown_requested => {
                        if crate::turn_cancel_registry::cancellation_stops_running_work(
                            &work_session,
                        ) {
                            #[cfg(unix)]
                            let signal = libc::SIGKILL;
                            #[cfg(not(unix))]
                            let signal = 0;
                            if let Some(pid) = work_pid {
                                let _ =
                                    crate::platform::signal_detached_process_group(pid, signal);
                            }
                            (child.wait().await?, true)
                        } else {
                            (child.wait().await?, false)
                        }
                    }
                    status = child.wait() => (status?, false),
                };

                let stdout = stdout_task.await.unwrap_or_default();
                let stderr = stderr_task.await.unwrap_or_default();

                let mut output = String::new();
                if !stdout.is_empty() {
                    output.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(&stderr);
                }
                if interrupted {
                    let mut text =
                        String::from("Command interrupted by user; its process group was killed.");
                    if !output.trim().is_empty() {
                        text.push_str("\n\n");
                        text.push_str(&output);
                    }
                    return Ok(ToolOutput::new(text)
                        .with_title(title_for_work)
                        .with_metadata(json!({ "interrupted": true })));
                }
                let output = format_command_output(output, status.code());
                Ok(ToolOutput::new(output).with_title(title_for_work))
            });

        match tokio::time::timeout(timeout_duration, &mut work_handle).await {
            Ok(join_result) => {
                #[cfg(unix)]
                process_group_guard.disarm();
                match join_result {
                    Ok(Ok(output)) => Ok(output),
                    Ok(Err(e)) => Err(anyhow::anyhow!("Command failed: {}", e)),
                    Err(join_err) => Err(anyhow::anyhow!("Command task panicked: {}", join_err)),
                }
            }
            Err(_) => {
                // The command keeps running on purpose, so drop the kill guard
                // with the work handle it no longer owns.
                #[cfg(unix)]
                process_group_guard.disarm();
                // Timed out, but the command is still running. Instead of killing
                // it, promote it to a background task so it keeps running, renders
                // as a background-task card, and the agent is told where to find it.
                let display_name =
                    summarize_background_command(params.intent.as_deref(), &params.command);
                let info = self
                    .manager()
                    .adopt_with_options(
                        "bash",
                        Some(display_name.clone()),
                        &ctx.session_id,
                        params.notify,
                        params.wake,
                        work_handle,
                    )
                    .await;
                // Route progress parsed from the still-running command's output
                // to the new background task, including any update seen before
                // promotion, so the task row shows real progress from the start.
                promoted_progress.attach_task(&info.task_id).await;

                let output = format!(
                    "Command exceeded the foreground timeout after {:.1}s and is continuing in background (not killed).\n\n\
                     Task ID: {}\n\
                     Name: {}\n\
                     Output file: {}\n\
                     Status file: {}\n\n\
                     {BACKGROUND_WAIT_GUIDANCE}\n\n\
                     The command is still running; do not rerun it unless you intentionally want a second copy.\n\
                     If you expected it to finish quickly and it did not, the `timeout` parameter is in MILLISECONDS; pass a larger value or omit it.",
                    timeout_ms as f64 / 1000.0,
                    info.task_id,
                    display_name,
                    info.output_file.display(),
                    info.status_file.display(),
                );

                Ok(ToolOutput::new(output)
                    .with_title(title)
                    .with_metadata(json!({
                        "background": true,
                        "task_id": info.task_id,
                        "display_name": display_name,
                        "output_file": info.output_file.to_string_lossy(),
                        "status_file": info.status_file.to_string_lossy(),
                        "timeout_promoted": true,
                        "foreground_timeout_ms": timeout_ms,
                    })))
            }
        }
    }

    #[cfg(unix)]
    async fn execute_foreground(
        &self,
        params: &BashInput,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let timeout_ms = params.timeout.unwrap_or(DEFAULT_TIMEOUT_MS).min(600000);
        let started = Instant::now();
        let started_at = Utc::now().to_rfc3339();
        let manager = self.manager();
        let info = manager.reserve_task_info();
        let supervised = BASH_SUPERVISED.try_with(|_| ()).is_ok();
        let mut child = launch_detached_bash(
            params,
            ctx,
            &info,
            crate::session_env::session_env(&ctx.session_id),
            supervised
                .then(|| background_timeout_ms(params.timeout))
                .flatten(),
        )?;
        let pid = child.id();
        let mut guard = ProcessGroupKillGuard::new(Some(pid));
        let title = params
            .intent
            .clone()
            .unwrap_or_else(|| params.command.clone());
        let shutdown = ctx.graceful_shutdown_signal.as_ref();
        let handoff = BASH_HANDOFF.try_with(Clone::clone).ok();
        let stops_work = crate::turn_cancel_registry::capture_running_work_cancel(&ctx.session_id);
        let (reason, reload, timed_out) = loop {
            if let Some(status) = child.try_wait()? {
                guard.disarm();
                let timeout = supervised
                    && tokio::fs::read_to_string(info.output_file.with_extension("exit"))
                        .await
                        .is_ok_and(|receipt| receipt.starts_with("timeout:"));
                let output = read_foreground_bash_output(&info).await;
                let exit_code = if timeout { Some(124) } else { status.code() };
                return Ok(ToolOutput::new(format_command_output(output, exit_code))
                    .with_title(title)
                    .with_metadata(json!({"exit_code": exit_code})));
            }
            if shutdown.is_some_and(|signal| signal.is_set()) {
                if stops_work() {
                    let _ = crate::platform::signal_detached_process_group(pid, libc::SIGKILL);
                    let _ = child.wait();
                    guard.disarm();
                    let output = read_foreground_bash_output(&info).await;
                    return Ok(ToolOutput::new(format!(
                        "Command interrupted by user; its process group was killed.\n\n{output}"
                    ))
                    .with_title(title)
                    .with_metadata(json!({ "interrupted": true, "pid": pid })));
                }
                break (
                    "Command continued in background due to reload.".to_owned(),
                    true,
                    false,
                );
            }
            if handoff.as_ref().is_some_and(|signal| signal.is_set()) {
                break (
                    "Command moved to background and is still running.".to_owned(),
                    false,
                    false,
                );
            }
            if !supervised && started.elapsed() >= Duration::from_millis(timeout_ms) {
                break (
                    format!(
                        "Command exceeded the foreground timeout after {:.1}s and is continuing in background (not killed).",
                        timeout_ms as f64 / 1000.0
                    ),
                    false,
                    true,
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        let display_name = summarize_background_command(params.intent.as_deref(), &params.command);
        manager
            .register_detached_task(
                &info,
                "bash",
                Some(display_name.clone()),
                &ctx.session_id,
                pid,
                &started_at,
                params.notify,
                params.wake,
            )
            .await?;
        guard.disarm();
        let cancellation = shutdown
            .cloned()
            .filter(|_| !reload)
            .map(|signal| BashCancellation {
                signal,
                stops_work: Box::new(stops_work),
            });
        spawn_detached_progress_follower(
            manager.clone(),
            info.task_id.clone(),
            info.output_file.clone(),
            cancellation,
        );
        Ok(ToolOutput::new(format!(
            "{reason}\n\nTask ID: {}\nName: {display_name}\nOutput file: {}\nStatus file: {}\n\n{BACKGROUND_WAIT_GUIDANCE}\n\nThe command is still running; do not rerun it unless you intentionally want a second copy.",
            info.task_id, info.output_file.display(), info.status_file.display(),
        )).with_title(title).with_metadata(json!({
            "background": true,
            "task_id": info.task_id,
            "display_name": display_name,
            "output_file": info.output_file.to_string_lossy(),
            "status_file": info.status_file.to_string_lossy(),
            "timeout_promoted": timed_out,
            "foreground_timeout_ms": timeout_ms,
            "foreground_elapsed_ms": started.elapsed().as_millis() as u64,
            "reload_persisted": reload,
            "pid": pid,
        })))
    }

    /// Execute a command in the background
    async fn execute_background(&self, params: BashInput, ctx: ToolContext) -> Result<ToolOutput> {
        let description = params.intent.clone();
        let display_name = summarize_background_command(description.as_deref(), &params.command);
        let wake = params.wake;
        let notify = params.notify || wake;
        let manager = self.manager();
        #[cfg(unix)]
        let info = {
            let session_environment = crate::session_env::session_env(&ctx.session_id);
            #[cfg(test)]
            tests::wait_for_session_env_launch_gate(&ctx.session_id).await;
            let info = manager.reserve_task_info();
            let started_at = Utc::now().to_rfc3339();
            let child = launch_detached_bash(
                &params,
                &ctx,
                &info,
                session_environment,
                background_timeout_ms(params.timeout),
            )?;
            let pid = child.id();
            let mut guard = ProcessGroupKillGuard::new(Some(pid));
            manager
                .register_detached_task(
                    &info,
                    "bash",
                    Some(display_name.clone()),
                    &ctx.session_id,
                    pid,
                    &started_at,
                    notify,
                    wake,
                )
                .await?;
            guard.disarm();
            spawn_detached_progress_follower(
                manager.clone(),
                info.task_id.clone(),
                info.output_file.clone(),
                None,
            );
            info
        };
        #[cfg(windows)]
        let info = {
            let command = params.command.clone();
            let working_dir = ctx.working_dir.clone();
            let cloud_environment = cloud_command_environment(working_dir.as_deref())?;
            let session_id_env = ctx.session_id.clone();
            // Capture before handing work to the manager. Its closure can start
            // after a caller has already replaced or cleared these overrides.
            let session_environment = crate::session_env::session_env(&ctx.session_id);
            let timeout_ms = background_timeout_ms(params.timeout);
            let timeout_duration = timeout_ms.map(Duration::from_millis);

            self.manager()
                .spawn_with_notify(
                    "bash",
                    Some(display_name.clone()),
                    &ctx.session_id,
                    notify,
                    wake,
                    move |output_path| async move {
                        let mut cmd = build_shell_command(&command);
                        cmd.envs(cloud_environment);
                        cmd.envs(session_environment);
                        cmd.env("JCODE_SESSION_ID", &session_id_env);
                        // build_shell_command runs the child in its own session
                        // (setsid), so it is already its own process group leader.
                        cmd.kill_on_drop(true);
                        configure_background_command_stdio(&mut cmd);
                        if let Some(ref dir) = working_dir {
                            cmd.current_dir(dir);
                        }
                        let mut child = cmd
                            .spawn()
                            .map_err(|e| anyhow::anyhow!("Failed to spawn command: {}", e))?;
                        #[cfg(unix)]
                        let mut process_group_guard = ProcessGroupKillGuard::new(child.id());

                        // Stream output to file
                        let mut file = tokio::fs::File::create(&output_path)
                            .await
                            .map_err(|e| anyhow::anyhow!("Failed to create output file: {}", e))?;

                        // Read stdout and stderr truly concurrently using select!
                        // Sequential reads can deadlock if the unread pipe fills up.
                        let stdout = child.stdout.take();
                        let stderr = child.stderr.take();

                        let mut stdout_lines = stdout.map(|s| BufReader::new(s).lines());
                        let mut stderr_lines = stderr.map(|s| BufReader::new(s).lines());
                        let mut stdout_done = stdout_lines.is_none();
                        let mut stderr_done = stderr_lines.is_none();
                        let timeout_sleep = timeout_duration.map(tokio::time::sleep);
                        tokio::pin!(timeout_sleep);
                        let mut timed_out = false;

                            while !stdout_done || !stderr_done {
                                tokio::select! {
                                    _ = async {
                                        match timeout_sleep.as_mut().as_pin_mut() {
                                            Some(sleep) => sleep.await,
                                            None => std::future::pending().await,
                                        }
                                    }, if timeout_duration.is_some() => {
                                        timed_out = true;
                                        #[cfg(unix)]
                                        {
                                            if let Some(pid) = child.id() {
                                                let _ = crate::platform::signal_detached_process_group(pid, libc::SIGKILL);
                                            } else {
                                                let _ = child.start_kill();
                                            }
                                        }
                                        #[cfg(not(unix))]
                                        {
                                            let _ = child.start_kill();
                                        }
                                        break;
                                    }
                                line = async {
                                    match stdout_lines.as_mut() {
                                        Some(r) => r.next_line().await,
                                        None => std::future::pending().await,
                                    }
                                }, if !stdout_done => {
                                    match line {
                                        Ok(Some(line)) => {
                                            handle_background_output_line(&output_path, &mut file, &line, false).await;
                                        }
                                        _ => { stdout_done = true; }
                                    }
                                }
                                line = async {
                                    match stderr_lines.as_mut() {
                                        Some(r) => r.next_line().await,
                                        None => std::future::pending().await,
                                    }
                                }, if !stderr_done => {
                                    match line {
                                        Ok(Some(line)) => {
                                            handle_background_output_line(&output_path, &mut file, &line, true).await;
                                        }
                                        _ => { stderr_done = true; }
                                    }
                                }
                            }
                        }

                        if timed_out {
                            let _ = child.wait().await;
                            #[cfg(unix)]
                            process_group_guard.disarm();
                            let msg = timeout_message(timeout_ms.unwrap_or_default());
                            let timeout_line = format!("\n--- {} ---\n", msg);
                            file.write_all(timeout_line.as_bytes()).await.ok();
                            return Ok(TaskResult::failed(Some(124), msg));
                        }

                        let status = child.wait().await?;
                        #[cfg(unix)]
                        process_group_guard.disarm();
                        let exit_code = status.code();

                        // Write final status line
                        let status_line = format!(
                            "\n--- Command finished with exit code: {} ---\n",
                            exit_code.unwrap_or(-1)
                        );
                        file.write_all(status_line.as_bytes()).await.ok();

                        if status.success() {
                            Ok(TaskResult::completed(exit_code))
                        } else {
                            Ok(TaskResult::failed(
                                exit_code,
                                format!("Command exited with code {}", exit_code.unwrap_or(-1)),
                            ))
                        }
                    },
                )
                .await
        };

        let notify_msg = if wake {
            "The agent will be woken when the task completes."
        } else if notify {
            "You will be notified when the task completes."
        } else {
            "Notifications disabled."
        };

        let stall_msg = match params.stall_wake_seconds {
            Some(requested) => match manager.arm_stall_watchdog(&info.task_id, requested).await {
                Some(effective) => format!(
                    "Stall watchdog armed: you will be woken after {}s with no output or progress (resets on activity).\n",
                    effective
                ),
                None => String::new(),
            },
            None => String::new(),
        };

        let output = format!(
            "Command started in background as requested.\n\n\
             Task ID: {}\n\
             Name: {}\n\
             Output file: {}\n\
             Status file: {}\n\n\
             {}\n{}\n\
             {BACKGROUND_WAIT_GUIDANCE}",
            info.task_id,
            display_name,
            info.output_file.display(),
            info.status_file.display(),
            notify_msg,
            stall_msg,
        );

        Ok(ToolOutput::new(output)
            .with_title(description.unwrap_or_else(|| format!("Background: {}", params.command)))
            .with_metadata(json!({
                "background": true,
                "task_id": info.task_id,
                "display_name": display_name,
                "output_file": info.output_file.to_string_lossy(),
                "status_file": info.status_file.to_string_lossy(),
            })))
    }
}

#[cfg(all(test, not(windows)))]
#[path = "bash_tests.rs"]
mod tests;
