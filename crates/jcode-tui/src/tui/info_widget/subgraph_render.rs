use super::*;

#[cfg(test)]
#[derive(Debug, Clone)]
pub(super) struct MemorySubgraph {
    pub(super) nodes: Vec<GraphNode>,
    pub(super) _edges: Vec<GraphEdge>,
}
#[cfg(test)]
pub(super) fn select_contextual_subgraph(
    info: &MemoryInfo,
    max_nodes: usize,
    max_edges: usize,
) -> Option<MemorySubgraph> {
    if info.graph_nodes.is_empty() || max_nodes == 0 {
        return None;
    }
    let node_count = info.graph_nodes.len();
    let center_idx = pick_subgraph_center(info)?;
    let mut neighbors: Vec<Vec<(usize, usize)>> = vec![Vec::new(); node_count];
    for (edge_idx, edge) in info.graph_edges.iter().enumerate() {
        if edge.source >= node_count || edge.target >= node_count {
            continue;
        }
        neighbors[edge.source].push((edge.target, edge_idx));
        neighbors[edge.target].push((edge.source, edge_idx));
    }
    let mut selected = Vec::with_capacity(max_nodes.min(node_count));
    let mut selected_set: HashSet<usize> = HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    selected.push(center_idx);
    selected_set.insert(center_idx);
    queue.push_back(center_idx);
    while let Some(current) = queue.pop_front() {
        if selected.len() >= max_nodes {
            break;
        }
        let mut ranked = neighbors[current].clone();
        ranked.sort_by(|(a_idx, a_edge), (b_idx, b_edge)| {
            edge_kind_priority(&info.graph_edges[*b_edge].kind)
                .cmp(&edge_kind_priority(&info.graph_edges[*a_edge].kind))
                .then_with(|| {
                    graph_node_score(&info.graph_nodes[*b_idx])
                        .partial_cmp(&graph_node_score(&info.graph_nodes[*a_idx]))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a_idx.cmp(b_idx))
        });
        for (next_idx, _) in ranked {
            if selected.len() >= max_nodes {
                break;
            }
            if selected_set.insert(next_idx) {
                selected.push(next_idx);
                queue.push_back(next_idx);
            }
        }
    }

    if selected.len() < max_nodes {
        let mut remaining: Vec<usize> = (0..node_count)
            .filter(|idx| !selected_set.contains(idx))
            .collect();
        remaining.sort_by(|a, b| {
            graph_node_score(&info.graph_nodes[*b])
                .partial_cmp(&graph_node_score(&info.graph_nodes[*a]))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cmp(b))
        });
        for idx in remaining {
            if selected.len() >= max_nodes {
                break;
            }
            selected_set.insert(idx);
            selected.push(idx);
        }
    }

    let mut old_to_new = HashMap::new();
    let mut sub_nodes = Vec::with_capacity(selected.len());
    for (new_idx, old_idx) in selected.iter().copied().enumerate() {
        old_to_new.insert(old_idx, new_idx);
        sub_nodes.push(info.graph_nodes[old_idx].clone());
    }

    let center_new = old_to_new.get(&center_idx).copied().unwrap_or(0);
    let mut dedup: HashSet<(usize, usize, String)> = HashSet::new();
    let mut sub_edges: Vec<GraphEdge> = info
        .graph_edges
        .iter()
        .filter_map(|edge| {
            let source = *old_to_new.get(&edge.source)?;
            let target = *old_to_new.get(&edge.target)?;
            if source == target {
                return None;
            }
            if !dedup.insert((source, target, edge.kind.clone())) {
                return None;
            }
            Some(GraphEdge {
                source,
                target,
                kind: edge.kind.clone(),
            })
        })
        .collect();

    sub_edges.sort_by(|a, b| {
        let a_center = a.source == center_new || a.target == center_new;
        let b_center = b.source == center_new || b.target == center_new;
        b_center
            .cmp(&a_center)
            .then_with(|| edge_kind_priority(&b.kind).cmp(&edge_kind_priority(&a.kind)))
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.target.cmp(&b.target))
    });
    if sub_edges.len() > max_edges {
        sub_edges.truncate(max_edges);
    }

    Some(MemorySubgraph {
        nodes: sub_nodes,
        _edges: sub_edges,
    })
}

#[cfg(test)]
pub(super) fn pick_subgraph_center(info: &MemoryInfo) -> Option<usize> {
    let mut best_idx: Option<usize> = None;
    let mut best_score: f32 = -1.0;

    for (idx, node) in info.graph_nodes.iter().enumerate() {
        let mut score = graph_node_score(node);
        if node.kind == "tag" || node.kind == "cluster" {
            score -= 0.75;
        }
        if !node.is_active {
            score -= 1.0;
        }
        if score > best_score {
            best_score = score;
            best_idx = Some(idx);
        }
    }

    best_idx
}

#[cfg(test)]
pub(super) fn edge_kind_priority(kind: &str) -> u8 {
    match kind {
        "contradicts" => 6,
        "supersedes" => 5,
        "derived_from" => 4,
        "relates_to" => 3,
        "in_cluster" => 2,
        "has_tag" => 1,
        _ => 1,
    }
}

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
