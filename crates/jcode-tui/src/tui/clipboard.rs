//! Copying text to the user's clipboard: native chain, OSC 52, and the SSH ordering contract.

mod helper;

/// Test-only clipboard sink.
///
/// A headless CI runner has no Wayland socket, no X11 display, and a
/// non-terminal stdout, so every real clipboard path correctly fails and
/// `copy_to_clipboard` returns false. Tests that only care about shortcut
/// wiring (does Alt+S reach the copy handler with the right text?) then fail
/// for an environment reason rather than a code reason. Capturing into this
/// sink lets those tests assert the wiring *and* the copied text without
/// depending on a desktop session (refs #596).
#[cfg(test)]
static TEST_CLIPBOARD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Test-only: route clipboard writes into an in-process sink instead of the OS.
#[cfg(test)]
pub(crate) fn capture_clipboard_for_tests() {
    if let Ok(mut sink) = TEST_CLIPBOARD.lock() {
        *sink = Some(String::new());
    }
}

/// Test-only: the last text written while capture was enabled.
#[cfg(test)]
pub(crate) fn captured_clipboard_for_tests() -> Option<String> {
    TEST_CLIPBOARD.lock().ok().and_then(|sink| sink.clone())
}

/// Test-only: stop capturing and drop any captured text.
#[cfg(test)]
pub(crate) fn stop_capturing_clipboard_for_tests() {
    if let Ok(mut sink) = TEST_CLIPBOARD.lock() {
        *sink = None;
    }
}

/// True when this process is running inside an SSH session, i.e. the user's
/// machine (and clipboard) is elsewhere. In that case the *local* clipboard
/// belongs to the server, and the only path to the user's clipboard is an
/// OSC 52 sequence emitted through the terminal (and forwarded by the SSH
/// client to the client-side terminal emulator).
#[cfg_attr(test, allow(dead_code))]
fn is_remote_ssh_session() -> bool {
    std::env::var("SSH_CONNECTION").is_ok() || std::env::var("SSH_TTY").is_ok()
}

/// Runs the session-appropriate copy chain and reports its outcome.
///
/// The *order* is the contract (it is what the original bug violated):
///
/// - **Remote (SSH) session**: OSC 52 runs first, because it is the only path
///   to the user's (client-side) clipboard, and the native server-side
///   clipboard (NSPasteboard / Win32) would otherwise succeed headless and
///   short-circuit the chain. The local clipboard is then also filled so
///   paste inside jcode keeps working. If OSC 52 is unavailable (e.g. stdout
///   is not a TTY), the local clipboard is still the fallback.
/// - **Local session**: the native clipboard is authoritative and OSC 52 only
///   runs if it fails (a working native clipboard must not emit terminal
///   escape sequences).
///
/// The writers are closures so the order can be unit-tested with stubs,
/// without touching any real clipboard.
fn run_copy_chain(
    remote: bool,
    mut osc52: impl FnMut() -> bool,
    mut local: impl FnMut() -> bool,
) -> bool {
    if remote {
        if osc52() {
            // Keep the local clipboard in sync so paste inside jcode
            // (which reads the server-side clipboard) keeps working.
            let _ = local();
            return true;
        }
        // OSC 52 did not go through (e.g. stdout is not a TTY): still try the
        // local clipboard rather than failing outright.
        return local();
    }
    if local() {
        return true;
    }
    osc52()
}

#[cfg(test)]
mod copy_chain_tests {
    use super::run_copy_chain;
    use std::cell::RefCell;

    /// Records the order in which the clipboard writers are invoked, so the
    /// ordering contract of [`run_copy_chain`] is pinned by tests: a
    /// regression that writes the server-side clipboard before OSC 52 in an
    /// SSH session (the original bug) fails here.
    struct OrderLog(RefCell<Vec<&'static str>>);

    impl OrderLog {
        fn writer(&self, label: &'static str, succeeds: bool) -> impl FnMut() -> bool + '_ {
            let log = &self.0;
            move || {
                log.borrow_mut().push(label);
                succeeds
            }
        }

        fn events(&self) -> Vec<&'static str> {
            self.0.borrow().clone()
        }
    }

    #[test]
    fn ssh_session_writes_osc52_first_then_local() {
        let log = OrderLog(RefCell::new(Vec::new()));
        assert!(run_copy_chain(
            true,
            log.writer("osc52", true),
            log.writer("local", true),
        ));
        assert_eq!(log.events(), ["osc52", "local"]);
    }

    #[test]
    fn ssh_session_still_writes_local_when_osc52_fails() {
        let log = OrderLog(RefCell::new(Vec::new()));
        assert!(run_copy_chain(
            true,
            log.writer("osc52", false),
            log.writer("local", true),
        ));
        assert_eq!(log.events(), ["osc52", "local"]);
    }

    #[test]
    fn ssh_session_reports_failure_when_both_writers_fail() {
        let log = OrderLog(RefCell::new(Vec::new()));
        assert!(!run_copy_chain(
            true,
            log.writer("osc52", false),
            log.writer("local", false),
        ));
        assert_eq!(log.events(), ["osc52", "local"]);
    }

    #[test]
    fn local_session_prefers_native_clipboard_and_skips_osc52() {
        let log = OrderLog(RefCell::new(Vec::new()));
        assert!(run_copy_chain(
            false,
            log.writer("osc52", true),
            log.writer("local", true),
        ));
        // A working native clipboard must not emit terminal escape sequences.
        assert_eq!(log.events(), ["local"]);
    }

    #[test]
    fn local_session_falls_back_to_osc52_when_native_fails() {
        let log = OrderLog(RefCell::new(Vec::new()));
        assert!(run_copy_chain(
            false,
            log.writer("osc52", true),
            log.writer("local", false),
        ));
        assert_eq!(log.events(), ["local", "osc52"]);
    }

    #[test]
    fn local_session_reports_failure_when_both_writers_fail() {
        let log = OrderLog(RefCell::new(Vec::new()));
        assert!(!run_copy_chain(
            false,
            log.writer("osc52", false),
            log.writer("local", false),
        ));
        assert_eq!(log.events(), ["local", "osc52"]);
    }
}

/// Fill the *local* (server-side) clipboard using the platform-native chain.
///
/// - Windows: Win32 via arboard, which is authoritative there (issue #497).
/// - macOS: NSPasteboard via arboard, with pbcopy as a belt-and-braces
///   fallback (Terminal.app ignores OSC 52, so the native path must win
///   locally).
/// - Linux: wl-copy (Wayland), then xclip/xsel (X11, which fork and keep
///   owning the selection unlike arboard, whose X11 backend reverts the
///   selection on drop - issue #684), then arboard.
#[cfg(not(test))]
fn copy_to_local_clipboard(text: &str) -> bool {
    #[cfg(windows)]
    {
        arboard::Clipboard::new()
            .and_then(|mut cb| cb.set_text(text.to_string()))
            .is_ok()
    }

    #[cfg(target_os = "macos")]
    {
        copy_to_local_macos_clipboard(text)
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    {
        if helper::copy_via_clipboard_helper("wl-copy", &[], text) {
            return true;
        }
        if helper::copy_via_clipboard_helper("xclip", &["-selection", "clipboard"], text)
        {
            return true;
        }
        if helper::copy_via_clipboard_helper("xsel", &["--clipboard", "--input"], text) {
            return true;
        }
        arboard::Clipboard::new()
            .and_then(|mut cb| cb.set_text(text.to_string()))
            .is_ok()
    }
}

#[cfg(all(not(test), target_os = "macos"))]
fn copy_to_local_macos_clipboard(text: &str) -> bool {
    if arboard::Clipboard::new()
        .and_then(|mut cb| cb.set_text(text.to_string()))
        .is_ok()
    {
        return true;
    }
    if let Ok(mut child) = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        use std::io::Write;
        if let Some(stdin) = child.stdin.as_mut()
            && stdin.write_all(text.as_bytes()).is_ok()
        {
            drop(child.stdin.take());
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

/// Copy text to clipboard.
///
/// The ordering contract lives in [`run_copy_chain`]: in a remote (SSH)
/// session OSC 52 is emitted first, because the platform-native clipboards
/// succeed even headless and would otherwise short-circuit the only path to
/// the user's own clipboard; the local clipboard is then also filled so
/// paste inside jcode keeps working. In a local session the native clipboard
/// is authoritative and OSC 52 is the final fallback (see
/// [`copy_to_local_clipboard`] for the per-platform native chain).
pub(crate) fn copy_to_clipboard(text: &str) -> bool {
    // Under test, never touch the OS clipboard. Beyond making results identical
    // on a desktop and a headless runner, the Linux path below spawns `wl-copy`,
    // which forks a clipboard server that does not exit; waiting on it hangs the
    // test binary indefinitely. Tests that assert copied text call
    // `capture_clipboard_for_tests` first and then read the sink; tests that
    // only assert "a copy happened" get a truthy result either way.
    //
    // The OS paths are cfg'd out (not merely skipped) so the test build does
    // not carry an unreachable tail after this block's `return`.
    #[cfg(test)]
    {
        if let Ok(mut sink) = TEST_CLIPBOARD.lock() {
            match sink.as_mut() {
                Some(captured) => {
                    captured.clear();
                    captured.push_str(text);
                }
                None => *sink = Some(text.to_string()),
            }
        }
        true
    }

    #[cfg(not(test))]
    {
        run_copy_chain(
            is_remote_ssh_session(),
            || copy_to_clipboard_osc52(text),
            || copy_to_local_clipboard(text),
        )
    }
}

/// Copy text to the clipboard using the OSC 52 terminal escape sequence.
#[cfg_attr(test, allow(dead_code))]
fn copy_to_clipboard_osc52(text: &str) -> bool {
    use base64::Engine as _;
    use std::io::{IsTerminal, Write};

    let mut out = std::io::stdout();
    if !out.is_terminal() {
        return false;
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    // OSC 52: ESC ] 52 ; c ; <base64> BEL
    let seq = format!("\x1b]52;c;{}\x07", encoded);
    out.write_all(seq.as_bytes()).is_ok() && out.flush().is_ok()
}
