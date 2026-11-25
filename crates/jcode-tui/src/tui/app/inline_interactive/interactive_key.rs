use super::*;

impl App {
    pub(in super::super) fn handle_inline_interactive_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Result<()> {
        if self
            .inline_interactive_state
            .as_ref()
            .is_some_and(|picker| picker.kind == PickerKind::Account)
            && (matches!(code, KeyCode::Char('[' | ']'))
                || (modifiers.contains(KeyModifiers::CONTROL)
                    && key_char_eq_ignore_ascii_case(code, 'r')))
        {
            self.edit_selected_account_priority(code)?;
            return Ok(());
        }
        // `/agents` pickers: Ctrl+G switches between editing this session and
        // the global default, then reopens the same view in the other scope.
        if modifiers.contains(KeyModifiers::CONTROL)
            && key_char_eq_ignore_ascii_case(code, 'g')
            && let Some(view) = self
                .inline_interactive_state
                .as_ref()
                .and_then(agent_models_picker_view)
        {
            self.agent_models_global_scope = !self.agent_models_global_scope;
            match view {
                AgentModelsPickerView::Targets => self.open_agents_picker(),
                AgentModelsPickerView::Models(target) => self.open_agent_model_picker(target),
            }
            self.set_status_notice(if self.agent_models_global_scope {
                "/agents: editing the global default (all sessions)"
            } else {
                "/agents: editing this session only"
            });
            return Ok(());
        }
        match code {
            KeyCode::Esc => {
                if let Some(ref mut picker) = self.inline_interactive_state
                    && !picker.filter.is_empty()
                {
                    picker.filter.clear();
                    Self::apply_inline_interactive_filter(picker);
                    return Ok(());
                }
                // Inside the picker's effort step, Esc goes back to the models
                // it came from instead of closing the picker.
                if self.model_picker_effort_parent.is_some() {
                    self.close_model_picker_effort_step();
                    return Ok(());
                }
                self.inline_interactive_state = None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let vim_nav = self
                    .inline_interactive_state
                    .as_ref()
                    .map(|picker| picker.uses_compact_navigation())
                    .unwrap_or(false);
                if matches!(code, KeyCode::Char('k'))
                    && !modifiers.contains(KeyModifiers::CONTROL)
                    && !vim_nav
                {
                    if let Some(ref mut picker) = self.inline_interactive_state {
                        picker.filter.push('k');
                        Self::apply_inline_interactive_filter(picker);
                    }
                    return Ok(());
                }
                if let Some(ref mut picker) = self.inline_interactive_state {
                    if picker.column == 0 {
                        picker.selected = picker.selected.saturating_sub(1);
                    } else if let Some(&idx) = picker.filtered.get(picker.selected) {
                        let entry = &mut picker.entries[idx];
                        entry.selected_option = entry.selected_option.saturating_sub(1);
                    }
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let vim_nav = self
                    .inline_interactive_state
                    .as_ref()
                    .map(|picker| picker.uses_compact_navigation())
                    .unwrap_or(false);
                if matches!(code, KeyCode::Char('j'))
                    && !modifiers.contains(KeyModifiers::CONTROL)
                    && !vim_nav
                {
                    if let Some(ref mut picker) = self.inline_interactive_state {
                        picker.filter.push('j');
                        Self::apply_inline_interactive_filter(picker);
                    }
                    return Ok(());
                }
                if let Some(ref mut picker) = self.inline_interactive_state {
                    if picker.column == 0 {
                        let max = picker.filtered.len().saturating_sub(1);
                        picker.selected = (picker.selected + 1).min(max);
                    } else if let Some(&idx) = picker.filtered.get(picker.selected) {
                        let entry = &mut picker.entries[idx];
                        let max = entry.options.len().saturating_sub(1);
                        entry.selected_option = (entry.selected_option + 1).min(max);
                    }
                }
            }
            KeyCode::Right => {
                if let Some(ref mut picker) = self.inline_interactive_state {
                    if picker.uses_compact_navigation() {
                        return Ok(());
                    }
                    if picker.column < picker.max_navigable_column()
                        && let Some(&idx) = picker.filtered.get(picker.selected)
                        && (picker.entries[idx].options.len() > 1 || picker.column > 0)
                    {
                        picker.column += 1;
                    }
                }
            }
            KeyCode::BackTab => {
                if self
                    .inline_interactive_state
                    .as_ref()
                    .map(picker_is_runtime_model_picker)
                    .unwrap_or(false)
                {
                    self.cycle_selected_model_favorite();
                    return Ok(());
                }
                if let Some(ref mut picker) = self.inline_interactive_state {
                    if picker.uses_compact_navigation() {
                        return Ok(());
                    }
                    if picker.column > 0 {
                        picker.column -= 1;
                    }
                }
            }
            KeyCode::Left => {
                if let Some(ref mut picker) = self.inline_interactive_state {
                    if picker.uses_compact_navigation() {
                        return Ok(());
                    }
                    if picker.column > 0 {
                        picker.column -= 1;
                    }
                }
            }
            KeyCode::Tab => {
                if let Some(ref mut picker) = self.inline_interactive_state {
                    if picker.uses_compact_navigation() {
                        return Ok(());
                    }
                    if picker.column == 0 && !picker.filter.is_empty() {
                        Self::tab_complete_inline_interactive_filter(picker);
                    } else if picker.column < picker.max_navigable_column()
                        && let Some(&idx) = picker.filtered.get(picker.selected)
                        && (picker.entries[idx].options.len() > 1 || picker.column > 0)
                    {
                        picker.column += 1;
                    }
                }
            }
            code if modifiers.contains(KeyModifiers::CONTROL)
                && key_char_eq_ignore_ascii_case(code, 'o') =>
            {
                if super::super::commands_dispatch::ssh_local_action_blocked(
                    self,
                    "Saving a default model",
                ) {
                    return Ok(());
                }
                if let Some(ref picker) = self.inline_interactive_state {
                    if !picker_is_runtime_model_picker(picker) {
                        return Ok(());
                    }
                    if picker.filtered.is_empty() {
                        return Ok(());
                    }
                    let idx = picker.filtered[picker.selected];
                    let entry = &picker.entries[idx];
                    if !matches!(entry.action, PickerAction::Model) {
                        return Ok(());
                    }
                    let route = entry.options.get(entry.selected_option);

                    let bare_name = model_entry_base_name(entry);
                    let entry_effort = entry.effort.clone();

                    let (model_spec, provider_key) = if let Some(r) = route {
                        let selection =
                            crate::provider::MultiProvider::default_model_selection_from_route(
                                &bare_name,
                                &r.api_method,
                                &r.provider,
                            );
                        (selection.model_spec, selection.provider_key)
                    } else {
                        (bare_name.clone(), None)
                    };

                    let notice = format!(
                        "Default → {} via {}",
                        model_spec,
                        provider_key.as_deref().unwrap_or("auto")
                    );

                    match crate::auth::account_pool::set_default_model(
                        Some(&model_spec),
                        provider_key.as_deref(),
                    ) {
                        Ok(()) => {
                            // Persist the effort variant the user picked, so an
                            // effort-qualified entry (e.g. "Claude Opus 5 (high)")
                            // survives restarts instead of silently dropping the
                            // effort (issue #675).
                            if let Some(effort) = entry_effort.as_deref() {
                                let save_result = match provider_key.as_deref() {
                                    Some("claude-oauth") | Some("claude-api") => {
                                        Some(crate::config::Config::set_anthropic_reasoning_effort(
                                            Some(effort),
                                        ))
                                    }
                                    Some("openai-oauth") | Some("openai-api") => {
                                        Some(crate::config::Config::set_openai_reasoning_effort(
                                            Some(effort),
                                        ))
                                    }
                                    _ => None,
                                };
                                if let Some(Err(e)) = save_result {
                                    self.push_display_message(DisplayMessage::error(format!(
                                        "Saved default model, but failed to save effort: {}",
                                        e
                                    )));
                                }
                            }
                            self.invalidate_model_picker_cache();
                            if let Some(ref mut picker) = self.inline_interactive_state {
                                for entry in &mut picker.entries {
                                    entry.is_default = false;
                                }
                                if let Some(entry) = picker.entries.get_mut(idx) {
                                    entry.is_default = true;
                                }
                            }
                            self.push_display_message(DisplayMessage::system(format!(
                                "Saved default model: {} via {}. This affects future sessions.",
                                model_spec,
                                provider_key.as_deref().unwrap_or("auto")
                            )));
                            self.set_status_notice(notice)
                        }
                        Err(e) => self.set_status_notice(format!("Failed to save default: {}", e)),
                    }
                }
            }
            code if modifiers.contains(KeyModifiers::CONTROL)
                && key_char_eq_ignore_ascii_case(code, 'n') =>
            {
                self.toggle_selected_model_favorite();
            }
            KeyCode::Enter => {
                let Some(ref mut picker) = self.inline_interactive_state else {
                    return Ok(());
                };
                if picker.filtered.is_empty() {
                    return Ok(());
                }
                let idx = picker.filtered[picker.selected];
                let entry = picker.entries[idx].clone();

                if crate::tui::is_ssh_remote()
                    && !matches!(
                        entry.action,
                        PickerAction::Model
                            | PickerAction::Account(AccountPickerAction::Switch { .. })
                            | PickerAction::Usage { .. }
                            | PickerAction::RemoteLogin { .. }
                            | PickerAction::RemoteImportDecision { .. }
                    )
                {
                    self.inline_interactive_state = None;
                    super::super::commands_dispatch::ssh_local_action_blocked(
                        self,
                        "Local account or agent-model picker",
                    );
                    return Ok(());
                }

                if matches!(entry.action, PickerAction::Model) {
                    if picker.column == 0 && entry.options.len() > 1 {
                        picker.column = 1;
                        return Ok(());
                    }
                    if picker.column == 1 {
                        picker.column = picker.max_navigable_column();
                        return Ok(());
                    }
                }

                let route = &entry.options[entry.selected_option];

                if !route.available {
                    let detail = if route.detail.is_empty() {
                        "not available".to_string()
                    } else {
                        route.detail.clone()
                    };
                    self.inline_interactive_state = None;
                    self.set_status_notice(format!("{} - {}", entry.name, detail));
                    return Ok(());
                }

                match entry.action {
                    PickerAction::Account(selection) => {
                        self.inline_interactive_state = None;
                        self.handle_account_picker_selection(selection);
                    }
                    PickerAction::Login(provider) => {
                        self.inline_interactive_state = None;
                        self.start_login_provider(provider);
                    }
                    PickerAction::AutoApiKey => {
                        self.inline_interactive_state = None;
                        self.start_auto_api_key_login();
                    }
                    PickerAction::ExpandLoginProviders => {
                        if let Some(picker) = self.inline_interactive_state.as_mut() {
                            let target_row = picker.selected;
                            picker.entries.retain(|entry| {
                                !matches!(entry.action, PickerAction::ExpandLoginProviders)
                            });
                            picker.preview = false;
                            picker.column = 0;
                            Self::apply_inline_interactive_filter(picker);
                            if !picker.filtered.is_empty() {
                                picker.selected = target_row.min(picker.filtered.len() - 1);
                            }
                        }
                    }
                    PickerAction::RemoteLogin { provider, import } => {
                        self.inline_interactive_state = None;
                        self.select_ssh_login_action(provider, import);
                    }
                    PickerAction::RemoteImportDecision { accept } => {
                        self.select_ssh_import_decision(accept);
                    }
                    PickerAction::Logout(provider) => {
                        self.inline_interactive_state = None;
                        self.start_logout_provider(provider);
                    }
                    PickerAction::LogoutAll => {
                        self.inline_interactive_state = None;
                        self.start_logout_all();
                    }
                    PickerAction::Usage {
                        title,
                        subtitle,
                        status,
                        detail_lines,
                        ..
                    } => {
                        self.inline_interactive_state = None;
                        let mut content = vec![format!("# {}", title), subtitle];
                        content.push(format!("status: {}", status.label_for_display()));
                        content.extend(detail_lines);
                        self.push_display_message(DisplayMessage::usage(content.join("\n")));
                        self.set_status_notice(format!("Usage → {}", title));
                    }
                    PickerAction::AgentTarget(target) => {
                        self.open_agent_model_picker(target);
                    }
                    PickerAction::AgentModelChoice {
                        target,
                        clear_override,
                    } => {
                        self.inline_interactive_state = None;
                        let spec = (!clear_override).then(|| model_entry_saved_spec(&entry));
                        let result = if self.is_remote {
                            // The server owns both scopes in remote mode: global
                            // defaults go to its config, not this client's. Only
                            // AgentModelsChanged confirms persistence. Generic Ack
                            // is sent by the server before request dispatch.
                            self.set_status_notice(if self.agent_models_global_scope {
                                "Applying agent model [global]…"
                            } else {
                                "Applying agent model [session]…"
                            });
                            return Ok(());
                        } else if self.agent_models_global_scope {
                            save_agent_model_override(target, spec.as_deref())
                        } else {
                            self.session
                                .set_agent_model_override(agent_model_target_slug(target), spec)
                                .and_then(|()| self.session.save_prepared())
                        };
                        match result {
                            Ok(()) => {
                                let label = format!(
                                    "{} [{}]",
                                    agent_model_target_label(target),
                                    if self.agent_models_global_scope {
                                        "global"
                                    } else {
                                        "session"
                                    }
                                );
                                if clear_override {
                                    self.push_display_message(DisplayMessage::system(format!(
                                        "{} model override cleared. Using default `{}`.",
                                        label,
                                        agent_model_default_summary(target, self)
                                    )));
                                    self.set_status_notice(format!("{} model: inherit", label));
                                } else {
                                    let spec = model_entry_saved_spec(&entry);
                                    self.push_display_message(DisplayMessage::system(format!(
                                        "Saved {} model override: `{}`.",
                                        label, spec
                                    )));
                                    self.set_status_notice(format!("{} model → {}", label, spec));
                                }
                            }
                            Err(error) => {
                                self.push_display_message(DisplayMessage::error(format!(
                                    "Failed to save {} model override: {}",
                                    agent_model_target_label(target),
                                    error
                                )));
                                self.set_status_notice("Agent model save failed");
                            }
                        }
                    }
                    PickerAction::SubagentModelChoice { inherit } => {
                        self.inline_interactive_state = None;
                        if inherit {
                            self.session.set_subagent_model(None);
                            self.set_status_notice("Subagent model: inherit");
                            self.push_display_message(DisplayMessage::system(format!(
                                "Subagent model reset to inherit the current model ({}).",
                                self.provider.model()
                            )));
                        } else {
                            let spec = model_entry_saved_spec(&entry);
                            self.session.set_subagent_model(Some(spec.clone()));
                            self.set_status_notice(format!("Subagent model → {}", spec));
                            self.push_display_message(DisplayMessage::system(format!(
                                "Subagent model pinned to {} for this session.",
                                spec
                            )));
                        }
                        let _ = self.session.save();
                    }
                    PickerAction::Model => {
                        if !route.available {
                            self.push_display_message(DisplayMessage::error(
                                crate::tui::app::model_context::unavailable_model_route_message(
                                    &entry.name,
                                    &route.provider,
                                    &route.detail,
                                    self.is_remote,
                                ),
                            ));
                            self.set_status_notice("Model unavailable");
                            return Ok(());
                        }
                        // Step one of two: a model whose route accepts reasoning
                        // levels opens its level list instead of switching.
                        if model_picker_stage::opens_effort_step(&entry) {
                            self.open_model_picker_effort_step(&entry);
                            return Ok(());
                        }

                        let bare_name = model_entry_base_name(&entry);
                        let spec = if crate::provider::ModelRouteApiMethod::parse(&route.api_method)
                            .is_openrouter()
                            && route.provider == "auto"
                        {
                            openrouter_route_model_id(&bare_name)
                        } else {
                            picker_route_model_spec(&entry, route)
                        };
                        let route_selection = picker_route_selection(&entry, route);

                        let effort = entry.effort.clone();
                        record_model_picker_selection(&bare_name, route, effort.as_deref());
                        let method_label =
                            crate::provider::ModelRouteApiMethod::parse(&route.api_method)
                                .display_label();
                        // Placeholder routes ("remote-catalog"/"current") are
                        // catalog-refresh stand-ins, not real provider routes:
                        // surfacing them produced confusing notices like
                        // "Model → x via Claude (remote-catalog) · refreshing
                        // route details…" right before the real switch
                        // confirmation. Show just the model in that case.
                        let placeholder_route =
                            placeholder_routes::is_placeholder_route_method(&route.api_method);
                        let notice = if placeholder_route {
                            format!("Model → {}", entry.name)
                        } else {
                            format!(
                                "Model → {} via {} ({})",
                                entry.name, route.provider, method_label
                            )
                        };
                        let route_detail = if placeholder_route {
                            String::new()
                        } else {
                            route.detail.trim().to_string()
                        };

                        // Record exactly which model spec + route the user chose
                        // and how it will be applied. Pairs with the server-side
                        // model-switch logs so we can trace a `/model` choice all
                        // the way to the provider endpoint that ends up serving it
                        // (issues #292/#278: switch routes to wrong endpoint).
                        crate::logging::event_info(
                            "model_picker_select",
                            vec![
                                ("entry", entry.name.clone()),
                                ("spec", spec.clone()),
                                ("provider", route.provider.clone()),
                                ("api_method", route.api_method.clone()),
                                ("route_provider", route_selection.provider_label.clone()),
                                ("route_model", route_selection.model.clone()),
                                ("route_api_method", route_selection.api_method.clone()),
                                (
                                    "effort",
                                    effort.clone().unwrap_or_else(|| "none".to_string()),
                                ),
                                ("remote", self.is_remote.to_string()),
                            ],
                        );

                        if self.is_remote {
                            self.inline_interactive_state = None;
                            self.upstream_provider = None;
                            self.status_detail = None;
                            // Track the chosen method client-side so post-error
                            // fallback picks know which credential path the
                            // active route uses (remote sessions have no other
                            // route bookkeeping).
                            let mut route = self.session.route();
                            route.route_api_method = Some(route_selection.api_method.clone());
                            self.session.apply_route(route);
                            self.pending_route_selection = Some(route_selection);
                            self.pending_model_switch = Some(spec);
                            // In remote mode `self.provider` is a local
                            // stand-in, so applying the picked effort variant
                            // to it does not reach the server. Stage it so the
                            // remote dispatcher forwards it right after the
                            // model switch; otherwise the server keeps its
                            // configured default (low) and silently runs e.g.
                            // "gpt-5.5 (high)" at low effort (issue #427).
                            self.pending_reasoning_effort = effort.clone();
                        } else {
                            match self.provider.set_route_selection(&route_selection) {
                                Ok(()) => {
                                    self.inline_interactive_state = None;
                                    self.provider_session_id = None;
                                    self.session.forget_provider_session();
                                    self.upstream_provider = None;
                                    self.status_detail = None;
                                    self.invalidate_model_picker_cache();
                                    let active_model = self.provider.model();
                                    self.update_context_limit_for_model(&active_model, None);
                                    let mut route = self.session.route();
                                    route.provider_key = crate::provider::MultiProvider::session_provider_key_after_model_switch(
                                        &spec,
                                        self.provider.name(),
                                        self.session.provider_key(),
                                    );
                                    route.model = Some(active_model.clone());
                                    route.route_api_method =
                                        Some(route_selection.api_method.clone());
                                    self.session.apply_route(route);
                                    let _ = self.session.save();
                                    crate::logging::event_info(
                                        "model_picker_select_applied",
                                        vec![
                                            ("spec", spec.clone()),
                                            ("active_model", active_model),
                                            ("provider", self.provider.name().to_string()),
                                            ("api_method", route_selection.api_method.clone()),
                                        ],
                                    );
                                }
                                Err(error) => {
                                    crate::logging::event_error(
                                        "model_picker_select_failed",
                                        vec![
                                            ("spec", spec.clone()),
                                            ("provider", route.provider.clone()),
                                            ("api_method", route_selection.api_method.clone()),
                                            ("error", error.to_string()),
                                        ],
                                    );
                                    self.push_display_message(DisplayMessage::error(
                                        crate::tui::app::model_context::model_switch_failure_message(
                                            &error.to_string(),
                                            self.is_remote,
                                        ),
                                    ));
                                    self.set_status_notice("Model switch failed");
                                    return Ok(());
                                }
                            }
                        }
                        if let Some(effort) = effort {
                            let _ = self.provider.set_reasoning_effort(&effort);
                        }
                        if !self.is_remote && !route_detail.is_empty() {
                            self.push_display_message(DisplayMessage::system(format!(
                                "{}\n{}",
                                notice, route_detail
                            )));
                        }
                        self.set_status_notice(if route_detail.is_empty() {
                            notice
                        } else {
                            format!("{} · {}", notice, route_detail)
                        });
                        // First-run onboarding: a model choice advances the flow.
                        self.onboarding_after_model_select();
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(ref mut picker) = self.inline_interactive_state
                    && picker.filter.pop().is_some()
                {
                    Self::apply_inline_interactive_filter(picker);
                }
            }
            KeyCode::Char(c) => {
                if let Some(ref mut picker) = self.inline_interactive_state
                    // Spaces separate independently matched search terms. The
                    // token matcher already permits those terms to appear in
                    // any order, so dropping whitespace here made that
                    // capability impossible to use from the picker.
                    && (!c.is_whitespace()
                        || (!picker.filter.is_empty() && !picker.filter.ends_with(' ')))
                {
                    picker.filter.push(if c.is_whitespace() { ' ' } else { c });
                    Self::apply_inline_interactive_filter(picker);
                }
            }
            _ => {}
        }
        Ok(())
    }
}
