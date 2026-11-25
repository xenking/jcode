use super::*;

/// Render content for a specific widget type: body rows plus border text.
pub(super) fn render_widget_content(
    kind: WidgetKind,
    data: &InfoWidgetData,
    inner: Rect,
) -> Framed {
    let framed = match kind {
        // Image/map/paged widgets are handled specially in render_single_widget.
        WidgetKind::Diagrams | WidgetKind::WorkspaceMap | WidgetKind::Overview => {
            return Framed::default();
        }
        WidgetKind::MemoryActivity => render_memory_widget(data, inner),
        WidgetKind::SwarmStatus => render_swarm_widget(data, inner),
        WidgetKind::BackgroundTasks => render_background_widget(data, inner),
        WidgetKind::Compaction => render_compaction_widget(data, inner),
        WidgetKind::AmbientMode => render_ambient_widget(data, inner),
        WidgetKind::UsageLimits if data.usage_pending_at_startup() => Framed::body(
            startup_usage_lines(inner.width)
                .into_iter()
                .skip(1)
                .collect(),
        )
        .title(frame::label("OpenAI limits"))
        .footer(frame::dim("first snapshot")),
        WidgetKind::UsageLimits => render_usage_widget(data, inner),
        WidgetKind::KvCache => render_kv_cache_widget(data, inner),
        WidgetKind::ModelInfo => render_model_widget(data, inner),
        WidgetKind::Tips => render_tips_widget(inner),
        WidgetKind::GitStatus => render_changes_framed(data, inner),
        WidgetKind::CloudVm => super::cloud::render_cloud_widget(data, inner),
        WidgetKind::CloudDirectories => {
            super::cloud_directories::render_cloud_directories(data, inner)
        }
        WidgetKind::Commits => render_commits_widget(data, inner),
    };
    framed.settle()
}

pub(super) fn render_compaction_widget(data: &InfoWidgetData, inner: Rect) -> Framed {
    let Some(info) = data.compaction_info.as_ref() else {
        return Framed::default();
    };
    let title_color = if info.is_compacting {
        rgb(255, 220, 140)
    } else {
        rgb(110, 210, 140)
    };
    let status = if info.is_compacting {
        "compacting"
    } else {
        "compacted"
    };
    let summary_tokens = (info.summary_chars / crate::compaction::CHARS_PER_TOKEN)
        .max(usize::from(info.summary_chars > 0));
    let detail = format!(
        "{} old · {} active",
        info.compacted_messages, info.active_messages
    );
    let summary = format!(
        "~{} tok",
        crate::kv_cache_monitor::format_token_count(summary_tokens as u64)
    );
    let footer = if info.mode.trim().is_empty() {
        summary
    } else {
        format!("{summary} · {}", info.mode.trim())
    };
    Framed::body(vec![Line::from(Span::styled(
        clip(&detail, inner.width as usize),
        Style::default().fg(rgb(180, 180, 190)),
    ))])
    .title(Line::from(vec![
        frame::label("Compaction "),
        Span::styled(status, Style::default().fg(title_color).bold()),
    ]))
    .footer(frame::dim(footer))
}
