use super::*;

pub(super) fn keyboard_enhancement_flags() -> crossterm::event::KeyboardEnhancementFlags {
    use crossterm::event::KeyboardEnhancementFlags;

    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
}

/// Whether this process still owns an unbalanced entry on the terminal's
/// Kitty keyboard-protocol stack.
///
/// The stack is shared with every program on the terminal: an editor spawned
/// via `suspend_terminal_for_editor` may push its own entry after jcode popped
/// ours. A later cleanup (signal handler, guard drop, picker exit) must not pop
/// again, or it would remove the editor's entry instead of ours.
pub(super) static KITTY_KEYBOARD_PUSH_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether an external editor currently owns the terminal.
///
/// During editor handoff jcode has already restored cooked input, left the
/// alternate screen, and disabled its reporting modes. If jcode receives a
/// process-only termination signal while the editor remains alive, cleanup must
/// not write mode disables again because the editor may have enabled those modes
/// for itself.
pub(super) static EDITOR_TERMINAL_OWNERSHIP: AtomicBool = AtomicBool::new(false);

/// Whether a termination signal has already begun the process's final cleanup.
///
/// Once termination started, the process is exiting and the shell behind it
/// must inherit a fully restored terminal. Re-arming any mode afterwards (an
/// editor reclaim/resume that lost the race against the signal cleanup) would
/// leave modes enabled after death because no further cleanup runs (greptile
/// #4232838288). Recorded under the kitty state lock so it cannot interleave
/// with the reclaim's ownership swap.
pub(super) static TERMINATION_STARTED: AtomicBool = AtomicBool::new(false);

/// Serializes the terminal writes and the ownership flag transitions of the
/// Kitty push/pop pair. A termination signal (tokio task, not a UNIX signal
/// handler, so taking a lock is allowed) runs its terminal cleanup on the
/// event loop; this lock makes that cleanup wait for an in-flight push or pop
/// to finish instead of observing a half-applied state and skipping the pop.
pub(super) static KITTY_KEYBOARD_STATE_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) static TERMINAL_OWNERSHIP_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Request Kitty keyboard reporting and tmux's extended-key mode.
///
/// Intentionally avoid REPORT_ALL_KEYS_AS_ESCAPE_CODES for now. When that flag is enabled,
/// terminals such as kitty/Alacritty/Warp can report printable keys as a base key plus
/// modifiers instead of the final text produced by the active keyboard layout. Crossterm does
/// not yet expose kitty's associated text / alternate key data, so we cannot safely reconstruct
/// shifted symbols for every keyboard layout. Prefer the terminal-delivered printable character
/// and only synthesize ASCII letter casing in the input fallback.
///
/// Returns whether the requests were written, not whether the terminal supports them.
pub fn enable_keyboard_enhancement() -> bool {
    let _guard = kitty_keyboard_state_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if termination_started() {
        crate::logging::info("Keyboard enhancement request skipped: termination already started");
        return false;
    }
    let result = enable_keyboard_enhancement_to(&mut std::io::stdout(), inside_tmux()).is_ok();
    crate::logging::info(&format!(
        "Keyboard enhancement request: {}",
        if result { "sent" } else { "FAILED" }
    ));
    if result {
        // Set while still holding the state lock so a signal cleanup cannot
        // observe the push write without the ownership flag.
        kitty_keyboard_push_active().store(true, Ordering::Release);
    }
    result
}

/// Adopt a Kitty keyboard entry the previous process left on the stack when
/// it exec'd into this one (restart, reload, rebuild, update).
///
/// The exec handoff deliberately keeps the terminal in raw mode with all
/// modes armed instead of popping and re-pushing, so this process must record
/// that the inherited entry is ours to pop on exit. Otherwise every cleanup
/// gate skips the pop and the following shell is left in CSI u mode.
pub fn adopt_keyboard_enhancement_ownership() {
    let _guard = kitty_keyboard_state_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if termination_started() {
        return;
    }
    kitty_keyboard_push_active().store(true, Ordering::Release);
}

pub(super) fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|value| !value.is_empty())
}

pub(super) fn enable_keyboard_enhancement_to(
    writer: &mut impl std::io::Write,
    inside_tmux: bool,
) -> std::io::Result<()> {
    use crossterm::event::PushKeyboardEnhancementFlags;
    request_tmux_extended_keys_to(writer, inside_tmux)?;
    crossterm::execute!(
        writer,
        PushKeyboardEnhancementFlags(keyboard_enhancement_flags())
    )
}

/// Reset tmux extended keys and pop Kitty keyboard reporting.
///
/// The pop is skipped when this process has no unbalanced push on the
/// terminal's stack (a suspended editor or the calling shell may own the top
/// entry), so an extra pop cannot remove someone else's keyboard settings.
pub fn disable_keyboard_enhancement() {
    if !disable_keyboard_enhancement_if_owned() {
        crate::logging::info("Keyboard enhancement pop skipped: no active push owned");
    }
}

/// Pop the Kitty keyboard entry this process pushed, if it is still unbalanced.
///
/// Returns whether a pop was performed. The state lock orders this against
/// a concurrent push or signal cleanup, so the flag swap and the pop write
/// happen as one unit: termination waits for the pop instead of exiting with
/// the entry still active.
pub fn disable_keyboard_enhancement_if_owned() -> bool {
    disable_keyboard_enhancement_if_owned_to(&mut std::io::stdout(), inside_tmux())
}

/// Writer-backed form of [`disable_keyboard_enhancement_if_owned`] so tests
/// can verify the pop bytes without writing to the developer's terminal or
/// resetting tmux extended keys.
pub(super) fn disable_keyboard_enhancement_if_owned_to(
    writer: &mut impl std::io::Write,
    inside_tmux: bool,
) -> bool {
    let _guard = kitty_keyboard_state_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if !kitty_keyboard_push_active().swap(false, std::sync::atomic::Ordering::AcqRel) {
        return false;
    }
    let _ = disable_keyboard_enhancement_to(writer, inside_tmux);
    true
}

/// Test hook: reset the ownership flag without touching the terminal.
#[cfg(test)]
pub(super) fn reset_keyboard_enhancement_ownership_for_test() {
    kitty_keyboard_push_active().store(false, Ordering::Release);
}

pub(super) fn kitty_keyboard_push_active() -> &'static AtomicBool {
    &KITTY_KEYBOARD_PUSH_ACTIVE
}

pub(super) fn kitty_keyboard_state_lock() -> &'static Mutex<()> {
    &KITTY_KEYBOARD_STATE_LOCK
}

pub fn editor_owns_terminal() -> bool {
    EDITOR_TERMINAL_OWNERSHIP.load(Ordering::Acquire)
}

pub fn set_editor_terminal_ownership(owns_terminal: bool) {
    EDITOR_TERMINAL_OWNERSHIP.store(owns_terminal, Ordering::Release);
}

/// Publish that an external editor owns the terminal, atomically with the
/// signal cleanup's ownership check (both take the kitty state lock).
///
/// Callers must only publish AFTER jcode's own terminal restores finished
/// (`suspend_terminal_for_editor` completed): while the restores are in flight
/// the flag must stay false so a termination signal in that window performs a
/// full cleanup instead of skipping one and leaving the shell in raw mode
/// (greptile #4230626960).
pub fn publish_editor_terminal_handoff() {
    with_kitty_state_lock(|| {
        set_editor_terminal_ownership(true);
    });
}

/// Reclaim the terminal after the editor exited, atomically with the signal
/// cleanup's ownership check. Called before `resume_terminal_after_editor`
/// re-arms jcode's own modes: a signal arriving after the editor exited must
/// run the full cleanup against whatever state the resume loop has armed so
/// far, not skip it.
pub fn reclaim_editor_terminal_handoff() {
    with_kitty_state_lock(|| {
        // The signal cleanup released the lock but the process has not exited
        // yet: this reclaim runs in that gap, and letting the subsequent resume
        // re-arm modes would leave them enabled after death because no further
        // cleanup runs (greptile #4232838288). Leave ownership set so the
        // resume loop sees the terminating state and writes nothing.
        if termination_started() {
            return;
        }
        set_editor_terminal_ownership(false);
    });
}

/// Run `cleanup` while holding the kitty state lock, so an in-flight editor
/// handoff publication or reclaim (which takes the same lock) is ordered
/// against it. A signal arriving mid-handoff then sees a consistent state:
/// either jcode still owns the terminal (restore everything) or the editor
/// does (skip).
pub fn with_kitty_state_lock<R>(cleanup: impl FnOnce() -> R) -> R {
    let lock = kitty_keyboard_state_lock();
    let _state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    cleanup()
}

/// Record that termination has begun, under the kitty state lock.
///
/// Called by the termination-signal cleanup as soon as it decides what to do
/// with the terminal. Recording under the same lock as the editor handoff's
/// publish/reclaim closes the gap between this cleanup releasing the lock and
/// the process actually exiting: a reclaim racing into that gap observes the
/// flag and leaves the terminal modes alone, so the shell receives normal
/// input after jcode dies instead of escape sequences (greptile #4232838288).
pub fn record_termination_started() {
    with_kitty_state_lock(|| {
        TERMINATION_STARTED.store(true, Ordering::Release);
    });
}

pub(super) fn termination_started() -> bool {
    TERMINATION_STARTED.load(Ordering::Acquire)
}

/// Test hook: clear the shared termination flag without touching the terminal.
#[cfg(test)]
pub(super) fn reset_termination_started_for_test() {
    TERMINATION_STARTED.store(false, Ordering::Release);
}

pub(super) fn disable_keyboard_enhancement_to(
    writer: &mut impl std::io::Write,
    inside_tmux: bool,
) -> std::io::Result<()> {
    if inside_tmux {
        writer.write_all(b"\x1b[>4;0m")?;
    }
    crossterm::execute!(writer, crossterm::event::PopKeyboardEnhancementFlags)
}

pub(super) fn request_tmux_extended_keys_to(
    writer: &mut impl std::io::Write,
    inside_tmux: bool,
) -> std::io::Result<()> {
    if inside_tmux {
        // `extended-keys on` needs modifyOtherKeys opt-in, not a Kitty push.
        writer.write_all(b"\x1b[>4;2m")?;
    }
    Ok(())
}

pub(super) fn reapply_keyboard_enhancement_to(
    writer: &mut impl std::io::Write,
    inside_tmux: bool,
) -> std::io::Result<()> {
    request_tmux_extended_keys_to(writer, inside_tmux)?;
    write!(writer, "\x1b[={}u", keyboard_enhancement_flags().bits())
}

/// Reassert terminal modes that terminals may clear while the TUI remains alive.
///
/// Kitty keyboard enhancement uses its `set` form rather than the stack-based
/// `push`, keeping the shutdown pop balanced. Enabling focus reporting may itself
/// produce a focus event, so focus-event handlers must pass `focus_change = false`.
pub(crate) fn reapply_terminal_modes_to(
    writer: &mut impl std::io::Write,
    mouse_capture: bool,
    keyboard_enhanced: bool,
    focus_change: bool,
) -> std::io::Result<()> {
    use crossterm::QueueableCommand;
    use crossterm::event::{EnableBracketedPaste, EnableFocusChange, EnableMouseCapture};

    writer.queue(EnableBracketedPaste)?;
    if focus_change {
        writer.queue(EnableFocusChange)?;
    }
    if mouse_capture {
        writer.queue(EnableMouseCapture)?;
        // Crossterm toggles Win32 console mouse input on Windows, but ConPTY
        // hosts such as VS Code also need the VT tracking modes reasserted.
        #[cfg(windows)]
        writer.write_all(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h")?;
    }
    if keyboard_enhanced {
        reapply_keyboard_enhancement_to(writer, inside_tmux())?;
    }
    writer.flush()
}

pub(crate) fn reapply_configured_terminal_modes_after_focus() {
    let policy = crate::perf::tui_policy();
    if let Err(error) = reapply_terminal_modes_after_focus_to(
        &mut std::io::stdout(),
        policy.enable_mouse_capture,
        policy.enable_keyboard_enhancement,
    ) {
        crate::logging::warn(&format!("failed to reapply terminal modes: {error}"));
    }
}

pub(super) fn reapply_terminal_modes_after_focus_to(
    writer: &mut impl std::io::Write,
    mouse_capture: bool,
    keyboard_enhanced: bool,
) -> std::io::Result<()> {
    reapply_terminal_modes_to(
        writer,
        mouse_capture,
        keyboard_enhanced,
        // Ghostty reports its current focus when mode 1004 is enabled. Re-arming
        // it from FocusGained would feed that reply back into this handler forever.
        // Startup and resume-after-editor still enable focus reporting normally.
        false,
    )
}
