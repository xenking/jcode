//! Shared macOS computer-use runtime. The host owns authorisation and session routing.

#![cfg(target_os = "macos")]

use anyhow::{Context, Result, bail};
use jcode_tool_types::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Mutex;

mod ax;
mod discover;
mod input;
mod keys;
mod native;
mod osa;
mod screen;
mod setup;
mod sys;
mod win;

#[derive(Debug, Deserialize)]
struct ComputerInput {
    #[serde(alias = "op")]
    action: String,
    // discovery
    #[serde(default)]
    category: Option<String>,
    // coordinates
    #[serde(default)]
    x: Option<f64>,
    #[serde(default)]
    y: Option<f64>,
    #[serde(default)]
    to_x: Option<f64>,
    #[serde(default)]
    to_y: Option<f64>,
    #[serde(default)]
    w: Option<f64>,
    #[serde(default)]
    h: Option<f64>,
    // text / keys
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    keys: Option<String>,
    #[serde(default)]
    dx: Option<i32>,
    #[serde(default)]
    dy: Option<i32>,
    #[serde(default)]
    depth: Option<u32>,
    // AX / scoping
    #[serde(default)]
    app: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    element: Option<Value>,
    #[serde(default)]
    ax_action: Option<String>,
    #[serde(default)]
    menu_path: Option<Vec<String>>,
    // windows
    #[serde(default)]
    window_id: Option<i64>,
    // scripting / wait / system
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    contains: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    region: Option<[f64; 4]>,
    #[serde(default)]
    level: Option<f64>,
    /// For mutating actions: resolve and report the target without acting.
    #[serde(default)]
    dry_run: Option<bool>,
}

/// The executable part of a request never crosses the wire or grants consent.
pub struct Operation(OperationKind);

enum OperationKind {
    Native(native::Request),
    Legacy(ComputerInput),
}

pub enum Request {
    Local(ToolOutput),
    Desktop(Operation),
}

impl Request {
    pub fn parse(input: Value) -> Result<Self> {
        let parsed: ComputerInput = serde_json::from_value(input.clone())
            .context("invalid macos_computer_use tool input")?;
        if parsed.action == "discover"
            || (parsed.dry_run == Some(true) && is_mutating(&parsed.action))
        {
            return run(parsed).map(Self::Local);
        }
        let kind = match native::Request::parse(&parsed.action, input)? {
            Some(native) => OperationKind::Native(native),
            None => OperationKind::Legacy(parsed),
        };
        Ok(Self::Desktop(Operation(kind)))
    }
}

/// Snapshot references are private to one session in the native GUI process.
#[derive(Default)]
pub struct Engine {
    snapshots: Mutex<native::Snapshots>,
}

impl Engine {
    pub fn execute(&self, operation: Operation) -> Result<ToolOutput> {
        match operation.0 {
            OperationKind::Native(request) => {
                let mut slots = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
                Ok(request.execute(&mut slots))
            }
            OperationKind::Legacy(request) => run(request),
        }
    }

    pub fn discard_expired(&self) {
        if let Ok(mut snapshots) = self.snapshots.try_lock() {
            snapshots.discard_expired();
        }
    }
}

/// Cap a tool output's text so a huge AX tree / clipboard / OCR dump cannot
/// blow up the context. Keeps the head and notes how much was dropped.
#[cfg(target_os = "macos")]
fn cap_output(mut out: ToolOutput, max_chars: usize) -> ToolOutput {
    if out.output.len() > max_chars {
        let mut cut = max_chars;
        while cut > 0 && !out.output.is_char_boundary(cut) {
            cut -= 1;
        }
        let dropped = out.output.len() - cut;
        let head = out.output[..cut].to_string();
        out.output = format!("{head}\n… [truncated {dropped} chars]");
    }
    out
}

/// Actions that change desktop/app state. Used for dry_run gating.
#[cfg(target_os = "macos")]
fn is_mutating(action: &str) -> bool {
    matches!(
        action,
        "act"
            | "setup"
            | "move"
            | "click"
            | "double_click"
            | "right_click"
            | "drag"
            | "scroll"
            | "type"
            | "key"
            | "key_down"
            | "key_up"
            | "press"
            | "set_value"
            | "perform_action"
            | "select_menu"
            | "activate_app"
            | "hide_app"
            | "quit_app"
            | "focus_window"
            | "move_window"
            | "resize_window"
            | "minimize_window"
            | "close_window"
            | "set_clipboard"
            | "run_applescript"
            | "run_jxa"
            | "notify"
            | "set_brightness"
    )
}

#[cfg(target_os = "macos")]
fn run(input: ComputerInput) -> Result<ToolOutput> {
    let action = input.action.as_str();

    // dry_run: for mutating actions, report the intended target and stop.
    if input.dry_run == Some(true) && is_mutating(action) {
        return Ok(ToolOutput::new(format!(
            "[dry_run] would perform '{action}' (no action taken). \
             Re-issue without dry_run to execute."
        )));
    }

    let result = dispatch(action, &input);
    // Cap large textual outputs to protect context (images are unaffected).
    result.map(|o| cap_output(o, 16_000))
}

#[cfg(target_os = "macos")]
fn dispatch(action: &str, input: &ComputerInput) -> Result<ToolOutput> {
    match action {
        // ---- discovery & setup ----
        "discover" => discover::discover(input.category.as_deref()),
        "setup" => setup::setup(),
        "check_permissions" => setup::check_permissions(),

        // ---- observe ----
        "screenshot" => screen::screenshot(),
        "ocr" => screen::ocr(input.region),
        "window_screenshot" => {
            let id = input
                .window_id
                .context("window_screenshot requires `window_id`")?;
            screen::window_screenshot(id)
        }
        "ui" => ax::ui_tree(input.app.as_deref(), input.depth.unwrap_or(12)),
        "cursor" => {
            let p = input::current_cursor()?;
            Ok(
                ToolOutput::new(format!("cursor at ({:.0}, {:.0})", p.x, p.y))
                    .with_metadata(json!({ "x": p.x, "y": p.y })),
            )
        }

        // ---- coordinate input (visible) ----
        "move" => {
            let (x, y) = require_xy(input)?;
            input::move_to(x, y)?;
            Ok(ToolOutput::new(format!("moved cursor to ({x:.0}, {y:.0})")))
        }
        "click" => {
            let p = input::click(input.x, input.y, input::Button::Left, 1)?;
            Ok(ToolOutput::new(format!(
                "clicked at ({:.0}, {:.0})",
                p.x, p.y
            )))
        }
        "double_click" => {
            let p = input::click(input.x, input.y, input::Button::Left, 2)?;
            Ok(ToolOutput::new(format!(
                "double-clicked at ({:.0}, {:.0})",
                p.x, p.y
            )))
        }
        "right_click" => {
            let p = input::click(input.x, input.y, input::Button::Right, 1)?;
            Ok(ToolOutput::new(format!(
                "right-clicked at ({:.0}, {:.0})",
                p.x, p.y
            )))
        }
        "drag" => {
            let (x, y) = require_xy(input)?;
            match (input.to_x, input.to_y) {
                (Some(tx), Some(ty)) => {
                    input::drag(x, y, tx, ty)?;
                    Ok(ToolOutput::new(format!(
                        "dragged from ({x:.0},{y:.0}) to ({tx:.0},{ty:.0})"
                    )))
                }
                _ => bail!("action='drag' requires `to_x` and `to_y`"),
            }
        }
        "scroll" => {
            let dx = input.dx.unwrap_or(0);
            let dy = input.dy.unwrap_or(0);
            if dx == 0 && dy == 0 {
                bail!("action='scroll' requires non-zero `dx` and/or `dy`");
            }
            input::scroll(input.x, input.y, dx, dy)?;
            Ok(ToolOutput::new(format!("scrolled dx={dx} dy={dy}")))
        }
        "type" => {
            let text = input
                .text
                .as_deref()
                .filter(|s| !s.is_empty())
                .context("action='type' requires non-empty `text`")?;
            input::type_text(text)?;
            Ok(ToolOutput::new(format!(
                "typed {} characters",
                text.chars().count()
            )))
        }
        "key" => {
            let keys = input
                .keys
                .as_deref()
                .filter(|s| !s.is_empty())
                .context("action='key' requires a `keys` chord, e.g. 'cmd+space'")?;
            input::key_chord(keys)?;
            Ok(ToolOutput::new(format!("pressed {keys}")))
        }
        "key_down" | "key_up" => {
            let keys = input
                .keys
                .as_deref()
                .filter(|s| !s.is_empty())
                .context("requires a `keys` value")?;
            input::key_hold(keys, action == "key_down")?;
            Ok(ToolOutput::new(format!("{action} {keys}")))
        }

        // ---- AX background actions (Tier 1) ----
        "find_element" => {
            let app = input
                .app
                .as_deref()
                .context("find_element requires `app`")?;
            ax::find_element(
                app,
                input.role.as_deref(),
                input.title.as_deref(),
                input.value.as_deref(),
                input.depth.unwrap_or(20),
            )
        }
        "element_at" => {
            let app = input.app.as_deref().context("element_at requires `app`")?;
            let (x, y) = require_xy(input)?;
            ax::element_at(app, x, y)
        }
        "press" => ax::press(&parse_element(input)?),
        "get_value" => ax::get_value(&parse_element(input)?),
        "set_value" => {
            let v = input
                .value
                .as_deref()
                .context("set_value requires `value`")?;
            ax::set_value(&parse_element(input)?, v)
        }
        "perform_action" => {
            let a = input
                .ax_action
                .as_deref()
                .context("perform_action requires `ax_action`")?;
            ax::perform_action(&parse_element(input)?, a)
        }
        "select_menu" => {
            let app = input.app.as_deref().context("select_menu requires `app`")?;
            let path = input
                .menu_path
                .as_ref()
                .context("select_menu requires `menu_path`")?;
            ax::select_menu(app, path)
        }

        // ---- windows / apps (Tier 2) ----
        "list_apps" => win::list_apps(),
        "list_windows" => win::list_windows(),
        "activate_app" => win::activate_app(req_app(input)?),
        "hide_app" => win::hide_app(req_app(input)?),
        "quit_app" => win::quit_app(req_app(input)?),
        "focus_window" => win::focus_window(req_app(input)?),
        "move_window" => {
            let (x, y) = require_xy(input)?;
            win::move_window(req_app(input)?, x, y)
        }
        "resize_window" => {
            let w = input.w.context("resize_window requires `w`")?;
            let h = input.h.context("resize_window requires `h`")?;
            win::resize_window(req_app(input)?, w, h)
        }
        "minimize_window" => win::minimize_window(req_app(input)?),
        "close_window" => win::close_window(req_app(input)?),

        // ---- clipboard / scripting / system (Tier 3/4) ----
        "get_clipboard" => sys::get_clipboard(),
        "set_clipboard" => {
            let t = input
                .text
                .as_deref()
                .context("set_clipboard requires `text`")?;
            sys::set_clipboard(t)
        }
        "run_applescript" => {
            let s = input
                .script
                .as_deref()
                .context("run_applescript requires `script`")?;
            sys::run_applescript(s)
        }
        "run_jxa" => {
            let s = input
                .script
                .as_deref()
                .context("run_jxa requires `script`")?;
            sys::run_jxa(s)
        }
        "wait_for" => {
            let app = input.app.as_deref().context("wait_for requires `app`")?;
            let c = input
                .contains
                .as_deref()
                .context("wait_for requires `contains`")?;
            sys::wait_for(app, c, input.timeout_ms.unwrap_or(10_000))
        }
        "notify" => {
            let t = input.text.as_deref().context("notify requires `text`")?;
            sys::notify(t, input.title.as_deref())
        }
        "system_state" => sys::system_state(),
        "set_brightness" => {
            let l = input
                .level
                .context("set_brightness requires `level` (0..1)")?;
            sys::set_brightness(l)
        }

        other => bail!(
            "Unknown macos_computer_use action: {other}. Call action='discover' (category='all') to list every action."
        ),
    }
}

#[cfg(target_os = "macos")]
fn require_xy(input: &ComputerInput) -> Result<(f64, f64)> {
    match (input.x, input.y) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => bail!("action='{}' requires both `x` and `y`", input.action),
    }
}

#[cfg(target_os = "macos")]
fn req_app(input: &ComputerInput) -> Result<&str> {
    input
        .app
        .as_deref()
        .with_context(|| format!("action='{}' requires `app`", input.action))
}

#[cfg(target_os = "macos")]
fn parse_element(input: &ComputerInput) -> Result<ax::ElementHandle> {
    let raw = input.element.clone().context(
        "this action requires an `element` handle {app, path:[...]} from find_element/ui",
    )?;
    serde_json::from_value(raw).context("invalid `element` handle")
}

#[cfg(test)]
mod coverage_tests;
#[cfg(test)]
mod tests;
