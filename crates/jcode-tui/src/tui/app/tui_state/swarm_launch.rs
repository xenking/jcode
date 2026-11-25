pub(super) fn open_swarm_agent_window(
    session_id: &str,
    is_ssh_remote: bool,
) -> anyhow::Result<bool> {
    open_swarm_agent_window_with_launcher(
        session_id,
        is_ssh_remote,
        |exe, session_id, cwd, context| {
            spawn_swarm_agent_window(exe, session_id, cwd, context, is_ssh_remote)
        },
    )
}

pub(super) fn open_swarm_agent_window_with_launcher(
    session_id: &str,
    is_ssh_remote: bool,
    mut launcher: impl FnMut(
        &std::path::Path,
        &str,
        &std::path::Path,
        &jcode_app_core::session_launch::SessionSpawnContext,
    ) -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    let exe = super::super::launch_client_executable();
    let cwd = crate::session::Session::load_startup_stub(session_id)
        .ok()
        .and_then(|session| session.working_dir)
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
    let context = swarm_popout_launch_context(is_ssh_remote, &crate::server::socket_path());
    launcher(&exe, session_id, &cwd, &context)
}

pub(super) fn spawn_swarm_agent_window(
    exe: &std::path::Path,
    session_id: &str,
    cwd: &std::path::Path,
    context: &jcode_app_core::session_launch::SessionSpawnContext,
    is_ssh_remote: bool,
) -> anyhow::Result<bool> {
    spawn_swarm_agent_window_with_launchers(
        exe,
        session_id,
        cwd,
        context,
        is_ssh_remote,
        jcode_app_core::terminal_launch::spawn_command_in_new_terminal,
        |exe, session_id, cwd| {
            let socket = std::env::var("JCODE_SOCKET").ok();
            super::super::spawn_in_new_terminal(exe, session_id, cwd, socket.as_deref())
        },
    )
}

pub(super) fn swarm_agent_terminal_command(
    exe: &std::path::Path,
    session_id: &str,
    context: &jcode_app_core::session_launch::SessionSpawnContext,
) -> jcode_app_core::terminal_launch::TerminalCommand {
    let title = jcode_app_core::session_launch::resumed_window_title(session_id);
    let mut args = vec!["--fresh-spawn".into(), "--resume".into(), session_id.into()];
    if let Some(socket) = context
        .runtime_env
        .iter()
        .find(|(key, _)| key == "JCODE_SOCKET")
        .map(|(_, value)| value)
    {
        args.extend(["--socket".into(), socket.clone()]);
    }
    let mut command = jcode_app_core::terminal_launch::TerminalCommand::new(exe, args)
        .title(title)
        .fresh_spawn()
        .kind("resume")
        .session_id(session_id);
    for (key, value) in &context.extra_env {
        command = command.spawn_env(key.clone(), value.clone());
    }
    if !context.runtime_env.is_empty() {
        command = command.runtime_env(context.runtime_env.clone());
    }
    if !context.client_terminal_env.is_empty() {
        command = command.client_terminal_env(context.client_terminal_env.clone());
    }
    command
}

pub(super) fn spawn_swarm_agent_window_with_launchers(
    exe: &std::path::Path,
    session_id: &str,
    cwd: &std::path::Path,
    context: &jcode_app_core::session_launch::SessionSpawnContext,
    is_ssh_remote: bool,
    mut local_launcher: impl FnMut(
        &jcode_app_core::terminal_launch::TerminalCommand,
        &std::path::Path,
    ) -> anyhow::Result<bool>,
    mut ssh_launcher: impl FnMut(&std::path::Path, &str, &std::path::Path) -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    if is_ssh_remote {
        ssh_launcher(exe, session_id, cwd)
    } else {
        let command = swarm_agent_terminal_command(exe, session_id, context);
        local_launcher(&command, cwd)
    }
}

/// Decode direct agent badges without capturing plain digits or other chords.
pub(crate) fn swarm_agent_shortcut_index(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Option<usize> {
    use crossterm::event::{KeyCode, KeyModifiers};
    if modifiers != KeyModifiers::ALT {
        return None;
    }
    match code {
        KeyCode::Char('0') => Some(9),
        KeyCode::Char(c @ '1'..='9') => Some((c as u8 - b'1') as usize),
        _ => None,
    }
}

/// Runtime context for opening a swarm agent's window: local daemon clients
/// carry their effective runtime so the new terminal connects back to the same
/// one; SSH clients keep the historical context-free behavior.
pub(super) fn swarm_popout_launch_context(
    is_ssh_remote: bool,
    socket: &std::path::Path,
) -> jcode_app_core::session_launch::SessionSpawnContext {
    if is_ssh_remote {
        jcode_app_core::session_launch::SessionSpawnContext::default()
    } else {
        jcode_app_core::session_launch::SessionSpawnContext::default()
            .with_local_runtime(socket)
            .with_client_terminal_env(
                jcode_app_core::terminal_launch::snapshot_client_terminal_env(),
            )
    }
}

/// What a key press should do while the swarm panel is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwarmPanelAction {
    SelectNext,
    SelectPrev,
    PopOut,
    OpenPrompt,
    Exit,
}

/// Map a key to a focused-swarm-panel action.
///
/// Deliberately narrow: the focused panel must NOT swallow plain typing (the
/// user may keep writing into the chat input while glancing at agents), so
/// only Esc and Alt-chords are claimed:
/// - Alt+↑ / Alt+↓ (also Alt+k / Alt+j): move the selection
/// - Alt+o / Alt+Enter: pop the selected agent out to a terminal
/// - Alt+Shift+p: open the active swarm routing prompt in the editor
/// - Esc: exit the panel
pub(crate) fn swarm_panel_action_for_key(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Option<SwarmPanelAction> {
    use crossterm::event::{KeyCode, KeyModifiers};
    if code == KeyCode::Esc && modifiers.is_empty() {
        return Some(SwarmPanelAction::Exit);
    }
    let alt = modifiers.contains(KeyModifiers::ALT);
    // macOS Option+letter often arrives as a transformed glyph with no ALT
    // modifier; normalize through the shared shortcut helper.
    let macos_letter = crate::tui::keybind::shortcut_char_for_macos_option_key(code, modifiers);
    let macos_shift_letter =
        crate::tui::keybind::shortcut_char_for_macos_option_shift_key(code, modifiers);
    if macos_shift_letter == Some('p') {
        return Some(SwarmPanelAction::OpenPrompt);
    }
    match code {
        KeyCode::Down | KeyCode::Char('j') if alt => Some(SwarmPanelAction::SelectNext),
        KeyCode::Up | KeyCode::Char('k') if alt => Some(SwarmPanelAction::SelectPrev),
        KeyCode::Char('o') | KeyCode::Enter if alt => Some(SwarmPanelAction::PopOut),
        KeyCode::Char('P') if alt => Some(SwarmPanelAction::OpenPrompt),
        KeyCode::Char('p') if alt && modifiers.contains(KeyModifiers::SHIFT) => {
            Some(SwarmPanelAction::OpenPrompt)
        }
        _ => match macos_letter {
            Some('j') => Some(SwarmPanelAction::SelectNext),
            Some('k') => Some(SwarmPanelAction::SelectPrev),
            Some('o') => Some(SwarmPanelAction::PopOut),
            _ => None,
        },
    }
}

/// Restrict swarm members to the descendants `self_id` actually spawned: every
/// member whose `report_back_to_session_id` chain reaches `self_id`, *excluding*
/// `self_id` itself.
///
/// This keeps the inline swarm strip scoped to the agents a session manages,
/// without listing the viewing session as one of "its" agents and without
/// showing unrelated members that merely share the swarm (e.g. other sessions in
/// the same repository).
///
/// Returns empty when the session has not spawned anyone, which the caller uses
/// to hide the strip entirely.
pub(crate) fn filter_inline_swarm_subtree(
    members: &[crate::protocol::SwarmMemberStatus],
    self_id: &str,
) -> Vec<crate::protocol::SwarmMemberStatus> {
    use std::collections::{HashMap, HashSet};

    // Build the parent -> children index once, then walk outward from this
    // session. The previous implementation rebuilt a cycle-detection HashSet
    // while walking the parent chain for every member, on every frame. Large,
    // long-lived swarms made that input render path needlessly expensive.
    let mut children_by_parent: HashMap<&str, Vec<&str>> = HashMap::new();
    for member in members {
        if let Some(parent) = member.report_back_to_session_id.as_deref() {
            children_by_parent
                .entry(parent)
                .or_default()
                .push(member.session_id.as_str());
        }
    }

    let mut descendants: HashSet<&str> = HashSet::new();
    let mut pending = vec![self_id];
    while let Some(parent) = pending.pop() {
        let Some(children) = children_by_parent.get(parent) else {
            continue;
        };
        for &child in children {
            // This both excludes cycles and ensures each subtree node is
            // expanded at most once.
            if child != self_id && descendants.insert(child) {
                pending.push(child);
            }
        }
    }

    members
        .iter()
        .filter(|m| descendants.contains(m.session_id.as_str()))
        .cloned()
        .collect()
}

/// A nonempty descendant traversal must start with a non-self direct child.
/// Conversely that child's own record is in the result, even with duplicate
/// IDs or cycles. No indexing, cloning, or elapsed-time updates are needed.
pub(super) fn has_inline_swarm_descendants(
    members: &[crate::protocol::SwarmMemberStatus],
    self_id: &str,
) -> bool {
    members.iter().any(|member| {
        member.session_id != self_id && member.report_back_to_session_id.as_deref() == Some(self_id)
    })
}
