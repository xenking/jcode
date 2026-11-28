//! Native macOS "computer use" tool.
//!
//! The desktop analog of the `browser` tool: a single `action`-dispatched tool
//! that lets the agent see the screen and control the macOS GUI.
//!
//! ## Mechanisms and visibility
//!
//! - **Coordinate input** (`click`/`type`/`key`/`scroll`/`drag`) uses Core
//!   Graphics CGEvents on the shared HID stream, so it is *visible*: it moves the
//!   real cursor and types into the focused app.
//! - **Accessibility actions** (`press`/`set_value`/`select_menu`/...) and
//!   **scripting** (`run_applescript`) act on apps *by reference*, so they can
//!   work in the **background** without moving the cursor.
//!
//! By default the tool prefers non-disruptive mechanisms and restraint: because
//! this runs on the user's own live machine, the agent should act only on the
//! requested task, prefer background AX/scripting over moving the cursor or
//! stealing focus, and never take proactive control of the desktop. This policy
//! is conveyed to the model via the tool description and the `discover` output.
//!
//! ## Progressive disclosure
//!
//! Only a small set of common actions is described in the always-on schema to
//! keep prompt cost low. The full action set is fetched on demand via
//! `action="discover"` with a `category`.
//!
use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

#[cfg(target_os = "macos")]
mod broker;

#[derive(Default)]
pub struct ComputerTool;

impl ComputerTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for ComputerTool {
    fn name(&self) -> &str {
        "macos_computer_use"
    }

    fn description(&self) -> &str {
        "Control macOS through the open local Jcode Desktop session with user consent: screenshot, ocr, ui tree, click, type, keys, background \
         Accessibility actions, apps, windows, clipboard, AppleScript. Coordinates are in points. \
         Prefer background AX over moving the cursor, and act only on the requested task."
    }

    fn parameters_schema(&self) -> Value {
        // Progressive disclosure: only the common actions + discover are spelled
        // out here to keep always-on prompt cost low (~370 tokens). Advanced
        // actions and their params are returned by action="discover".
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "description": "Prefer observe, act, wait (discover category=ax). Legacy: screenshot, ui, click, type, setup."
                },
                "category": {
                    "type": "string",
                    "enum": ["mouse","keyboard","observe","ax","windows","apps","clipboard","scripting","system","setup","all"],
                    "description": "For action='discover': which group to return full action specs for."
                },
                "x": { "type": "number", "description": "Screen X in points (top-left origin)." },
                "y": { "type": "number", "description": "Screen Y in points." },
                "text": { "type": "string", "description": "Text for type / set_clipboard / notify." },
                "keys": { "type": "string", "description": "Key chord, e.g. cmd+space, return, esc, ctrl+shift+t." },
                "app": { "type": "string", "description": "Target app/process name (AX, windows, scripting scope)." },
                "role": { "type": "string", "description": "AX role filter for find_element, e.g. AXButton." },
                "title": { "type": "string", "description": "AX title/label substring for find_element." },
                "value": { "type": "string", "description": "Value to match (find_element) or set (set_value)." },
                "element": {
                    "type": "object",
                    "description": "{app, path:[child indices]} from find_element/ui.",
                    "properties": {
                        "app": { "type": "string" },
                        "path": { "type": "array", "items": { "type": "integer" } }
                    }
                },
                "script": { "type": "string", "description": "AppleScript (run_applescript) or JS (run_jxa) source." },
                "depth": { "type": "integer", "description": "Max AX tree depth for ui/find_element (default 12)." },
                "to_x": { "type": "number" },
                "to_y": { "type": "number" },
                "dx": { "type": "integer" },
                "dy": { "type": "integer" },
                "w": { "type": "number" },
                "h": { "type": "number" },
                "window_id": { "type": "integer", "description": "Window id (from list_windows)." },
                "view": { "type": "string", "enum": ["ax","image","ax_image"] },
                "query": { "type": "string" },
                "budget": { "type": "integer" },
                "snapshot_id": { "type": "string" },
                "element_ref": { "type": "string" },
                "verb": { "type": "string", "enum": ["press","set_value","type","menu_path"] },
                "predicate": { "type": "string", "enum": ["exists","value_changed","focused","window_closed"] },
                "menu_path": { "type": "array", "items": { "type": "string" }, "description": "select_menu path, e.g. [\"File\",\"Save\"]." },
                "ax_action": { "type": "string", "description": "perform_action AX action, e.g. AXShowMenu." },
                "contains": { "type": "string", "description": "wait_for: substring to await." },
                "timeout_ms": { "type": "integer" },
                "region": { "type": "array", "items": { "type": "number" }, "description": "ocr region [x,y,w,h]; omit for full screen." },
                "level": { "type": "number", "description": "set_brightness 0..1." },
                "dry_run": { "type": "boolean", "description": "Mutating actions: report intended action without doing it." }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        #[cfg(target_os = "macos")]
        {
            match jcode_computer::Request::parse(input.clone())? {
                jcode_computer::Request::Local(output) => Ok(output),
                jcode_computer::Request::Desktop(_) => broker::execute(ctx, input).await,
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (ctx, input);
            anyhow::bail!("The macos_computer_use tool is only supported on macOS.")
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests;
