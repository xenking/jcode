//! Permission setup: check, request, deep-link, and poll the macOS TCC grants
//! needed for desktop control.

use super::native::{capture_trusted as screen_recording_ok, probe_accessibility, trusted};
use anyhow::Result;
use jcode_tool_types::ToolOutput;
use serde_json::json;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

fn yes_no(b: bool) -> &'static str {
    if b { "granted" } else { "NOT granted" }
}

const ACCESSIBILITY_REGRANT: &str =
    "Re-add Jcode in System Settings > Privacy & Security > Accessibility and enable it.";

#[derive(Debug, PartialEq, Eq)]
enum Accessibility {
    Granted,
    NeedsRegrant,
    Unverified,
}

fn accessibility_status(ax_code: Option<i32>) -> Accessibility {
    match ax_code {
        Some(0) => Accessibility::Granted,
        Some(-25200 | -25211) => Accessibility::NeedsRegrant,
        _ => Accessibility::Unverified,
    }
}

fn accessibility_ok() -> bool {
    accessibility_status(probe_accessibility()) == Accessibility::Granted
}

/// Report status only.
pub fn check_permissions() -> Result<ToolOutput> {
    let ax_code = probe_accessibility();
    let ax = accessibility_status(ax_code);
    let screen = screen_recording_ok();
    let swift = std::path::Path::new("/usr/bin/swift").exists()
        || Command::new("/usr/bin/which")
            .arg("swift")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

    let ax_label = match ax {
        Accessibility::Granted => "granted",
        Accessibility::NeedsRegrant => "NOT granted",
        Accessibility::Unverified => "not verified",
    };
    let mut lines = vec![
        format!("Accessibility (input + AX control): {ax_label}"),
        format!(
            "Screen Recording (screenshots/OCR): {} (macOS preflight only, capture not tested)",
            yes_no(screen)
        ),
        format!(
            "Swift toolchain (for OCR):          {}",
            if swift { "present" } else { "missing" }
        ),
    ];
    match ax {
        Accessibility::Granted => {}
        Accessibility::NeedsRegrant => lines.push(ACCESSIBILITY_REGRANT.into()),
        Accessibility::Unverified => lines.push("Retry with an unlocked macOS desktop.".into()),
    }
    if !screen {
        lines.push("Run action='setup' to open the Screen Recording settings pane.".into());
    }
    Ok(ToolOutput::new(lines.join("\n")).with_metadata(json!({
        "accessibility": ax == Accessibility::Granted,
        "accessibility_ax_code": ax_code, "accessibility_trusted": trusted(),
        "screen_recording": screen, "swift": swift,
    })))
}

/// Request permissions: prompt, deep-link, and poll Accessibility until granted.
pub fn setup() -> Result<ToolOutput> {
    let mut log = Vec::new();

    let ax0 = accessibility_ok();
    let screen0 = screen_recording_ok();
    log.push(format!(
        "Initial: accessibility={}, screen_recording_preflight={}",
        ax0, screen0
    ));

    if !ax0 {
        // Deep-link to the exact Accessibility pane.
        let _ = Command::new("/usr/bin/open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .status();
        log.push(
            "Opened Privacy & Security > Accessibility. Add and enable your terminal/jcode there."
                .into(),
        );
    }
    if !screen0 {
        let _ = Command::new("/usr/bin/open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
            .status();
        log.push(
            "Opened Privacy & Security > Screen Recording. Add and enable your terminal/jcode there."
                .into(),
        );
    }

    // Poll Accessibility for up to ~30s so the agent can report "ready".
    if !ax0 {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut granted = false;
        while Instant::now() < deadline {
            if accessibility_ok() {
                granted = true;
                break;
            }
            sleep(Duration::from_millis(1000));
        }
        log.push(format!(
            "Accessibility after wait: {}",
            if granted {
                "granted"
            } else {
                "still not verified (re-run check_permissions for details)"
            }
        ));
    }

    let ax = accessibility_ok();
    let screen = screen_recording_ok();
    log.push(format!(
        "Final: accessibility={}, screen_recording_preflight={}",
        ax, screen
    ));
    if !ax {
        log.push(
            "NOTE: the Accessibility toggle cannot be enabled programmatically (macOS security). \
             It is the one switch you must flip by hand."
                .into(),
        );
    }

    Ok(ToolOutput::new(log.join("\n")).with_metadata(json!({
        "accessibility": ax, "screen_recording": screen,
    })))
}

#[cfg(test)]
mod tests {
    use super::{Accessibility, accessibility_status};

    #[test]
    fn only_ax_success_grants_accessibility() {
        assert_eq!(accessibility_status(Some(0)), Accessibility::Granted);
        for code in [-25200, -25211] {
            assert_eq!(
                accessibility_status(Some(code)),
                Accessibility::NeedsRegrant
            );
        }
        for code in [Some(-25204), Some(-25212), None] {
            assert_eq!(accessibility_status(code), Accessibility::Unverified);
        }
    }
}
