mod platform;
mod target;
mod tree;

#[cfg(test)]
mod tests;

pub(super) use platform::{capture_trusted, probe_accessibility, trusted};

use super::{ToolOutput, screen};
use anyhow::ensure;
use platform::{AxError, Element};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use target::{Scope, Target, hash_value};
use tree::{Snapshot, collect};

const WINDOW_LIMIT: usize = 128;
const MAX_CHILDREN: usize = 500;

#[derive(Debug)]
struct Failure {
    reason: &'static str,
    ax_code: Option<i32>,
}

impl Failure {
    fn new(reason: &'static str) -> Self {
        Self {
            reason,
            ax_code: None,
        }
    }
}

impl From<AxError> for Failure {
    fn from(error: AxError) -> Self {
        Self {
            reason: "ax_error",
            ax_code: Some(error.0),
        }
    }
}

type NativeResult<T> = std::result::Result<T, Failure>;

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum View {
    Ax,
    Image,
    AxImage,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Verb {
    Press,
    SetValue,
    Type,
    MenuPath,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Predicate {
    Exists,
    ValueChanged,
    Focused,
    WindowClosed,
}

#[derive(Deserialize)]
pub(super) struct Observe {
    #[serde(flatten)]
    scope: Scope,
    view: Option<View>,
    query: Option<String>,
    budget: Option<usize>,
}

#[derive(Deserialize)]
pub(super) struct Act {
    snapshot_id: String,
    element_ref: String,
    verb: Verb,
    value: Option<String>,
    text: Option<String>,
    menu_path: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(super) struct Wait {
    snapshot_id: String,
    element_ref: Option<String>,
    predicate: Predicate,
    query: Option<String>,
    timeout_ms: Option<u64>,
}

pub(super) enum Request {
    Observe(Observe),
    Act(Act),
    Wait(Wait),
}

impl Request {
    pub(super) fn parse(action: &str, input: Value) -> anyhow::Result<Option<Self>> {
        Ok(Some(match action {
            "observe" => {
                let q: Observe = serde_json::from_value(input)?;
                q.scope.validate()?;
                ensure!(
                    q.query
                        .as_ref()
                        .is_none_or(|q| !q.trim().is_empty() && q.len() <= 256),
                    "query must contain 1–256 bytes"
                );
                Self::Observe(q)
            }
            "act" => {
                ensure!(
                    input.get("app").is_none() && input.get("window_id").is_none(),
                    "act targets only the supplied snapshot and element reference"
                );
                let q: Act = serde_json::from_value(input)?;
                match q.verb {
                    Verb::Press => {}
                    Verb::SetValue => ensure!(
                        q.value.as_ref().is_some_and(|s| s.len() <= 64_000),
                        "set_value requires value up to 64000 bytes"
                    ),
                    Verb::Type => ensure!(
                        q.text
                            .as_ref()
                            .is_some_and(|s| !s.is_empty() && s.len() <= 64_000),
                        "type requires non-empty text up to 64000 bytes"
                    ),
                    Verb::MenuPath => ensure!(
                        q.menu_path
                            .as_ref()
                            .is_some_and(|p| (2..=8).contains(&p.len())
                                && p.iter().all(|s| !s.is_empty() && s.len() <= 256)),
                        "menu_path requires 2–8 non-empty titles"
                    ),
                }
                Self::Act(q)
            }
            "wait" => {
                ensure!(
                    input.get("app").is_none() && input.get("window_id").is_none(),
                    "wait targets only the supplied snapshot"
                );
                let q: Wait = serde_json::from_value(input)?;
                match q.predicate {
                    Predicate::Exists => ensure!(
                        q.query
                            .as_ref()
                            .is_some_and(|s| !s.trim().is_empty() && s.len() <= 256),
                        "exists requires query of 1–256 bytes"
                    ),
                    Predicate::Focused | Predicate::ValueChanged => {
                        ensure!(q.element_ref.is_some(), "predicate requires element_ref")
                    }
                    Predicate::WindowClosed => {}
                }
                Self::Wait(q)
            }
            _ => return Ok(None),
        }))
    }

    pub(super) fn execute(self, slots: &mut Snapshots) -> ToolOutput {
        // Even the image route uses AX to pin a window before capturing it.
        if !trusted() {
            let reason = "accessibility_permission_required";
            return match self {
                Self::Observe(_) => failure(Failure::new(reason)),
                Self::Act(_) => {
                    receipt(json!({"delivery":"not_sent", "effect":"unverified", "reason":reason}))
                }
                Self::Wait(_) => receipt(json!({"matched":false, "reason":reason})),
            };
        }
        match self {
            Self::Observe(request) => observe(request, slots).unwrap_or_else(failure),
            Self::Act(request) => act(request, slots),
            Self::Wait(request) => wait(request, slots),
        }
    }
}

#[derive(Default)]
pub(super) struct Snapshots(Option<Snapshot>);

impl Snapshots {
    pub(super) fn discard_expired(&mut self) {
        if self.0.as_ref().is_some_and(Snapshot::expired) {
            self.0 = None;
        }
    }

    fn publish(
        &mut self,
        target: Target,
        query: Option<&str>,
        budget: usize,
        ax: bool,
    ) -> NativeResult<Value> {
        let (snapshot, result) = collect(target, query, budget, ax)?;
        self.0 = Some(snapshot);
        Ok(result)
    }

    fn refresh(&mut self) -> Option<Value> {
        let old = self.0.take()?;
        if !old.target.live().ok()? {
            return None;
        }
        self.publish(old.target, old.query.as_deref(), old.budget, true)
            .ok()
    }
}

fn receipt(mut data: Value) -> ToolOutput {
    data["executor_pid"] = json!(std::process::id());
    ToolOutput::new(data.to_string()).with_metadata(data)
}

fn failure(error: Failure) -> ToolOutput {
    receipt(json!({"reason": error.reason, "ax_code": error.ax_code}))
}

fn observe(request: Observe, slots: &mut Snapshots) -> NativeResult<ToolOutput> {
    let target = request.scope.resolve()?;
    let view = request.view.unwrap_or(View::Ax);
    let image = if matches!(view, View::Image | View::AxImage) {
        if !capture_trusted() {
            return Err(Failure::new("screen_recording_permission_required"));
        }
        let id = target.image_id()?;
        let image =
            screen::window_screenshot(i64::from(id)).map_err(|_| Failure::new("capture_failed"))?;
        if target.image_id()? != id {
            return Err(Failure::new("window_changed"));
        }
        Some(image)
    } else {
        None
    };
    let mut state = slots.publish(
        target,
        request.query.as_deref(),
        request.budget.unwrap_or(4500).clamp(256, 12_000),
        !matches!(view, View::Image),
    )?;
    if let Some(image) = &image {
        state["frame"] = image.metadata.clone().unwrap_or(Value::Null);
    }
    let mut output = receipt(state);
    if let Some(image) = image {
        output.images = image.images;
    }
    Ok(output)
}

fn act(request: Act, slots: &mut Snapshots) -> ToolOutput {
    let result = (|| {
        let snapshot = slots
            .0
            .as_ref()
            .filter(|s| !s.expired() && s.id == request.snapshot_id)
            .ok_or_else(|| Failure::new("stale_ref"))?;
        let bound = snapshot
            .refs
            .get(&request.element_ref)
            .ok_or_else(|| Failure::new("stale_ref"))?;
        snapshot.target.validate(bound)?;
        let element = &bound.element;
        if element.boolean("AXEnabled")? == Some(false) {
            return Err(Failure::new("element_disabled"));
        }
        match request.verb {
            Verb::Press => {
                let before = element.value()?;
                snapshot.target.validate(bound)?;
                press(element, before)
            }
            Verb::SetValue => {
                let value = request.value.as_deref().expect("validated input");
                if !element.settable("AXValue")? {
                    return Err(Failure::new("background_unavailable"));
                }
                snapshot.target.validate(bound)?;
                Ok(deliver(element.set("AXValue", value), || {
                    element.value().ok().flatten().as_deref() == Some(value)
                }))
            }
            Verb::Type => {
                if !element.settable("AXSelectedText")? {
                    return Err(Failure::new("background_unavailable"));
                }
                let before = element
                    .string("AXValue")?
                    .ok_or_else(|| Failure::new("background_unavailable"))?;
                let range = element
                    .selection()?
                    .ok_or_else(|| Failure::new("background_unavailable"))?;
                let input = request.text.as_deref().expect("validated input");
                let expected = replace_utf16(&before, range.location, range.length, input)
                    .ok_or_else(|| Failure::new("background_unavailable"))?;
                snapshot.target.validate(bound)?;
                Ok(deliver(element.set("AXSelectedText", input), || {
                    element.string("AXValue").ok().flatten().as_deref() == Some(expected.as_str())
                }))
            }
            Verb::MenuPath => {
                if element != &snapshot.target.window {
                    return Err(Failure::new("menu_requires_window_ref"));
                }
                let path = request.menu_path.as_deref().expect("validated input");
                let menu_item = snapshot.target.menu_item(path)?;
                let before = menu_item.value()?;
                snapshot.target.validate(bound)?;
                if snapshot.target.menu_item(path)? != menu_item {
                    return Err(Failure::new("stale_ref"));
                }
                press(&menu_item, before)
            }
        }
    })();
    let (delivery, effect, reason, ax_code) = match result {
        Ok((status, effect, error)) => (
            status,
            effect,
            error.map(|_| "ax_action_error"),
            error.map(|e| e.0),
        ),
        Err(e) => ("not_sent", "unverified", Some(e.reason), e.ax_code),
    };
    let next_state = slots.refresh();
    receipt(
        json!({"delivery": delivery, "effect": effect, "reason": reason, "ax_code": ax_code, "next_state": next_state}),
    )
}

fn press(element: &Element, before: Option<String>) -> NativeResult<Delivery> {
    if !element.actions()?.iter().any(|action| action == "AXPress") {
        return Err(Failure::new("background_unavailable"));
    }
    Ok(deliver(element.press(), || {
        element
            .value()
            .ok()
            .flatten()
            .is_some_and(|after| Some(after) != before)
    }))
}

type Delivery = (&'static str, &'static str, Option<AxError>);

fn deliver(result: std::result::Result<(), AxError>, verify: impl FnOnce() -> bool) -> Delivery {
    match result {
        Ok(()) => (
            "sent",
            if verify() { "verified" } else { "unverified" },
            None,
        ),
        Err(err) => (
            if err.uncertain_delivery() {
                "unknown"
            } else {
                "not_sent"
            },
            "unverified",
            Some(err),
        ),
    }
}

fn replace_utf16(value: &str, start: isize, length: isize, insert: &str) -> Option<String> {
    let start = usize::try_from(start).ok()?;
    let end = start.checked_add(usize::try_from(length).ok()?)?;
    let mut units: Vec<_> = value.encode_utf16().collect();
    if end > units.len() {
        return None;
    }
    let boundary = |index| {
        !units
            .get(index)
            .is_some_and(|u| (0xDC00..=0xDFFF).contains(u))
    };
    if !boundary(start) || !boundary(end) {
        return None;
    }
    units.splice(start..end, insert.encode_utf16());
    String::from_utf16(&units).ok()
}

fn wait(request: Wait, slots: &mut Snapshots) -> ToolOutput {
    let result = (|| {
        let snapshot = slots
            .0
            .as_ref()
            .filter(|s| !s.expired() && s.id == request.snapshot_id)
            .ok_or_else(|| Failure::new("stale_ref"))?;
        let bound = request
            .element_ref
            .as_ref()
            .map(|e| {
                snapshot
                    .refs
                    .get(e)
                    .ok_or_else(|| Failure::new("stale_ref"))
            })
            .transpose()?;
        let end =
            Instant::now() + Duration::from_millis(request.timeout_ms.unwrap_or(5000).min(15_000));
        loop {
            let live = snapshot.target.live()?;
            if !live && !matches!(request.predicate, Predicate::WindowClosed) {
                return Err(Failure::new("window_closed"));
            }
            let matched = match request.predicate {
                Predicate::WindowClosed => !live,
                Predicate::Exists => tree::exists(
                    &snapshot.target,
                    request.query.as_deref().expect("validated input"),
                    end,
                )?,
                Predicate::ValueChanged | Predicate::Focused => {
                    let bound = bound.expect("validated input");
                    snapshot.target.validate(bound)?;
                    if matches!(request.predicate, Predicate::Focused) {
                        bound.element.boolean("AXFocused")? == Some(true)
                    } else {
                        bound.element.value()?.as_deref().map(hash_value) != bound.value_hash
                    }
                }
            };
            if matched {
                return Ok(true);
            }
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            std::thread::sleep(remaining.min(Duration::from_millis(150)));
        }
    })();
    let (matched, reason, ax_code) = match result {
        Ok(matched) => (matched, (!matched).then_some("timeout"), None),
        Err(e) => (false, Some(e.reason), e.ax_code),
    };
    let next_state = if matched || reason == Some("stale_ref") {
        slots.refresh()
    } else {
        None
    };
    receipt(json!({"matched":matched, "reason":reason, "ax_code":ax_code, "next_state":next_state}))
}
