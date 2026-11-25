use super::*;

impl App {
    pub(in super::super) fn open_session_picker(&mut self) {
        if super::super::commands_dispatch::ssh_local_action_blocked(self, "Local session picker") {
            return;
        }
        let current_dir = self.session.working_dir.clone();
        // Even the cached list can contain tens of MB of search text. Never
        // deserialize it on the input thread before showing the overlay.
        let mut picker = SessionPicker::loading();
        picker.set_current_dir(current_dir);
        picker.set_current_session_id(Some(super::super::commands::active_session_id(self)));
        picker.apply_resume_project_scope_default();
        self.session_picker_overlay = Some(RefCell::new(picker));
        self.session_picker_mode = SessionPickerMode::Resume;
        self.set_status_notice("Loading sessions...");
        self.start_session_picker_load();
    }

    /// Open the active sessions manager: the session picker scoped to live
    /// (open) sessions, showing which are still working on a response and
    /// which are ready for input. Reached via Left arrow on an empty input
    /// (when `display.active_sessions_manager` is enabled) or `/active`.
    pub(in super::super) fn open_active_sessions_picker(&mut self) {
        if super::super::commands_dispatch::ssh_local_action_blocked(
            self,
            "Local active-session picker",
        ) {
            return;
        }
        let current_dir = self.session.working_dir.clone();
        let mut picker = SessionPicker::loading();
        picker.set_current_dir(current_dir);
        picker.set_current_session_id(Some(super::super::commands::active_session_id(self)));
        picker.activate_active_filter();
        self.session_picker_overlay = Some(RefCell::new(picker));
        self.session_picker_mode = SessionPickerMode::ActiveSessions;
        self.set_status_notice("Loading active sessions...");
        self.start_session_picker_load();
    }

    /// Opt-in Left-arrow gesture: pressing Left on an empty input opens the
    /// active sessions manager. Gated behind `display.active_sessions_manager`
    /// so the default input behavior is unchanged. Returns true when the
    /// gesture fired.
    pub(in super::super) fn maybe_open_active_sessions_on_left(&mut self) -> bool {
        if !self.input.is_empty() || self.cursor_pos != 0 {
            return false;
        }
        if !crate::config::config().display.active_sessions_manager {
            return false;
        }
        self.open_active_sessions_picker();
        true
    }

    /// Tick hook: while the session picker overlay is up, periodically refresh
    /// the live presence snapshot so working/ready badges (and the Active view
    /// membership) track reality. Returns true when a redraw is needed.
    pub(in super::super) fn poll_session_picker_presence(&mut self) -> bool {
        let Some(picker_cell) = self.session_picker_overlay.as_ref() else {
            return false;
        };
        picker_cell.borrow_mut().maybe_refresh_live_presence()
    }

    pub(super) fn start_session_picker_load(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending_session_picker_load =
            Some(super::super::PendingSessionPickerLoad { receiver: rx });

        tokio::task::spawn_blocking(move || {
            session_picker::send_session_list_updates(tx);
        });
    }

    /// Rebuild the picker overlay from a freshly loaded session list, applying
    /// the filter for the active picker mode. Returns true when the overlay was
    /// (re)built so the caller can request a redraw.
    pub(super) fn apply_loaded_session_picker(
        &mut self,
        server_groups: Vec<session_picker::ServerGroup>,
        orphan_sessions: Vec<session_picker::SessionInfo>,
    ) -> bool {
        // When a picker overlay is already on screen (the common case: the cached
        // list rendered instantly and this is the async full-refresh landing),
        // reseed it in place so the user's selection, scroll, search, focus, and
        // multi-select survive the swap. Rebuilding a fresh picker here used to
        // yank the view out from under the user a second or two after they opened
        // `/resume`, which felt like a lag/jump.
        let has_overlay = self.session_picker_overlay.is_some();
        if has_overlay {
            let notice = match self.session_picker_mode {
                SessionPickerMode::Resume => {
                    if let Some(existing) = self.session_picker_overlay.as_ref() {
                        let mut picker = existing.borrow_mut();
                        picker.reseed_grouped(server_groups, orphan_sessions);
                        picker.apply_resume_project_scope_default();
                    }
                    "Sessions loaded"
                }
                SessionPickerMode::CatchUp => {
                    if let Some(existing) = self.session_picker_overlay.as_ref() {
                        let mut picker = existing.borrow_mut();
                        // Keep the catch-up filter active; reseed preserves it.
                        picker.activate_catchup_filter();
                        picker.reseed_grouped(server_groups, orphan_sessions);
                    }
                    "Catch Up sessions loaded"
                }
                SessionPickerMode::ActiveSessions => {
                    if let Some(existing) = self.session_picker_overlay.as_ref() {
                        let mut picker = existing.borrow_mut();
                        // Keep the active filter; reseed preserves it and
                        // refreshes the live presence snapshot.
                        picker.activate_active_filter();
                        picker.reseed_grouped(server_groups, orphan_sessions);
                    }
                    "Active sessions loaded"
                }
                SessionPickerMode::Onboarding => return false,
            };
            self.set_status_notice(notice);
            return true;
        }

        match self.session_picker_mode {
            SessionPickerMode::Resume => {
                let mut picker = SessionPicker::new_grouped(server_groups, orphan_sessions);
                picker.set_current_dir(self.session.working_dir.clone());
                picker
                    .set_current_session_id(Some(super::super::commands::active_session_id(self)));
                picker.apply_resume_project_scope_default();
                self.session_picker_overlay = Some(RefCell::new(picker));
                self.set_status_notice("Sessions loaded");
                true
            }
            SessionPickerMode::CatchUp => {
                let mut picker = SessionPicker::new_grouped(server_groups, orphan_sessions);
                picker.activate_catchup_filter();
                picker.set_current_dir(self.session.working_dir.clone());
                self.session_picker_overlay = Some(RefCell::new(picker));
                self.set_status_notice("Catch Up sessions loaded");
                true
            }
            SessionPickerMode::ActiveSessions => {
                let mut picker = SessionPicker::new_grouped(server_groups, orphan_sessions);
                picker.set_current_dir(self.session.working_dir.clone());
                picker
                    .set_current_session_id(Some(super::super::commands::active_session_id(self)));
                picker.activate_active_filter();
                self.session_picker_overlay = Some(RefCell::new(picker));
                self.set_status_notice("Active sessions loaded");
                true
            }
            // Onboarding constructs its action-only picker synchronously, so it
            // never flows through this async path.
            SessionPickerMode::Onboarding => false,
        }
    }

    pub(in super::super) fn poll_session_picker_load(&mut self) -> bool {
        let recv_result = {
            let Some(pending) = self.pending_session_picker_load.as_ref() else {
                return false;
            };
            pending.receiver.try_recv()
        };

        let picker_active = self.session_picker_overlay.is_some()
            && matches!(
                self.session_picker_mode,
                SessionPickerMode::Resume
                    | SessionPickerMode::CatchUp
                    | SessionPickerMode::ActiveSessions
            );

        match recv_result {
            Ok(session_picker::SessionListUpdate::CloudUnavailable(warning)) => {
                if picker_active && let Some(picker) = self.session_picker_overlay.as_ref() {
                    picker.borrow_mut().set_cloud_warning(warning);
                    return true;
                }
                false
            }
            Ok(session_picker::SessionListUpdate::Cached((server_groups, orphan_sessions))) => {
                if picker_active {
                    let changed = self.apply_loaded_session_picker(server_groups, orphan_sessions);
                    self.set_status_notice("Refreshing sessions...");
                    return changed;
                }
                false
            }
            Ok(session_picker::SessionListUpdate::Fresh(Ok((server_groups, orphan_sessions)))) => {
                self.pending_session_picker_load = None;
                if picker_active {
                    return self.apply_loaded_session_picker(server_groups, orphan_sessions);
                }
                false
            }
            Ok(session_picker::SessionListUpdate::Fresh(Err(e))) => {
                self.pending_session_picker_load = None;
                if picker_active {
                    self.session_picker_overlay = None;
                    self.push_display_message(DisplayMessage::error(format!(
                        "Failed to load sessions: {}",
                        e
                    )));
                    self.set_status_notice("Session load failed");
                    return true;
                }
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pending_session_picker_load = None;
                if picker_active {
                    self.session_picker_overlay = None;
                    self.push_display_message(DisplayMessage::error(
                        "Session loading stopped before returning a result.".to_string(),
                    ));
                    self.set_status_notice("Session load failed");
                    return true;
                }
                false
            }
        }
    }

    pub(in super::super) fn open_catchup_picker(&mut self) {
        if super::super::commands_dispatch::ssh_local_action_blocked(self, "Local catch-up picker")
        {
            return;
        }
        let current_session_id = super::super::commands::active_session_id(self);
        if catchup_candidates(&current_session_id).is_empty() {
            self.push_display_message(DisplayMessage::system(
                "No sessions currently need catch up.".to_string(),
            ));
            self.set_status_notice("Catch Up: none waiting");
            return;
        }

        // Both cached and fresh lists arrive off-thread, just like /resume.
        let mut picker = SessionPicker::loading();
        // Ensure the filter is applied even on the loading placeholder so the
        // refreshed list lands in the catch-up view.
        picker.activate_catchup_filter();
        self.session_picker_overlay = Some(RefCell::new(picker));
        self.session_picker_mode = SessionPickerMode::CatchUp;
        self.set_status_notice("Loading Catch Up sessions...");
        self.start_session_picker_load();
    }

    pub(in super::super) fn handle_session_picker_selection(&mut self, targets: &[ResumeTarget]) {
        if super::super::commands_dispatch::ssh_local_action_blocked(
            self,
            "Opening local session terminals",
        ) {
            return;
        }
        if targets.is_empty() {
            return;
        }

        if self.session_picker_mode == SessionPickerMode::CatchUp {
            let current_session_id = super::super::commands::active_session_id(self);
            let mut names = Vec::with_capacity(targets.len());
            for target in targets {
                let ResumeTarget::JcodeSession { session_id } = target else {
                    continue;
                };
                let queue_position = catchup_queue_position(&current_session_id, session_id);
                self.queue_catchup_resume(
                    session_id.to_string(),
                    Some(current_session_id.clone()),
                    queue_position,
                    true,
                );
                names.push(
                    crate::id::extract_session_name(session_id)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| session_id.to_string()),
                );
            }

            if names.len() == 1 {
                self.push_display_message(DisplayMessage::system(format!(
                    "Queued Catch Up for {}.",
                    names[0],
                )));
                self.set_status_notice(format!("Catch Up → {}", names[0]));
            } else {
                self.push_display_message(DisplayMessage::system(format!(
                    "Queued Catch Up for {} sessions: {}.",
                    names.len(),
                    names.join(", "),
                )));
                self.set_status_notice(format!("Catch Up → {} sessions", names.len()));
            }
            return;
        }

        let default_cwd = std::env::current_dir().unwrap_or_default();
        let socket = std::env::var("JCODE_SOCKET").ok();
        let mut spawned = 0usize;
        let mut failed = Vec::new();
        let mut names = Vec::with_capacity(targets.len());

        for target in targets {
            let mut cwd = default_cwd.clone();
            if let Some(picker_cell) = self.session_picker_overlay.as_ref() {
                let picker = picker_cell.borrow();
                if let Some(session) = picker.session_for_target(target)
                    && let Some(dir) = session.working_dir.as_deref()
                    && std::path::Path::new(dir).is_dir()
                {
                    cwd = std::path::PathBuf::from(dir);
                }
            }

            let name = match target {
                ResumeTarget::JcodeSession { session_id } => {
                    crate::id::extract_session_name(session_id)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| session_id.to_string())
                }
                ResumeTarget::ClaudeCodeSession { session_id, .. } => {
                    format!(
                        "Claude Code {}",
                        jcode_core::util::truncate_str(session_id, 8)
                    )
                }
                ResumeTarget::CodexSession { session_id, .. } => {
                    format!("Codex {}", jcode_core::util::truncate_str(session_id, 8))
                }
                ResumeTarget::PiSession { session_path } => std::path::Path::new(session_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Pi session")
                    .to_string(),
                ResumeTarget::OpenCodeSession { session_id, .. } => {
                    format!("OpenCode {}", jcode_core::util::truncate_str(session_id, 8))
                }
                ResumeTarget::CursorSession { session_id, .. } => {
                    format!("Cursor {}", jcode_core::util::truncate_str(session_id, 8))
                }
            };
            let resolved_target = match crate::import::resolve_resume_target_to_jcode(target) {
                Ok(target) => target,
                Err(err) => {
                    failed.push(format!("failed to import {}: {}", name, err));
                    continue;
                }
            };

            if let ResumeTarget::JcodeSession { session_id } = &resolved_target
                && crate::session_window::apply_resume_open_elsewhere(session_id)
                    == crate::session_window::OpenElsewhereOutcome::Moved
            {
                spawned += 1;
                names.push(name);
                continue;
            }

            match spawn_resume_target_in_new_terminal(&resolved_target, &cwd, socket.as_deref()) {
                Ok(true) => {
                    spawned += 1;
                    names.push(name);
                }
                Ok(false) | Err(_) => {
                    // No terminal emulator could be spawned. For a single jcode
                    // session, fall back to resuming in the current terminal
                    // instead of dead-ending with a manual command (issue #203).
                    if targets.len() == 1
                        && spawned == 0
                        && matches!(resolved_target, ResumeTarget::JcodeSession { .. })
                    {
                        self.handle_session_picker_current_terminal_selection(
                            std::slice::from_ref(target),
                        );
                        return;
                    }
                    failed.push(resume_target_manual_command(
                        &resolved_target,
                        socket.as_deref(),
                    ));
                }
            }
        }

        if spawned > 0 && failed.is_empty() {
            if names.len() == 1 {
                self.push_display_message(DisplayMessage::system(format!(
                    "Resumed {} in new window.",
                    names[0],
                )));
                self.set_status_notice(format!("Resumed {}", names[0]));
            } else {
                self.push_display_message(DisplayMessage::system(format!(
                    "Resumed {} sessions in new windows: {}.",
                    names.len(),
                    names.join(", "),
                )));
                self.set_status_notice(format!("Resumed {} sessions", names.len()));
            }
            return;
        }

        let manual: Vec<String> = failed.iter().map(|cmd| format!("  {}", cmd)).collect();

        if spawned > 0 {
            self.push_display_message(DisplayMessage::system(format!(
                "Resumed {} session(s) in new windows. {} failed:\n{}",
                spawned,
                failed.len(),
                manual.join("\n")
            )));
            self.set_status_notice(format!("Resumed {} session(s)", spawned));
        } else {
            self.push_display_message(DisplayMessage::system(format!(
                "No terminal found. Resume manually:\n{}",
                manual.join("\n")
            )));
        }
    }

    pub(in super::super) fn handle_session_picker_current_terminal_selection(
        &mut self,
        targets: &[ResumeTarget],
    ) {
        if super::super::commands_dispatch::ssh_local_action_blocked(
            self,
            "Importing local sessions",
        ) {
            return;
        }
        let Some(target) = targets.first() else {
            return;
        };

        let name = match target {
            ResumeTarget::JcodeSession { session_id } => {
                crate::id::extract_session_name(session_id)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| session_id.to_string())
            }
            ResumeTarget::ClaudeCodeSession { session_id, .. } => {
                format!(
                    "Claude Code {}",
                    jcode_core::util::truncate_str(session_id, 8)
                )
            }
            ResumeTarget::CodexSession { session_id, .. } => {
                format!("Codex {}", jcode_core::util::truncate_str(session_id, 8))
            }
            ResumeTarget::PiSession { session_path } => std::path::Path::new(session_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Pi session")
                .to_string(),
            ResumeTarget::OpenCodeSession { session_id, .. } => {
                format!("OpenCode {}", jcode_core::util::truncate_str(session_id, 8))
            }
            ResumeTarget::CursorSession { session_id, .. } => {
                format!("Cursor {}", jcode_core::util::truncate_str(session_id, 8))
            }
        };

        let resolved_target = match crate::import::resolve_resume_target_to_jcode(target) {
            Ok(target) => target,
            Err(err) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Failed to import {}: {}",
                    name, err
                )));
                return;
            }
        };

        let ResumeTarget::JcodeSession { session_id } = resolved_target else {
            self.push_display_message(DisplayMessage::error(format!(
                "Cannot resume {} in the current terminal.",
                name
            )));
            return;
        };

        // Ownership takes precedence even over the picker entry labelled current:
        // its local transcript may have moved while this client was open.
        if let Some(lease) = crate::storage::read_session_lease(&session_id)
            && let Some(host) = lease.away_host
        {
            if self.is_processing
                || self.pending_turn
                || self.cloud_job.is_some()
                || self.cloud_handoff_blocks_work()
                || self.should_quit
            {
                self.push_display_message(DisplayMessage::error(
                    "Wait for the current turn or cloud handoff to finish before attaching to a cloud session.".to_string(),
                ));
                return;
            }
            // A picker selection consumes no composer command. In particular,
            // a typed `/cloud` is still an unsent draft, not an accepted command.
            if !self.input.trim().is_empty() {
                self.push_display_message(DisplayMessage::error(
                    "Cloud attach blocked: send or save the composer draft first. Nothing was removed.".to_string(),
                ));
                return;
            }
            // Reuse the cloud handoff guard before quitting this client. This
            // covers attachments, queues and unacknowledged interrupts.
            if !super::super::commands_cloud::check_cloud_handoff_input(self, "/cloud") {
                return;
            }
            self.cloud_handoff_requested = Some(CloudHandoff::RemoteResume {
                session_id,
                host,
                working_dir: lease.repo_root,
            });
            self.session_picker_overlay = None;
            self.session_picker_mode = SessionPickerMode::Resume;
            self.should_quit = true;
            return;
        }

        // Selecting the session we are already in (visible in the Active view,
        // labeled "current") should simply close the picker, not re-resume.
        if session_id == super::super::commands::active_session_id(self) {
            self.session_picker_overlay = None;
            self.session_picker_mode = SessionPickerMode::Resume;
            self.set_status_notice("Already in this session");
            return;
        }

        if crate::session_window::apply_resume_open_elsewhere(&session_id)
            == crate::session_window::OpenElsewhereOutcome::Moved
        {
            self.session_picker_overlay = None;
            self.session_picker_mode = SessionPickerMode::Resume;
            self.set_status_notice(format!("Brought {} here", name));
            return;
        }

        if targets.len() > 1 {
            self.push_display_message(DisplayMessage::system(format!(
                "Selected {} sessions; resuming {} in this terminal.",
                targets.len(),
                name
            )));
        }
        self.workspace_client.queue_resume_session(session_id);
        self.session_picker_overlay = None;
        self.session_picker_mode = SessionPickerMode::Resume;
        self.set_status_notice(format!("Switching → {}", name));
    }

    pub(super) fn handle_live_claude_takeover(&mut self, target: &ResumeTarget) -> bool {
        if super::super::commands_dispatch::ssh_local_action_blocked(self, "Local Claude takeover")
        {
            return false;
        }
        let ResumeTarget::ClaudeCodeSession { session_id, .. } = target else {
            self.push_display_message(DisplayMessage::error(
                "Live takeover is only available for Claude Code sessions.",
            ));
            return false;
        };
        let display_id = jcode_core::util::truncate_str(session_id, 8);
        self.set_status_notice(format!("Preparing Claude takeover → {display_id}"));

        let resolved = match crate::import::take_over_live_claude_session(target) {
            Ok(target) => target,
            Err(err) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Claude takeover failed: {err}"
                )));
                self.set_status_notice("Claude takeover did not complete");
                return false;
            }
        };
        let ResumeTarget::JcodeSession { session_id } = resolved else {
            self.push_display_message(DisplayMessage::error(
                "Claude takeover did not produce a Jcode session.",
            ));
            return false;
        };

        self.push_display_message(DisplayMessage::system(format!(
            "Claude Code exited and its transcript was prepared as {session_id}."
        )));
        self.workspace_client.queue_resume_session(session_id);
        self.session_picker_overlay = None;
        self.session_picker_mode = SessionPickerMode::Resume;
        self.set_status_notice(format!("Taking over Claude → {display_id}"));
        true
    }

    pub(in super::super) fn handle_batch_crash_restore(&mut self, session_ids: &[String]) {
        if super::super::commands_dispatch::ssh_local_action_blocked(self, "Local session recovery")
        {
            return;
        }
        let recovered = match crate::session::recover_crashed_sessions_by_ids(session_ids) {
            Ok(ids) => ids,
            Err(e) => {
                self.push_display_message(DisplayMessage::error(format!(
                    "Failed to recover crashed sessions: {}",
                    e
                )));
                return;
            }
        };

        if recovered.is_empty() {
            self.push_display_message(DisplayMessage::system(
                "No crashed sessions found in the selected restore group.".to_string(),
            ));
            return;
        }

        let exe = launch_client_executable();
        let cwd = std::env::current_dir().unwrap_or_default();
        let socket = std::env::var("JCODE_SOCKET").ok();
        let mut spawned = 0usize;
        let mut failed = Vec::new();

        for session_id in &recovered {
            let mut session_cwd = cwd.clone();
            if let Ok(session) = crate::session::Session::load_startup_stub(session_id)
                && let Some(dir) = session.working_dir.as_deref()
                && std::path::Path::new(dir).is_dir()
            {
                session_cwd = std::path::PathBuf::from(dir);
            }

            match spawn_in_new_terminal(&exe, session_id, &session_cwd, socket.as_deref()) {
                Ok(true) => spawned += 1,
                Ok(false) => failed.push(session_id.clone()),
                Err(e) => {
                    crate::logging::error(&format!(
                        "Failed to spawn session {}: {}",
                        session_id, e
                    ));
                    failed.push(session_id.clone());
                }
            }
        }

        // Single recovered session that could not get a new terminal: resume it
        // in the current terminal instead of forcing a manual command (#203).
        if spawned == 0 && recovered.len() == 1 && failed.len() == 1 {
            self.handle_session_picker_current_terminal_selection(&[ResumeTarget::JcodeSession {
                session_id: recovered[0].clone(),
            }]);
            return;
        }
        if spawned > 0 && failed.is_empty() {
            self.push_display_message(DisplayMessage::system(format!(
                "Restored {} crashed session(s) in new windows.",
                spawned
            )));
            self.set_status_notice(format!("Restored {} session(s)", spawned));
        } else if spawned > 0 {
            let manual: Vec<String> = failed
                .iter()
                .map(|id| format!("  jcode --resume {}", id))
                .collect();
            self.push_display_message(DisplayMessage::system(format!(
                "Restored {} session(s) in new windows. {} failed:\n{}",
                spawned,
                failed.len(),
                manual.join("\n")
            )));
        } else {
            let manual: Vec<String> = recovered
                .iter()
                .map(|id| format!("  jcode --resume {}", id))
                .collect();
            self.push_display_message(DisplayMessage::system(format!(
                "No terminal found. Resume manually:\n{}",
                manual.join("\n")
            )));
        }
    }

    pub(in super::super) fn handle_session_picker_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Result<()> {
        if super::super::commands_dispatch::ssh_local_action_blocked(self, "Local session picker") {
            self.session_picker_overlay = None;
            return Ok(());
        }
        let action = {
            let Some(picker_cell) = self.session_picker_overlay.as_ref() else {
                return Ok(());
            };
            let mut picker = picker_cell.borrow_mut();
            picker.handle_overlay_key(code, modifiers)?
        };
        self.handle_session_picker_action(action);
        Ok(())
    }

    pub(in super::super) fn poll_session_picker_search_submission(&mut self) -> bool {
        let Some(picker_cell) = self.session_picker_overlay.as_ref() else {
            return false;
        };
        let (pending, action) = {
            let mut picker = picker_cell.borrow_mut();
            (
                picker.search_submission_pending(),
                picker.poll_search_submission(),
            )
        };
        let Some(action) = action else {
            return pending;
        };
        self.handle_session_picker_action(action);
        true
    }

    pub(super) fn handle_session_picker_action(&mut self, action: OverlayAction) {
        match action {
            OverlayAction::Continue => {}
            OverlayAction::Close => {
                self.session_picker_overlay = None;
                if self.session_picker_mode == SessionPickerMode::Onboarding {
                    // Escaping the onboarding choice starts a clean new session.
                    self.session_picker_mode = SessionPickerMode::Resume;
                    self.onboarding_show_suggestions();
                } else {
                    self.session_picker_mode = SessionPickerMode::Resume;
                }
            }
            OverlayAction::Selected(result)
                if matches!(self.session_picker_mode, SessionPickerMode::Onboarding) =>
            {
                let ids = match result {
                    PickerResult::Selected(ids)
                    | PickerResult::SelectedInNewTerminal(ids)
                    | PickerResult::SelectedInCurrentTerminal(ids) => ids,
                    PickerResult::TakeOverClaude(target) => {
                        if self.handle_live_claude_takeover(&target) {
                            self.onboarding_finish();
                        }
                        return;
                    }
                    PickerResult::RestoreCrashedGroup(_) => Vec::new(),
                    PickerResult::StartNewSession => {
                        // User explicitly chose to start fresh; close the picker
                        // and show the onboarding suggestion cards.
                        self.session_picker_overlay = None;
                        self.session_picker_mode = SessionPickerMode::Resume;
                        self.onboarding_show_suggestions();
                        return;
                    }
                    PickerResult::ReviewRecentProject => {
                        self.session_picker_overlay = None;
                        self.session_picker_mode = SessionPickerMode::Resume;
                        self.onboarding_start_recent_project_review();
                        return;
                    }
                };
                self.session_picker_overlay = None;
                self.session_picker_mode = SessionPickerMode::Resume;
                if ids.is_empty() {
                    self.onboarding_show_suggestions();
                } else {
                    // Single-select: resume only the first chosen transcript.
                    self.handle_session_picker_current_terminal_selection(&ids[..1]);
                    self.onboarding_finish();
                }
            }
            OverlayAction::Selected(PickerResult::Selected(ids))
            | OverlayAction::Selected(PickerResult::SelectedInNewTerminal(ids)) => {
                self.handle_session_picker_selection(&ids);
                if let Some(picker_cell) = self.session_picker_overlay.as_ref() {
                    picker_cell.borrow_mut().clear_selected_sessions();
                }
            }
            OverlayAction::Selected(PickerResult::SelectedInCurrentTerminal(ids)) => {
                if self.session_picker_mode == SessionPickerMode::CatchUp {
                    self.handle_session_picker_selection(&ids);
                    if let Some(picker_cell) = self.session_picker_overlay.as_ref() {
                        picker_cell.borrow_mut().clear_selected_sessions();
                    }
                } else {
                    self.handle_session_picker_current_terminal_selection(&ids);
                }
            }
            OverlayAction::Selected(PickerResult::TakeOverClaude(target)) => {
                let _ = self.handle_live_claude_takeover(&target);
            }
            OverlayAction::Selected(PickerResult::RestoreCrashedGroup(session_ids)) => {
                self.handle_batch_crash_restore(&session_ids);
            }
            OverlayAction::Selected(PickerResult::StartNewSession) => {
                // Only the onboarding picker emits this, and that case is
                // handled by the onboarding arm above. Outside onboarding,
                // treat it as a no-op close.
                self.session_picker_overlay = None;
                self.session_picker_mode = SessionPickerMode::Resume;
            }
            OverlayAction::Selected(PickerResult::ReviewRecentProject) => {
                // Only the onboarding picker emits this. Outside onboarding,
                // close defensively without launching a proactive turn.
                self.session_picker_overlay = None;
                self.session_picker_mode = SessionPickerMode::Resume;
            }
        }
    }
}
