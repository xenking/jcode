use super::*;

pub(in super::super) fn current_subagent_model_summary(app: &App) -> String {
    match app.session.subagent_model() {
        Some(model) => format!("fixed {}", model),
        None => format!("inherit current ({})", app.provider.model()),
    }
}

pub(in super::super) fn parse_manual_subagent_spec(
    rest: &str,
) -> Result<ManualSubagentSpec, String> {
    let mut iter = rest.split_whitespace().peekable();
    let mut subagent_type = "general".to_string();
    let mut model = None;
    let mut session_id = None;
    let mut prompt_tokens = Vec::new();

    while let Some(token) = iter.next() {
        match token {
            "--type" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "Missing value for --type.".to_string())?;
                subagent_type = value.to_string();
            }
            "--model" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "Missing value for --model.".to_string())?;
                model = Some(value.to_string());
            }
            "--continue" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "Missing value for --continue.".to_string())?;
                session_id = Some(value.to_string());
            }
            flag if flag.starts_with("--") => {
                return Err(format!("Unknown flag {}.", flag));
            }
            prompt_start => {
                prompt_tokens.push(prompt_start.to_string());
                prompt_tokens.extend(iter.map(str::to_string));
                break;
            }
        }
    }

    let prompt = prompt_tokens.join(" ").trim().to_string();
    if prompt.is_empty() {
        return Err("Missing prompt. Add text after /subagent.".to_string());
    }

    Ok(ManualSubagentSpec {
        subagent_type,
        model,
        session_id,
        prompt,
    })
}

pub(super) fn handle_subagent_model_command(app: &mut App, trimmed: &str) -> bool {
    if !trimmed.starts_with("/subagent-model") {
        return false;
    }

    if app.is_remote {
        app.push_display_message(DisplayMessage::error(
            "/subagent-model requires a live jcode server connection in remote mode.".to_string(),
        ));
        return true;
    }

    let rest = trimmed
        .strip_prefix("/subagent-model")
        .unwrap_or_default()
        .trim();

    if rest.is_empty() || matches!(rest, "show" | "status") {
        app.push_display_message(DisplayMessage::system(format!(
            "Subagent model for this session: {}\n\nUse /subagent-model <name> to pin a model, or /subagent-model inherit to use the current model.",
            current_subagent_model_summary(app)
        )));
        return true;
    }

    if matches!(rest, "inherit" | "reset" | "clear") {
        app.session.set_subagent_model(None);
        let _ = app.session.save();
        app.push_display_message(DisplayMessage::system(format!(
            "Subagent model reset to inherit the current model ({}).",
            app.provider.model()
        )));
        app.set_status_notice("Subagent model: inherit");
        return true;
    }

    app.session.set_subagent_model(Some(rest.to_string()));
    let _ = app.session.save();
    app.push_display_message(DisplayMessage::system(format!(
        "Subagent model pinned to {} for this session.",
        rest
    )));
    app.set_status_notice(format!("Subagent model → {}", rest));
    true
}

pub(super) fn handle_subagent_command(app: &mut App, trimmed: &str) -> bool {
    if !trimmed.starts_with("/subagent") || trimmed.starts_with("/subagent-model") {
        return false;
    }

    if app.is_remote {
        app.push_display_message(DisplayMessage::error(
            "/subagent requires a live jcode server connection in remote mode.".to_string(),
        ));
        return true;
    }

    let rest = trimmed.strip_prefix("/subagent").unwrap_or_default().trim();
    if rest.is_empty() {
        app.push_display_message(DisplayMessage::error(
            "Usage: /subagent [--type <kind>] [--model <name>] [--continue <session_id>] <prompt>"
                .to_string(),
        ));
        return true;
    }

    match parse_manual_subagent_spec(rest) {
        Ok(spec) => launch_manual_subagent(app, spec),
        Err(error) => {
            app.push_display_message(DisplayMessage::error(format!(
                "{}\nUsage: /subagent [--type <kind>] [--model <name>] [--continue <session_id>] <prompt>",
                error
            )));
        }
    }
    true
}

/// `/cancel` (and `/stop`) interrupt the in-flight turn, mirroring Ctrl+C
/// while processing. The command has long been registered and advertised by
/// interactive prompts, but had no top-level dispatch, so typing it outside a
/// pending prompt fell through to skill parsing and produced
/// "Unknown skill: /cancel" (issue #496).
pub(in super::super) fn handle_cancel_command(app: &mut App, trimmed: &str) -> bool {
    if trimmed != "/cancel" && trimmed != "/stop" {
        return false;
    }

    let pending_retry = app.rate_limit_reset.is_some() && app.rate_limit_pending_message.is_some();
    if app.is_processing {
        app.cancel_requested = true;
        app.interleave_message = None;
        app.interleave_images.clear();
        app.pending_soft_interrupts.clear();
        app.pending_soft_interrupt_requests.clear();
        if app.cancel_overnight_for_interrupt() {
            app.set_status_notice("Interrupting... Overnight cancelled");
        } else {
            app.set_status_notice("Interrupting...");
        }
    } else if pending_retry {
        app.clear_pending_remote_retry();
        if matches!(app.status, ProcessingStatus::WaitingForNetwork { .. }) {
            app.status = ProcessingStatus::Idle;
            app.status_detail = None;
        }
        app.set_status_notice("Pending retry cancelled");
    } else {
        app.push_display_message(DisplayMessage::system(
            "Nothing to cancel: no prompt or operation is in progress.".to_string(),
        ));
    }
    true
}

pub(in super::super) fn handle_help_command(app: &mut App, trimmed: &str) -> bool {
    if let Some(topic) = trimmed
        .strip_prefix("/help ")
        .or_else(|| trimmed.strip_prefix("/? "))
    {
        if let Some(help) = app.command_help(topic) {
            app.push_display_message(DisplayMessage::system(help));
        } else {
            app.push_display_message(DisplayMessage::error(format!(
                "Unknown command '{}'. Use /help to list commands.",
                topic.trim()
            )));
        }
        return true;
    }

    if trimmed == "/help" || trimmed == "/?" || trimmed == "/commands" {
        app.help_scroll = Some(0);
        return true;
    }

    false
}

/// `/keys` shows the keymap diagnostics: detected terminal, discovered terminal
/// and macOS shortcuts, and any conflicts with jcode's own keybindings.
/// `/keys refresh` forces a fresh scan of the machine (otherwise a cached
/// snapshot up to a day old is reused).
pub(in super::super) fn handle_keys_command(app: &mut App, trimmed: &str) -> bool {
    let Some(rest) = slash_command_rest(trimmed, "/keys")
        .or_else(|| slash_command_rest(trimmed, "/keybindings"))
    else {
        return false;
    };

    let force_refresh = matches!(rest.trim(), "refresh" | "rescan" | "reload");
    let snapshot = if force_refresh {
        crate::setup_hints::keymap::refresh_and_save()
    } else {
        crate::setup_hints::keymap::snapshot_cached_or_refresh()
    };

    let cfg = crate::config::config();
    let report = crate::setup_hints::keymap::render_report(&cfg.keybindings, &snapshot);
    app.push_display_message(DisplayMessage::system(report));

    if let Some(status) =
        crate::setup_hints::keymap::render_status_line(&cfg.keybindings, &snapshot)
    {
        app.set_status_notice(status);
    } else {
        app.set_status_notice("No keybinding conflicts detected");
    }
    true
}

pub(in super::super) fn handle_model_status_command(app: &mut App, trimmed: &str) -> bool {
    let Some(rest) = slash_command_rest(trimmed, "/provider-test-coverage")
        .or_else(|| slash_command_rest(trimmed, "/model-status"))
    else {
        return false;
    };

    if rest.trim().is_empty() {
        app.model_status_content = build_provider_test_coverage_summary();
        app.model_status_scroll = Some(0);
        return true;
    }

    let mut parts = rest.split_whitespace();
    let provider = parts
        .next()
        .map(str::to_string)
        .unwrap_or_else(|| app.provider_name().to_string());
    let explicit_model = parts.collect::<Vec<_>>().join(" ");
    let model = if explicit_model.trim().is_empty() {
        app.provider_model()
    } else {
        explicit_model
    };

    app.model_status_content = build_model_status_report(&provider, &model);
    app.model_status_scroll = Some(0);
    true
}

/// Parse an explicit diff-mode name accepted by `/diff <mode>`. Returns `None`
/// for unrecognized values so the caller can report a usage error.
pub(super) fn parse_diff_mode_name(value: &str) -> Option<crate::config::DiffDisplayMode> {
    use crate::config::DiffDisplayMode;
    match value.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "hide" | "hidden" => Some(DiffDisplayMode::Off),
        "inline" | "on" => Some(DiffDisplayMode::Inline),
        "full" | "full-inline" | "full_inline" | "fullinline" | "inline-full" => {
            Some(DiffDisplayMode::FullInline)
        }
        "file" | "fullfile" | "full-file" => Some(DiffDisplayMode::File),
        _ => None,
    }
}

pub(super) fn apply_diff_mode(app: &mut App, mode: crate::config::DiffDisplayMode) {
    app.diff_mode = mode;
    if !app.diff_pane_visible() {
        app.diff_pane_focus = false;
    }
    app.set_status_notice(format!("Diffs: {}", app.diff_mode.label()));
}

pub(in super::super) fn handle_diff_command(app: &mut App, trimmed: &str) -> bool {
    let Some(rest) = slash_command_rest(trimmed, "/diff") else {
        return false;
    };
    let arg = rest.trim();

    if arg.is_empty() || arg.eq_ignore_ascii_case("cycle") || arg.eq_ignore_ascii_case("next") {
        let next = app.diff_mode.cycle();
        apply_diff_mode(app, next);
        return true;
    }

    if arg.eq_ignore_ascii_case("status") {
        app.push_display_message(DisplayMessage::system(format!(
            "Diff mode: {} (use /diff [off|inline|full|file] or /diff to cycle)",
            app.diff_mode.label()
        )));
        return true;
    }

    match parse_diff_mode_name(arg) {
        Some(mode) => apply_diff_mode(app, mode),
        None => app.push_display_message(DisplayMessage::error(
            "Usage: /diff [off|inline|full|file|cycle|status]".to_string(),
        )),
    }
    true
}

pub(in super::super) fn handle_log_command(app: &mut App, trimmed: &str) -> bool {
    let Some(rest) = slash_command_rest(trimmed, "/log") else {
        return false;
    };

    let mut parts = rest.splitn(2, char::is_whitespace);
    let subcommand = parts.next().unwrap_or_default();
    let note = parts.next().unwrap_or_default().trim();

    if subcommand != "mark" {
        app.push_display_message(DisplayMessage::error("Usage: /log mark [note]".to_string()));
        return true;
    }

    let marker_id = format!(
        "logmark-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S%.3f")
    );
    let working_dir = active_working_dir(app)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let note_for_log = if note.is_empty() { "(none)" } else { note };

    crate::logging::info(&format!(
        "JCODE_LOG_MARK id={} session={} provider={} model={} cwd={} note={}",
        marker_id,
        app.session.id(),
        app.provider_name(),
        app.provider_model(),
        working_dir,
        note_for_log
    ));

    let mut message = format!(
        "Log mark written: {}\n\nAgents can search ~/.jcode/logs/ for JCODE_LOG_MARK or this marker id.",
        marker_id
    );
    if !note.is_empty() {
        message.push_str(&format!("\n\nNote: {}", note));
    }
    app.push_display_message(DisplayMessage::system(message));
    app.set_status_notice(format!("Log mark {}", marker_id));
    true
}

pub(super) fn build_model_status_report(provider_query: &str, model_query: &str) -> String {
    crate::live_tests::format_provider_test_coverage_report(provider_query, model_query, None)
}

pub(super) fn build_provider_test_coverage_summary() -> String {
    match crate::live_tests::load_coverage(None) {
        Ok((coverage, path)) => {
            let summary = crate::live_tests::strict_live_provider_model_coverage_summary(
                &coverage,
                path.display().to_string(),
            );
            // 0 = no per-pair cap: the overlay scrolls, so show every pair.
            crate::live_tests::format_strict_live_provider_model_coverage_summary(&summary, 0)
        }
        Err(err) => {
            let mut out = String::new();
            out.push_str("Live provider/model E2E coverage\n");
            out.push_str("Status: no verification ledger found on this install\n");
            out.push_str(&format!("Ledger error: {err}\n"));
            out
        }
    }
}
