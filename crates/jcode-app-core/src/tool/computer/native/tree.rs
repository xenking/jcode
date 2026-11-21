use super::target::{Bound, Identity, Target, hash_value};
use super::{Failure, NativeResult};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const MAX_VISITED: usize = 320;
const MAX_DEPTH: usize = 20;
const SNAPSHOT_TTL: Duration = Duration::from_secs(120);

pub(super) struct Snapshot {
    pub(super) id: String,
    pub(super) target: Target,
    pub(super) refs: HashMap<String, Bound>,
    pub(super) query: Option<String>,
    pub(super) budget: usize,
    created: Instant,
}

impl Snapshot {
    pub(super) fn expired(&self) -> bool {
        self.created.elapsed() > SNAPSHOT_TTL
    }
}

struct Node {
    bound: Bound,
    value: Option<String>,
    parent: Option<usize>,
    depth: usize,
    actions: Vec<String>,
}

struct Tree {
    nodes: Vec<Node>,
    truncated: bool,
    matched: bool,
}

fn walk(target: &Target, deadline: Instant, stop_at: Option<&str>) -> NativeResult<Tree> {
    let mut pending = vec![(target.window.clone(), None, 0)];
    let mut nodes: Vec<Node> = Vec::new();
    let mut truncated = false;
    let mut visited = 0;
    while let Some((element, parent, depth)) = pending.pop() {
        if visited >= MAX_VISITED || Instant::now() >= deadline {
            truncated = true;
            break;
        }
        visited += 1;
        if nodes.iter().any(|n| n.bound.element == element) {
            continue;
        }
        let identity = match Identity::read(&element) {
            Ok(Some(identity)) => identity,
            Ok(None) if depth != 0 => continue,
            Err(e) if depth != 0 && e.ax_code == Some(-25202) => {
                truncated = true;
                continue;
            }
            Ok(None) => return Err(Failure::new("window_changed")),
            Err(e) => return Err(e),
        };
        if identity.secure() {
            continue;
        }
        let value = element.value()?;
        if stop_at.is_some_and(|needle| matches_query(&identity, value.as_deref(), needle)) {
            return Ok(Tree {
                nodes,
                truncated,
                matched: true,
            });
        }
        let actions = if stop_at.is_none() {
            element.actions()?
        } else {
            Vec::new()
        };
        let index = nodes.len();
        let child_limit = MAX_VISITED.saturating_sub(visited).saturating_add(1);
        let mut children = element.elements("AXChildren", child_limit)?;
        if depth == 0 {
            for sheet in element.elements("AXSheets", child_limit)? {
                if !children.contains(&sheet) {
                    children.push(sheet);
                }
            }
        }
        if depth == MAX_DEPTH {
            truncated |= !children.is_empty();
        } else {
            pending.extend(
                children
                    .into_iter()
                    .rev()
                    .map(|child| (child, Some(index), depth + 1)),
            );
        }
        nodes.push(Node {
            bound: Bound {
                element,
                identity,
                value_hash: value.as_deref().map(hash_value),
            },
            value,
            parent,
            depth,
            actions,
        });
    }
    Ok(Tree {
        nodes,
        truncated,
        matched: false,
    })
}

fn matches_query(identity: &Identity, value: Option<&str>, needle: &str) -> bool {
    [
        Some(identity.role.as_str()),
        identity.label.as_deref(),
        value,
    ]
    .into_iter()
    .flatten()
    .any(|s| s.to_lowercase().contains(needle))
}

pub(super) fn exists(target: &Target, query: &str, end: Instant) -> NativeResult<bool> {
    let tree = walk(target, end, Some(&query.to_lowercase()))?;
    if tree.truncated && !tree.matched && Instant::now() < end {
        return Err(Failure::new("ax_search_limit"));
    }
    Ok(tree.matched)
}

pub(super) fn collect(
    target: Target,
    query: Option<&str>,
    budget: usize,
    ax: bool,
) -> NativeResult<(Snapshot, Value)> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let mut refs = HashMap::new();
    let mut rows = Vec::new();
    let mut focus_ref = None;
    let mut truncated = false;
    if ax {
        let focus = target.app.element("AXFocusedUIElement")?;
        let tree = walk(&target, Instant::now() + Duration::from_secs(2), None)?;
        truncated = tree.truncated;
        let needle = query.map(str::to_lowercase);
        let mut visible: Vec<_> = tree
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| {
                let identity = &node.bound.identity;
                let value = node.value.as_deref();
                i == 0
                    || needle.as_deref().map_or_else(
                        || {
                            !matches!(
                                identity.role.as_str(),
                                "AXGroup" | "AXLayoutArea" | "AXUnknown" | "AXScrollArea"
                            ) || identity.label.is_some()
                                || value.is_some()
                                || !node.actions.is_empty()
                        },
                        |needle| matches_query(identity, value, needle),
                    )
            })
            .collect();
        for (i, node) in tree.nodes.iter().enumerate().rev() {
            if visible[i]
                && let Some(parent) = node.parent
            {
                visible[parent] = true;
            }
        }
        let mut used = 2;
        for (index, node) in tree
            .nodes
            .into_iter()
            .enumerate()
            .filter(|(i, _)| visible[*i])
        {
            let handle = format!("e{:x}", index + 1);
            let row = render(&node, &handle);
            let bytes = serde_json::to_string(&row)
                .expect("string JSON encoding")
                .len()
                + 1;
            if used + bytes > budget {
                truncated = true;
                break;
            }
            used += bytes;
            if focus.as_ref() == Some(&node.bound.element) {
                focus_ref = Some(handle.clone());
            }
            rows.push(row);
            refs.insert(handle, node.bound);
        }
    }
    let output = json!({"snapshot_id":id,"target":target.describe(),"focus_ref":focus_ref,"tree":rows,"truncated":truncated});
    Ok((
        Snapshot {
            id,
            target,
            refs,
            query: query.map(str::to_owned),
            budget,
            created: Instant::now(),
        },
        output,
    ))
}

fn render(node: &Node, handle: &str) -> String {
    let mut out = format!(
        "{}{} {}",
        " ".repeat(node.depth),
        handle,
        node.bound.identity.role.trim_start_matches("AX")
    );
    if let Some(label) = &node.bound.identity.label {
        out.push(' ');
        out.push_str(&summary(label));
    }
    if let Some(value) = &node.value {
        out.push_str(" = ");
        out.push_str(&summary(value));
    }
    let mut names = node.actions.iter().map(|s| s.trim_start_matches("AX"));
    if let Some(first) = names.next() {
        out.push_str(" [");
        out.push_str(first);
        for action in names {
            out.push(',');
            out.push_str(action);
        }
        out.push(']');
    }
    out
}

fn summary(s: &str) -> String {
    let mut result: String = s
        .chars()
        .take(68)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if s.chars().nth(68).is_some() {
        result.push('…');
    }
    result
}
