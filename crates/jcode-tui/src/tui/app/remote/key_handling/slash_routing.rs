use super::*;

/// Route a slash input through the remote client instead of the local
/// `submit_input` path. Built-in slash commands still belong to the client,
/// but a skill invocation with a trailing prompt must become a remote turn.
/// Calling `App::submit_input` directly for that case sets `pending_turn`,
/// which only the local run loop consumes, leaving remote sessions stuck in
/// the sending state.
pub(in crate::tui::app) async fn submit_remote_slash_input(
    app: &mut App,
    remote: &mut RemoteConnection,
    prepared: input::PreparedInput,
) -> Result<()> {
    let raw_input = prepared.raw_input.clone();

    if matches!(
        prepared.expanded.split_whitespace().next(),
        Some("/mode" | "/acct")
    ) {
        return Box::pin(submit_prepared_remote_enter_input(app, remote, prepared)).await;
    }

    if crate::tui::is_ssh_remote() {
        // Box the shared dispatcher because its non-SSH fallback can call
        // this entry point. The SSH branch terminates in a wire send instead.
        return Box::pin(submit_prepared_remote_enter_input(app, remote, prepared)).await;
    }

    // Text that merely starts with `/` is not necessarily a command. A terminal
    // file drop (`/tmp/shot.png`) or a bare path (`/home/me/notes`) is ordinary
    // user input. Routing those through `App::submit_input` stages a *local*
    // turn via `pending_turn`, which no remote run loop consumes, so the client
    // parks in "Sending" forever. Send them as a normal remote turn instead.
    //
    // `/?` is the one builtin whose token is not identifier-shaped, so it is
    // allowed through explicitly.
    // Resolve registered multi-word skill names before falling back to the
    // existing single-token command handling.
    let snapshot = app.current_skills_snapshot();
    let trimmed = raw_input.trim();
    let is_command_shaped = trimmed == "/?"
        || (input::parse_dropped_paths(&raw_input).is_none()
            && (snapshot.resolve_invocation(&raw_input).is_some()
                || app_mod::commands_dispatch::contains_registered_slash_command(trimmed)));
    if !is_command_shaped {
        return super::super::input_dispatch::submit_prepared_remote_input(app, remote, prepared)
            .await;
    }

    let Some(invocation) = snapshot.resolve_invocation(&raw_input) else {
        app.input = raw_input;
        app.cursor_pos = app.input.len();
        app.submit_input();
        return Ok(());
    };

    let mut skill_snapshot = snapshot;
    if skill_snapshot.get(invocation.name).is_none() {
        app.refresh_skills_snapshot();
        skill_snapshot = app.current_skills_snapshot();
    }
    if let Some(skill) = skill_snapshot.get(invocation.name) {
        if !app_mod::registered_command_names()
            .any(|name| trimmed.split_whitespace().next() == Some(name))
        {
            match skill.command_line(invocation.raw_arguments(&prepared.expanded)) {
                Ok(Some(command)) => {
                    return super::super::input_dispatch::submit_remote_input_shell(
                        app, remote, raw_input, command,
                    )
                    .await;
                }
                Err(error) => {
                    app.push_display_message(DisplayMessage::error(error.to_string()));
                    return Ok(());
                }
                Ok(None) => {}
            }
        }
        remote
            .runtime_command(
                "mode",
                vec![
                    "--invoke-skill".into(),
                    invocation.name.to_owned(),
                    invocation.raw_arguments(&prepared.expanded).to_owned(),
                ],
            )
            .await?;
    }

    let Some(trailing_prompt) = invocation.prompt else {
        app.input = raw_input;
        app.cursor_pos = app.input.len();
        app.submit_input();
        return Ok(());
    };

    let skill_name = invocation.name.to_string();
    if skill_snapshot.get(&skill_name).is_none() {
        // Preserve the existing unknown-skill and built-in slash-command
        // handling, including the helpful endorsed-skill installation hint.
        app.input = raw_input;
        app.cursor_pos = app.input.len();
        app.submit_input();
        return Ok(());
    }

    // Reuse the normal bare invocation path to update active_skill and show
    // the activation notice, then prepare only the trailing prompt for the
    // remote request. This avoids duplicating slash-command presentation and
    // keeps pasted images attached to the same user turn.
    app.input = format!("/{}", skill_name);
    app.cursor_pos = app.input.len();
    app.pending_images.clear();
    app.submit_input();

    let expanded_prompt = app
        .current_skills_snapshot()
        .resolve_invocation(&prepared.expanded)
        .and_then(|invocation| invocation.prompt)
        .unwrap_or(trailing_prompt)
        .to_string();
    super::super::input_dispatch::submit_prepared_remote_input(
        app,
        remote,
        input::PreparedInput {
            pasted_contents: Vec::new(),
            raw_input: prepared.raw_input,
            expanded: expanded_prompt,
            images: prepared.images,
        },
    )
    .await
}
