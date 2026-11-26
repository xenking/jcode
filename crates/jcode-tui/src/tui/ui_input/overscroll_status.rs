use super::*;

/// `cache 47m` on the pinned status line. Quiet while the provider's prompt
/// cache is warm, warm itself when the retention window is nearly over, and
/// dim once it has lapsed, so the glassy "why did this turn cost so much"
/// question is answered before it happens. Estimated windows get a `~`: the
/// provider may evict earlier than the number says.
pub(crate) fn overscroll_cache_spans(
    info: &crate::tui::CacheTtlInfo,
) -> Option<Vec<Span<'static>>> {
    let text = if info.is_cold {
        "󰆼 0s".to_string()
    } else {
        let remaining = info.remaining_secs;
        let span = if remaining >= 60 {
            format!("{}m", remaining.div_ceil(60))
        } else {
            format!("{remaining}s")
        };
        let tilde = if info.is_estimate { "~" } else { "" };
        format!("󰆼 {tilde}{span}")
    };
    let ink = if info.is_cold {
        rgb(100, 100, 110)
    } else if info.expiring_soon() {
        rgb(255, 193, 7)
    } else {
        rgb(140, 140, 150)
    };
    Some(vec![Span::styled(text, Style::default().fg(ink))])
}

/// Draw the session status line, pinned directly below the input. Shows the
/// directory, branch, git status, context usage, access method, provider, and
/// model, compacting facts step by step as the terminal narrows.
pub(in super::super) fn draw_overscroll_status(frame: &mut Frame, app: &dyn TuiState, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let data = app.info_widget_data();

    let model = data
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| app.provider_model());
    let provider = data
        .provider_name
        .clone()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| app.provider_name());
    let facts = OverscrollFacts {
        machine: app.remote_machine(),
        dir: overscroll_directory_transition(
            data.original_working_dir.as_deref(),
            app.working_dir().as_deref(),
        ),
        branch: overscroll_git_branch(&data),
        git: overscroll_git_info(&data).cloned(),
        context: overscroll_context_usage(&data),
        account_remaining: overscroll_account_remaining(&data),
        account_windows: if app.remote_machine().is_some()
            && !crate::usage::remote_account_usage_received()
        {
            None
        } else {
            overscroll_multi_usage_windows(&super::super::header::status_usage_accounts(
                app.remote_machine().is_some(),
            ))
            .or_else(|| overscroll_usage_windows(&data))
        },
        cache: app.cache_ttl_status(),
        auth: overscroll_auth_label(data.auth_method),
        provider: (!provider.is_empty() && !overscroll_is_runtime_placeholder(&provider))
            .then(|| overscroll_provider_display(&provider)),
        model: (!model.is_empty() && !overscroll_is_placeholder(&model))
            .then(|| session_facts::pretty_model(&model)),
        effort: data
            .reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|effort| !effort.is_empty())
            .map(str::to_string),
    };

    let alignment = if app.centered_mode() {
        Alignment::Center
    } else {
        Alignment::Right
    };
    let (spans, _) = overscroll_fit_facts(&facts, area.width as usize);
    if spans.is_empty() {
        return;
    }
    let line = Line::from(spans).alignment(alignment);
    frame.render_widget(Paragraph::new(line), area);
}

/// Render only the supplied session facts. Unlike the live adapter this never
/// falls back to the current App, process directory, SSH host or credentials.
#[cfg(test)]
pub(crate) fn preview_overscroll_line(
    data: &crate::tui::info_widget::InfoWidgetData,
    width: u16,
) -> Line<'static> {
    preview_overscroll_line_for_host(data, width, None)
}

/// The host belongs to the selected preview, never the attached live session.
#[cfg(test)]
pub(crate) fn preview_overscroll_line_for_host(
    data: &crate::tui::info_widget::InfoWidgetData,
    width: u16,
    host: Option<&str>,
) -> Line<'static> {
    let facts = OverscrollFacts {
        machine: host
            .filter(|host| !host.trim().is_empty())
            .map(str::to_owned),
        dir: overscroll_directory_transition(
            data.original_working_dir.as_deref(),
            data.working_dir.as_deref(),
        ),
        branch: overscroll_git_branch(data),
        git: overscroll_git_info(data).cloned(),
        context: overscroll_context_usage(data),
        account_remaining: overscroll_account_remaining(data),
        account_windows: overscroll_usage_windows(data),
        cache: None,
        auth: overscroll_auth_label(data.auth_method),
        provider: data
            .provider_name
            .as_ref()
            .filter(|name| !name.is_empty() && !overscroll_is_runtime_placeholder(name))
            .map(|name| overscroll_provider_display(name)),
        model: data
            .model
            .as_ref()
            .filter(|name| !name.is_empty() && !overscroll_is_placeholder(name))
            .map(|name| session_facts::pretty_model(name)),
        effort: data
            .reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|effort| !effort.is_empty())
            .map(str::to_string),
    };
    let label = if data.historical_context {
        "saved · "
    } else {
        ""
    };
    let label_width = unicode_width::UnicodeWidthStr::width(label);
    let mut spans = overscroll_fit_facts(&facts, (width as usize).saturating_sub(label_width)).0;
    if !label.is_empty() && width as usize > label_width {
        spans.insert(0, Span::styled(label, Style::default().fg(dim_color())));
    }
    Line::from(spans).alignment(Alignment::Right)
}

/// Model name color on the overscroll status line.
pub(super) const OVERSCROLL_MODEL_PINK: Color = Color::Rgb(255, 135, 200);

/// Raw facts for the overscroll status line, before width fitting.
pub(super) struct OverscrollFacts {
    /// Remote machine this session runs on (cloud VM or SSH host).
    pub(super) machine: Option<String>,
    pub(super) dir: Option<String>,
    pub(super) branch: Option<String>,
    pub(super) git: Option<crate::tui::info_widget::GitInfo>,
    pub(super) context: Option<(usize, usize)>,
    /// Prompt-cache countdown, when the provider documents a retention window.
    pub(super) cache: Option<crate::tui::CacheTtlInfo>,
    pub(super) account_remaining: Option<u8>,
    pub(super) account_windows: Option<String>,
    pub(super) auth: Option<(&'static str, Color)>,
    pub(super) provider: Option<String>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
}

/// Per-fact detail level. 0 is the fullest form. Higher levels are more
/// compact, and the last level of each optional fact hides it.
#[derive(Clone, Copy, Default)]
pub(super) struct OverscrollLevels {
    pub(super) machine: u8,
    pub(super) dir: u8,
    pub(super) branch: u8,
    pub(super) git: u8,
    pub(super) context: u8,
    pub(super) cache: u8,
    pub(super) account_bar: u8,
    pub(super) account_windows: u8,
    pub(super) context_bar: u8,
    pub(super) auth: u8,
    pub(super) provider: u8,
    pub(super) model: u8,
}

/// Compaction ladder, applied one step at a time until the line fits.
///
/// Priority (most to least important): directory, model, context usage,
/// git branch, git status, provider, auth. Low-value facts are shortened or
/// hidden first. The directory, model, and context usage are never hidden,
/// only shortened, so the line always answers "where am I, what am I running,
/// how full is the context".
pub(super) const OVERSCROLL_LADDER: &[fn(&mut OverscrollLevels)] = &[
    |l| l.account_windows = 1,
    |l| l.account_bar = 1, // drop account bar before shortening other facts
    |l| l.context_bar = 1, // keep token counts when the context bar cannot fit
    |l| l.auth = 1,        // hide auth ("OAuth"/"API key")
    |l| l.cache = 1,       // hide the prompt-cache countdown
    |l| l.git = 1,         // "~3 +1 ?2 ↑1" -> "±6 ↑1"
    |l| l.branch = 1,      // long branch -> 12 chars
    |l| l.provider = 1,    // hide provider
    |l| l.context = 1,     // "74k/256k 29%" -> "29%"
    |l| l.git = 2,         // hide git status
    |l| l.branch = 2,      // hide branch
    |l| l.model = 1,       // drop reasoning effort
    |l| l.machine = 1,     // "☁ jcode-cloud:" -> "☁ "
    |l| l.dir = 1,         // "~/…/jcode" -> "jcode"
];

pub(super) fn overscroll_fact_spans(
    facts: &OverscrollFacts,
    levels: OverscrollLevels,
    show_resets: bool,
) -> Vec<Vec<Span<'static>>> {
    let muted = Style::default().fg(rgb(140, 140, 150));
    let mut out: Vec<Vec<Span<'static>>> = Vec::new();

    let machine_spans = |levels: OverscrollLevels| -> Vec<Span<'static>> {
        let Some(machine) = &facts.machine else {
            return Vec::new();
        };
        let cloud = Style::default().fg(rgb(170, 200, 255));
        let mut spans = vec![Span::styled("☁ ", cloud)];
        if levels.machine == 0 {
            spans.push(Span::styled(
                overscroll_truncate_label(machine, 24),
                cloud.bold(),
            ));
            if facts.dir.is_some() {
                spans.push(Span::styled(":", muted));
            }
        }
        spans
    };

    if let Some(dir) = &facts.dir {
        let label = if levels.dir == 0 {
            dir.clone()
        } else {
            dir.split(" → ")
                .map(|path| path.rsplit('/').find(|seg| !seg.is_empty()).unwrap_or(path))
                .collect::<Vec<_>>()
                .join(" → ")
        };
        let mut group = machine_spans(levels);
        if group.is_empty() {
            group.push(Span::styled(" ", Style::default().fg(rgb(140, 180, 255))));
        }
        group.push(Span::styled(label, muted));
        out.push(group);
    } else if facts.machine.is_some() {
        out.push(machine_spans(levels));
    }

    if let Some(branch) = &facts.branch {
        let label = match levels.branch {
            0 => Some(branch.clone()),
            1 => Some(overscroll_truncate_label(branch, 12)),
            _ => None,
        };
        if let Some(label) = label {
            out.push(vec![Span::styled(
                format!(" {label}"),
                Style::default().fg(rgb(150, 170, 140)),
            )]);
        }
    }

    if let Some(git) = &facts.git
        && let Some(spans) = overscroll_git_status_spans(git, levels.git)
    {
        out.push(spans);
    }

    if let Some((used, limit)) = facts.context {
        out.push(vec![
            Span::styled("󰍛 ", muted),
            overscroll_context_percentage(used, limit),
        ]);
    }

    if levels.cache == 0
        && let Some(info) = &facts.cache
        && let Some(spans) = overscroll_cache_spans(info)
    {
        out.push(spans);
    }

    if levels.provider == 0 && levels.account_windows == 0 {
        if let Some(windows) = &facts.account_windows {
            let mut group = Vec::new();
            for (i, word) in windows
                .split_whitespace()
                .filter(|word| {
                    show_resets
                        || !word
                            .strip_suffix(['d', 'h', 'm'])
                            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                })
                .enumerate()
            {
                if i > 0 {
                    group.push(Span::raw(" "));
                }
                let color = word
                    .strip_suffix('%')
                    .and_then(|s| s.parse::<u8>().ok())
                    .map(overscroll_usage_color)
                    .unwrap_or(rgb(140, 140, 150));
                group.push(Span::styled(word.to_owned(), Style::default().fg(color)));
            }
            out.push(group);
        } else if let (Some(provider), Some(remaining)) = (&facts.provider, facts.account_remaining)
        {
            out.push(vec![
                Span::styled(format!("{} ", overscroll_provider_glyph(provider)), muted),
                overscroll_percent(100 - remaining),
            ]);
        }
    }
    let _ = facts.auth;

    if let Some(model) = &facts.model {
        let mut group = vec![Span::styled(
            model.clone(),
            Style::default().fg(OVERSCROLL_MODEL_PINK).bold(),
        )];
        if levels.model == 0
            && let Some(effort) = &facts.effort
            && !overscroll_model_already_includes_effort(model, effort)
        {
            let text = overscroll_short_reasoning(effort).unwrap_or_default();
            group.push(Span::styled(
                format!(" {text}"),
                Style::default().fg(effort_ink(effort)),
            ));
        }
        out.push(group);
    }

    out
}

pub(super) fn overscroll_is_standard_effort_word(word: &str) -> bool {
    let trimmed = word.trim_matches(|c: char| matches!(c, '(' | ')' | '[' | ']'));
    matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    )
}

pub(super) fn overscroll_model_already_includes_effort(model: &str, effort: &str) -> bool {
    let effort = effort.trim();
    if effort.is_empty() {
        return false;
    }
    let Some(last_word) = model.split_whitespace().last() else {
        return false;
    };
    if last_word.eq_ignore_ascii_case(effort) {
        return true;
    }
    // Models that encode their own thinking/effort level in their name (e.g.
    // "Gemini 4 Neon Low") should never render a second standard effort suffix
    // like "Gemini 4 Neon Low medium". Non-standard sentinels like "swarm"
    // remain visible.
    overscroll_is_standard_effort_word(last_word) && overscroll_is_standard_effort_word(effort)
}

/// Fit the facts into `max_width` by walking the compaction ladder. Returns
/// the spans and whether they fit without last-resort character truncation.
pub(super) fn overscroll_fit_facts(
    facts: &OverscrollFacts,
    max_width: usize,
) -> (Vec<Span<'static>>, bool) {
    use unicode_width::UnicodeWidthStr;
    let render = |levels: OverscrollLevels| {
        let mut out: Vec<Span<'static>> = Vec::new();
        for group in overscroll_fact_spans(facts, levels, max_width >= 120) {
            if !out.is_empty() {
                out.push(Span::raw(" "));
            }
            out.extend(group);
        }
        compact_status_fit(Line::from(out), usize::MAX).spans
    };
    let width = |spans: &[Span<'static>]| spans.iter().map(|s| s.content.width()).sum::<usize>();

    let mut levels = OverscrollLevels::default();
    let mut spans = render(levels);
    for step in OVERSCROLL_LADDER {
        if width(&spans) <= max_width {
            return (spans, true);
        }
        step(&mut levels);
        spans = render(levels);
    }
    if width(&spans) <= max_width {
        return (spans, true);
    }
    (overscroll_truncate_spans(spans, max_width), false)
}

pub(super) fn overscroll_truncate_label(label: &str, max_width: usize) -> String {
    compact_status_fit(Line::from(label.to_owned()), max_width).to_string()
}

/// Truncate a list of spans to at most `max_width` display columns, appending a
/// single-cell ellipsis when content is dropped. Preserves per-span styling.
pub(super) fn overscroll_truncate_spans(
    spans: Vec<Span<'static>>,
    max_width: usize,
) -> Vec<Span<'static>> {
    compact_status_fit(Line::from(spans), max_width).spans
}

pub(super) fn compact_status_fit(mut line: Line<'static>, width: usize) -> Line<'static> {
    for span in &mut line.spans {
        if span.content.chars().any(char::is_control) {
            span.content = span
                .content
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect::<String>()
                .into();
        }
    }
    jcode_tui_render::truncate_line_with_ellipsis_to_width(&line, width)
}

/// Format a working dir path home-relative (~/foo/bar), keeping the last 2 segments.
/// Git facts for the status line. Cloud sessions keep their repository in
/// `dual_git_info.cloud`, so prefer that over the (absent) local `git_info`.
pub(super) fn overscroll_git_info(
    data: &crate::tui::info_widget::InfoWidgetData,
) -> Option<&crate::tui::info_widget::GitInfo> {
    if let Some(dual) = &data.dual_git_info
        && let crate::tui::cloud_git::RepoState::Ready(info) = &dual.cloud
    {
        return Some(info);
    }
    data.git_info.as_ref()
}

/// Compact git branch label for the status line and fact stack. Truncated so
/// long branch names cannot crowd out the other facts.
pub(super) fn overscroll_git_branch(
    data: &crate::tui::info_widget::InfoWidgetData,
) -> Option<String> {
    let branch = overscroll_git_info(data)?.branch.trim();
    if branch.is_empty() {
        return None;
    }
    Some(overscroll_truncate_label(branch, 24))
}

/// Git status counts using the git widget palette. Level 0 is the full
/// `~modified +staged ?untracked ↑ahead ↓behind` form, level 1 collapses the
/// local changes into a single `±N` count, and level 2 hides it. `None` when
/// the tree is clean and in sync.
pub(super) fn overscroll_git_status_spans(
    info: &crate::tui::info_widget::GitInfo,
    level: u8,
) -> Option<Vec<Span<'static>>> {
    let parts: Vec<(usize, &str, Color)> = match level {
        0 => vec![
            (info.modified, "~", rgb(240, 200, 80)),
            (info.staged, "+", rgb(100, 200, 100)),
            (info.untracked, "?", rgb(140, 140, 150)),
            (info.ahead, "↑", rgb(100, 200, 100)),
            (info.behind, "↓", rgb(255, 140, 100)),
        ],
        1 => vec![
            (
                info.modified + info.staged + info.untracked,
                "±",
                rgb(240, 200, 80),
            ),
            (info.ahead, "↑", rgb(100, 200, 100)),
            (info.behind, "↓", rgb(255, 140, 100)),
        ],
        _ => return None,
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (count, symbol, color) in parts {
        if count == 0 {
            continue;
        }
        if !spans.is_empty() {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            format!("{symbol}{count}"),
            Style::default().fg(color),
        ));
    }
    (!spans.is_empty()).then_some(spans)
}

pub(super) fn overscroll_dir_label(path: &str) -> Option<String> {
    session_facts::dir_label_short(path)
}

pub(super) fn overscroll_directory_transition(
    original: Option<&str>,
    current: Option<&str>,
) -> Option<String> {
    let current = current.filter(|path| !path.trim().is_empty())?;
    let label = overscroll_dir_label(current).unwrap_or_else(|| current.to_string());
    let Some(original) = original.filter(|path| !path.trim().is_empty()) else {
        return Some(label);
    };
    if std::path::Path::new(original) == std::path::Path::new(current) {
        return Some(label);
    }
    let origin_label = overscroll_dir_label(original).unwrap_or_else(|| original.to_string());
    // Elision can hide the differing ancestor. Keep both paths distinguishable.
    if origin_label == label {
        Some(format!("{original} → {current}"))
    } else {
        Some(format!("{origin_label} → {label}"))
    }
}

/// Placeholder header strings used during remote startup; not real model names.
pub(super) fn overscroll_is_placeholder(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    m.starts_with("connecting")
        || m.starts_with("loading")
        || m == "connected"
        || m.contains("connecting to server")
}

/// Inert runtime provider labels (remote/replay/test-harness) shown before the
/// real provider name is known; not real provider names.
pub(super) fn overscroll_is_runtime_placeholder(provider: &str) -> bool {
    matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "remote" | "replay" | "test-harness"
    )
}

pub(super) fn overscroll_provider_display(provider: &str) -> String {
    match provider.to_ascii_lowercase().as_str() {
        // Keep provider labels credential-neutral: the adjacent auth chip
        // (`overscroll_auth_label`) reports OAuth vs API key from the canonical
        // credential resolution. Baking a credential into the provider name
        // used to produce contradictions like "Claude OAuth · API key" when
        // the Anthropic route was pinned to the API key.
        "claude" => "Claude".to_string(),
        "anthropic" => "Anthropic".to_string(),
        "openai" => "OpenAI".to_string(),
        "openrouter" => "OpenRouter".to_string(),
        "opencode" => "OpenCode".to_string(),
        "gemini" => "Gemini".to_string(),
        "copilot" => "GitHub Copilot".to_string(),
        "cursor" => "Cursor".to_string(),
        "bedrock" => "AWS Bedrock".to_string(),
        "antigravity" => "Antigravity".to_string(),
        _ => provider.to_string(),
    }
}

pub(super) fn overscroll_auth_label(
    method: crate::tui::info_widget::AuthMethod,
) -> Option<(&'static str, Color)> {
    use crate::tui::info_widget::AuthMethod;
    match method {
        AuthMethod::Unknown => None,
        AuthMethod::ApiKey | AuthMethod::AnthropicApiKey | AuthMethod::OpenAIApiKey => {
            Some(("API key", rgb(180, 180, 190)))
        }
        AuthMethod::OpenRouterApiKey | AuthMethod::OpenCodeApiKey => {
            Some(("API key", rgb(140, 180, 255)))
        }
        AuthMethod::AnthropicOAuth => Some(("OAuth", rgb(255, 160, 100))),
        AuthMethod::OpenAIOAuth => Some(("OAuth", rgb(100, 200, 180))),
        AuthMethod::CopilotOAuth => Some(("OAuth", rgb(110, 200, 140))),
        AuthMethod::GeminiOAuth => Some(("OAuth", rgb(120, 190, 255))),
    }
}

/// One glyph for the pinned status line's reasoning level, the ladder every
/// other surface draws. Swarm is an orchestration mode rather than a rung, so
/// it keeps its word.
pub(super) fn overscroll_short_reasoning(effort: &str) -> Option<&str> {
    let effort = effort.trim();
    if effort.is_empty() {
        return None;
    }
    if crate::prompt::is_swarm_mode_effort(effort) {
        return Some(crate::tui::effort_labels::effort_display_label(effort));
    }
    match jcode_provider_core::reasoning_effort_glyph(effort) {
        Some(glyph) => Some(glyph),
        // An unknown value still beats silence: show what the provider said.
        None => Some(effort),
    }
}

/// Glyph ink: the level's position on the shared ladder blends quiet chrome
/// ink toward the accent, so a stronger level reads brighter.
pub(super) fn effort_ink(effort: &str) -> Color {
    let top = jcode_provider_core::REASONING_EFFORT_MAX_RANK.max(1) as f32;
    let t = jcode_provider_core::reasoning_effort_rank(effort)
        .map(|rank| rank as f32 / top)
        .unwrap_or(0.0);
    effort_ramp(t)
}

pub(super) fn overscroll_context_usage(
    data: &crate::tui::info_widget::InfoWidgetData,
) -> Option<(usize, usize)> {
    let used_tokens = if let Some(observed) = data.observed_context_tokens {
        observed as usize
    } else if let Some(reported) = data.last_reported_context_tokens {
        // A resumed client has no local transcript, but History restores the
        // last prompt size the provider reported.
        reported as usize
    } else {
        let info = data.context_info.as_ref()?;
        if info.total_chars == 0 {
            return None;
        }
        info.estimated_tokens(data.token_family())
    };
    let limit = data
        .compaction_threshold_tokens
        .filter(|threshold| *threshold > 0)
        .or(data.context_limit)
        .unwrap_or(crate::provider::DEFAULT_CONTEXT_LIMIT)
        .max(1);
    Some((used_tokens, limit))
}

/// The tightest subscription window determines the remaining account allowance.
/// Never turn missing usage or an API-key cost total into a fictional quota.
pub(super) fn overscroll_account_remaining(
    data: &crate::tui::info_widget::InfoWidgetData,
) -> Option<u8> {
    use crate::tui::info_widget::{AuthMethod, UsageProvider};
    let info = data.usage_info.as_ref()?;
    if !matches!(
        (data.auth_method, info.provider),
        (AuthMethod::OpenAIOAuth, UsageProvider::OpenAI)
            | (AuthMethod::AnthropicOAuth, UsageProvider::Anthropic)
    ) {
        return None;
    }
    if info.limit_reached {
        return Some(0);
    }
    if !info.available
        || !info.five_hour.is_finite()
        || !info.seven_day.is_finite()
        || info.spark.is_some_and(|value| !value.is_finite())
    {
        return None;
    }
    Some(100u8.saturating_sub(info.max_usage_pct()))
}

pub(super) fn overscroll_usage_windows(
    data: &crate::tui::info_widget::InfoWidgetData,
) -> Option<String> {
    overscroll_account_remaining(data)?;
    let info = data.usage_info.as_ref()?;
    if !info.available {
        return None;
    }
    let windows = [
        (
            &info.primary_limit_label,
            info.five_hour,
            &info.five_hour_resets_at,
        ),
        (
            &info.secondary_limit_label,
            info.seven_day,
            &info.seven_day_resets_at,
        ),
    ];
    let parts: Vec<String> = windows
        .into_iter()
        .filter_map(|(label, ratio, reset)| {
            label.as_deref()?;
            if !ratio.is_finite() {
                return None;
            }
            let reset = reset
                .as_deref()
                .and_then(overscroll_short_reset)
                .map(|value| format!(" {value}"))
                .unwrap_or_default();
            let used = (ratio.clamp(0., 1.) * 100.).round() as u8;
            Some(format!("{}{reset}", overscroll_usage_percentage(used)))
        })
        .collect();
    (!parts.is_empty()).then(|| {
        format!(
            "{} {}",
            overscroll_provider_glyph(match info.provider {
                crate::tui::info_widget::UsageProvider::Anthropic => "Claude",
                crate::tui::info_widget::UsageProvider::OpenAI => "Codex",
                _ => "",
            }),
            parts.join(" ")
        )
    })
}

pub(super) fn overscroll_usage_percentage(used: u8) -> String {
    format!("{used}%")
}

pub(super) fn overscroll_provider_glyph(provider: &str) -> &'static str {
    match provider.to_ascii_lowercase().as_str() {
        "claude" | "anthropic" => "\u{ec82}",
        "codex" | "openai" => "\u{ec81}",
        _ => "󰚩",
    }
}

pub(super) fn overscroll_short_reset(at: &str) -> Option<String> {
    let reset = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    let seconds = reset
        .signed_duration_since(chrono::Utc::now())
        .num_seconds()
        .max(0);
    Some(if seconds >= 86_400 {
        format!("{}d", seconds / 86_400)
    } else if seconds >= 3600 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}m", (seconds + 59) / 60)
    })
}

pub(super) fn overscroll_multi_usage_windows(
    accounts: &[(&str, crate::usage::AccountUsageSnapshot)],
) -> Option<String> {
    let providers: Vec<String> = accounts
        .iter()
        .filter_map(|(provider, account)| {
            if account.error.is_some() {
                return None;
            }
            let windows = [
                (&account.primary_label, account.five_hour_ratio),
                (&account.secondary_label, account.seven_day_ratio),
            ];
            let parts: Vec<_> = windows
                .into_iter()
                .filter_map(|(label, ratio)| {
                    let ratio =
                        ratio.filter(|ratio| ratio.is_finite() && (0.0..=1.0).contains(ratio))?;
                    let used = (ratio * 100.0).round() as u8;
                    label.as_deref()?;
                    Some(overscroll_usage_percentage(used))
                })
                .collect();
            let reset = (parts.len() == 1)
                .then_some(account.resets_at.as_deref())
                .flatten()
                .and_then(overscroll_short_reset)
                .map(|reset| format!(" {reset}"))
                .unwrap_or_default();
            (!parts.is_empty()).then(|| {
                format!(
                    "{} {}{reset}",
                    overscroll_provider_glyph(provider),
                    parts.join(" ")
                )
            })
        })
        .collect();
    (!providers.is_empty()).then(|| providers.join(" · "))
}

/// Format context counts compactly while retaining useful precision.
#[cfg(test)]
pub(super) fn overscroll_format_tokens(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 10_000 {
        format!("{}k", tokens / 1000)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        tokens.to_string()
    }
}

/// Render the colored percentage alongside the context token counts.
pub(super) fn overscroll_context_percentage(used: usize, limit: usize) -> Span<'static> {
    let limit = limit.max(1);
    let ratio = (used as f64 / limit as f64).clamp(0.0, 1.0);
    let pct = (ratio * 100.0).round() as u16;

    overscroll_percent(pct as u8)
}

pub(super) fn overscroll_usage_color(used: u8) -> Color {
    if used >= 80 {
        rgb(255, 100, 100)
    } else if used >= 50 {
        rgb(255, 200, 100)
    } else {
        rgb(140, 140, 150)
    }
}

pub(super) fn overscroll_percent(used: u8) -> Span<'static> {
    Span::styled(
        format!("{used}%"),
        Style::default().fg(overscroll_usage_color(used)),
    )
}
