use super::{ToolContext, ToolOutput};
use anyhow::{Context, Result, ensure};
use jcode_sdk::desktop_control::{
    COMPUTER_USE_TIMEOUT_SECS, DESKTOP_CONTROL_VERSION, DesktopAction, DesktopRequest,
    DesktopResponse, DesktopTarget, endpoints, request,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub(super) async fn execute(ctx: ToolContext, input: Value) -> Result<ToolOutput> {
    let ToolContext {
        session_id,
        mut graceful_shutdown_signal,
        ..
    } = ctx;
    tokio::select! {
        biased;
        _ = async {
            match graceful_shutdown_signal.as_mut() {
                Some(signal) => signal.notified().await,
                None => std::future::pending().await,
            }
        } => Ok(failure("interrupted", "Computer use was cancelled. If an action had already started, its result is unknown.")),
        output = execute_for(&session_id, input) => output,
    }
}

async fn execute_for(session_id: &str, input: Value) -> Result<ToolOutput> {
    let session_id = session_id.to_owned();
    let selected = tokio::task::spawn_blocking(move || locate(endpoints()?, session_id)).await?;
    let (path, target) = match selected {
        Ok(route) => route,
        Err(error) => return Ok(failure("desktop_unavailable", &error.to_string())),
    };
    let response = tokio::time::timeout(
        Duration::from_secs(COMPUTER_USE_TIMEOUT_SECS + 5),
        send(&path, DesktopAction::ComputerUse { target, input }),
    )
    .await;
    match response {
        Ok(Ok(DesktopResponse::Ok { result })) => Ok(serde_json::from_value(result)?),
        Ok(Ok(DesktopResponse::Error { code, message })) => Ok(failure(&code, &message)),
        Ok(Err(error)) => Ok(failure("desktop_transport", &error.to_string())),
        Err(_) => Ok(failure(
            "desktop_timeout",
            "No response from the Desktop computer-use broker. If an action had already started, its result is unknown.",
        )),
    }
}

fn locate(paths: Vec<PathBuf>, session_id: String) -> Result<(PathBuf, DesktopTarget)> {
    let mut found = None;
    for path in paths {
        let Ok(DesktopResponse::Ok { result }) = request(
            &path,
            DesktopAction::ComputerTarget {
                session_id: session_id.clone(),
            },
            Duration::from_millis(500),
        ) else {
            continue;
        };
        let Ok(target) = serde_json::from_value::<DesktopTarget>(result) else {
            continue;
        };
        if target.session_id != session_id {
            continue;
        }
        ensure!(
            found.is_none(),
            "Computer use requires the session to be open in exactly one local Jcode Desktop window"
        );
        found = Some((path, target));
    }
    found.context("Open this session in a compatible local Jcode Desktop to use macOS computer use")
}

async fn send(path: &Path, action: DesktopAction) -> Result<DesktopResponse> {
    // Keep the connection in this future: cancellation closes it rather than
    // leaving a blocking client to execute after the agent turn is gone.
    let mut stream = UnixStream::connect(path)
        .await
        .context("connect to Desktop computer use")?;
    let mut bytes = serde_json::to_vec(&DesktopRequest {
        v: DESKTOP_CONTROL_VERSION,
        action,
    })?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    let mut response = String::new();
    BufReader::new(stream)
        .take(8 * 1024 * 1024)
        .read_line(&mut response)
        .await?;
    ensure!(
        response.ends_with('\n'),
        "incomplete Desktop computer-use response"
    );
    Ok(serde_json::from_str(&response)?)
}

fn failure(reason: &str, message: &str) -> ToolOutput {
    ToolOutput::new(json!({"reason": reason, "message": message}).to_string())
        .with_metadata(json!({"reason": reason}))
}
