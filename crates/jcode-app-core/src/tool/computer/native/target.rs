use super::platform::{self, Element, Process, WindowInfo};
use super::{Failure, MAX_CHILDREN, NativeResult, WINDOW_LIMIT};
use anyhow::ensure;
use core_graphics::geometry::CGRect;
use serde::Deserialize;
use serde_json::{Value, json};
use std::hash::{DefaultHasher, Hash, Hasher};

#[derive(Deserialize)]
pub(super) struct Scope {
    app: Option<String>,
    window_id: Option<u32>,
}

impl Scope {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.window_id.is_some() || self.app.is_some(),
            "observe requires app or window_id"
        );
        ensure!(self.window_id != Some(0), "window_id must be positive");
        ensure!(
            self.app
                .as_ref()
                .is_none_or(|a| !a.trim().is_empty() && a.len() <= 256),
            "app must be an owner name or pid:<number>"
        );
        Ok(())
    }

    pub(super) fn resolve(&self) -> NativeResult<Target> {
        let windows = platform::windows().ok_or_else(|| Failure::new("no_gui_session"))?;
        let by_id = self
            .window_id
            .map(|id| {
                windows
                    .iter()
                    .find(|w| w.id == id)
                    .ok_or_else(|| Failure::new("window_not_found"))
            })
            .transpose()?;
        let pid_hint = self
            .app
            .as_deref()
            .and_then(|app| app.strip_prefix("pid:"))
            .and_then(|pid| pid.parse::<i32>().ok());
        let app_matches = |w: &WindowInfo| match (pid_hint, self.app.as_deref()) {
            (Some(pid), _) => pid == w.pid,
            (None, Some(name)) => w
                .owner
                .as_deref()
                .is_some_and(|owner| owner.eq_ignore_ascii_case(name)),
            (None, None) => true,
        };
        let pid = if let Some(window) = by_id {
            if !app_matches(window) {
                return Err(Failure::new("window_app_mismatch"));
            }
            window.pid
        } else if let Some(pid) = pid_hint {
            pid
        } else {
            let mut pids: Vec<_> = windows
                .iter()
                .filter(|w| app_matches(w))
                .map(|w| w.pid)
                .collect();
            pids.sort_unstable();
            pids.dedup();
            match pids.as_slice() {
                [pid] => *pid,
                [] => return Err(Failure::new("app_not_found")),
                _ => return Err(Failure::new("ambiguous_app")),
            }
        };
        let process = Process::own(pid)
            .filter(|_| pid > 0)
            .ok_or_else(|| Failure::new("app_not_available"))?;
        let app = Element::application(pid)?;
        let owner = windows
            .iter()
            .find(|w| w.pid == pid && w.owner.is_some())
            .and_then(|w| w.owner.clone());
        if owner.as_deref().is_some_and(|name| {
            matches!(
                name,
                "SecurityAgent" | "CoreServicesUIAgent" | "loginwindow" | "authorizationhost"
            )
        }) {
            return Err(Failure::new("protected_app"));
        }
        let candidates = app.elements("AXWindows", WINDOW_LIMIT + 1)?;
        if candidates.len() > WINDOW_LIMIT {
            return Err(Failure::new("ambiguous_window"));
        }
        let window = if let Some(exact) = by_id {
            // Public AX does not expose a CoreGraphics window id. Match only
            // a unique pair across both catalogs, never a nearest candidate.
            if matching_cg(&windows, pid, exact.bounds).count() != 1 {
                return Err(Failure::new("ambiguous_window"));
            }
            let mut matches = Vec::new();
            for w in &candidates {
                if w.bounds()?
                    .is_some_and(|bounds| same_rect(bounds, exact.bounds))
                {
                    matches.push(w);
                }
            }
            match matches.as_slice() {
                [window] => (*window).clone(),
                [] => return Err(Failure::new("window_not_found")),
                _ => return Err(Failure::new("ambiguous_window")),
            }
        } else {
            app.element("AXFocusedWindow")?
                .or(app.element("AXMainWindow")?)
                .filter(|w| candidates.contains(w))
                .or_else(|| (candidates.len() == 1).then(|| candidates[0].clone()))
                .ok_or_else(|| {
                    Failure::new(if candidates.is_empty() {
                        "window_not_found"
                    } else {
                        "ambiguous_window"
                    })
                })?
        };
        let cg_id = if let Some(exact) = by_id {
            Some(exact.id)
        } else if let Some(bounds) = window.bounds()? {
            let mut matches = matching_cg(&windows, pid, bounds);
            let first = matches.next();
            if matches.next().is_none() {
                first.map(|w| w.id)
            } else {
                None
            }
        } else {
            None
        };
        let target = Target {
            process,
            app,
            window,
            owner,
            cg_id,
        };
        if by_id.is_some() {
            target.image_id()?;
        } else if !target.live()? {
            return Err(Failure::new("window_changed"));
        }
        Ok(target)
    }
}

fn matching_cg(
    windows: &[WindowInfo],
    pid: i32,
    bounds: CGRect,
) -> impl Iterator<Item = &WindowInfo> {
    windows
        .iter()
        .filter(move |w| w.pid == pid && same_rect(w.bounds, bounds))
}

fn same_rect(a: CGRect, b: CGRect) -> bool {
    [
        (a.origin.x, b.origin.x),
        (a.origin.y, b.origin.y),
        (a.size.width, b.size.width),
        (a.size.height, b.size.height),
    ]
    .into_iter()
    .all(|(a, b)| a.is_finite() && b.is_finite() && (a - b).abs() < 1.0)
}

pub(super) struct Target {
    process: Process,
    pub(super) app: Element,
    pub(super) window: Element,
    owner: Option<String>,
    cg_id: Option<u32>,
}

impl Target {
    pub(super) fn live(&self) -> NativeResult<bool> {
        if !self.process.live() {
            return Ok(false);
        }
        match self.app.elements("AXWindows", WINDOW_LIMIT + 1) {
            Ok(windows) if windows.len() <= WINDOW_LIMIT => Ok(windows.contains(&self.window)),
            Ok(_) => Err(Failure::new("ambiguous_window")),
            Err(e) => Err(e.into()),
        }
    }

    pub(super) fn describe(&self) -> Value {
        json!({"pid":self.process.pid, "app":self.owner, "window_id":self.cg_id})
    }

    pub(super) fn image_id(&self) -> NativeResult<u32> {
        let id = self.cg_id.ok_or_else(|| Failure::new("ambiguous_window"))?;
        if !self.live()? {
            return Err(Failure::new("window_changed"));
        }
        let bounds = self
            .window
            .bounds()?
            .ok_or_else(|| Failure::new("window_changed"))?;
        let windows = platform::windows().ok_or_else(|| Failure::new("no_gui_session"))?;
        let mut matches = matching_cg(&windows, self.process.pid, bounds);
        if !matches.next().is_some_and(|w| w.id == id) || matches.next().is_some() {
            return Err(Failure::new("window_changed"));
        }
        let neighbors = self.app.elements("AXWindows", WINDOW_LIMIT + 1)?;
        if neighbors.len() > WINDOW_LIMIT {
            return Err(Failure::new("ambiguous_window"));
        }
        for w in neighbors {
            if w != self.window && w.bounds()?.is_some_and(|other| same_rect(bounds, other)) {
                return Err(Failure::new("ambiguous_window"));
            }
        }
        Ok(id)
    }

    pub(super) fn validate(&self, bound: &Bound) -> NativeResult<()> {
        if !self.live()? {
            return Err(Failure::new("stale_ref"));
        }
        match self.validate_member(bound) {
            Err(e)
                if e.ax_code
                    .is_some_and(|n| platform::AxError(n).invalid_element()) =>
            {
                Err(Failure::new("stale_ref"))
            }
            result => result,
        }
    }

    fn validate_member(&self, bound: &Bound) -> NativeResult<()> {
        if Identity::read(&bound.element)?.as_ref() != Some(&bound.identity)
            || bound.identity.secure()
        {
            return Err(Failure::new("stale_ref"));
        }
        let mut element = bound.element.clone();
        for _ in 0..=24 {
            if element == self.window {
                return Ok(());
            }
            let Some(parent) = element.element("AXParent")? else {
                break;
            };
            if !parent
                .elements("AXChildren", MAX_CHILDREN)?
                .contains(&element)
                && !parent
                    .elements("AXSheets", MAX_CHILDREN)?
                    .contains(&element)
            {
                break;
            }
            element = parent;
        }
        Err(Failure::new("stale_ref"))
    }

    pub(super) fn menu_item(&self, titles: &[String]) -> NativeResult<Element> {
        // Menu items are application-scoped. Do not deliver them when a
        // different window could receive the command.
        let windows = self.app.elements("AXWindows", 2)?;
        if windows.as_slice() != [self.window.clone()]
            || self.app.element("AXFocusedWindow")?.as_ref() != Some(&self.window)
        {
            return Err(Failure::new("background_unavailable"));
        }
        let mut item = self
            .app
            .element("AXMenuBar")?
            .ok_or_else(|| Failure::new("background_unavailable"))?;
        for (index, title) in titles.iter().enumerate() {
            let children = item.elements("AXChildren", MAX_CHILDREN + 1)?;
            if children.len() > MAX_CHILDREN {
                return Err(Failure::new("ambiguous_menu_item"));
            }
            let pool = if index == 0 {
                children
            } else {
                let mut menus = Vec::new();
                for child in children {
                    if child.string("AXRole")?.as_deref() == Some("AXMenu") {
                        menus.push(child);
                    }
                }
                match menus.as_slice() {
                    [menu] => menu.elements("AXChildren", MAX_CHILDREN + 1)?,
                    _ => return Err(Failure::new("background_unavailable")),
                }
            };
            if pool.len() > MAX_CHILDREN {
                return Err(Failure::new("ambiguous_menu_item"));
            }
            let mut matches = Vec::new();
            for child in pool {
                if child.string("AXTitle")?.as_ref() == Some(title) {
                    matches.push(child);
                }
            }
            item = match matches.len() {
                0 => return Err(Failure::new("menu_item_not_found")),
                1 => matches.pop().expect("one menu match"),
                _ => return Err(Failure::new("ambiguous_menu_item")),
            };
        }
        if item.boolean("AXEnabled")? != Some(true) {
            return Err(Failure::new("element_disabled"));
        }
        Ok(item)
    }
}

pub(super) struct Bound {
    pub(super) element: Element,
    pub(super) identity: Identity,
    pub(super) value_hash: Option<u64>,
}

pub(super) fn hash_value(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[derive(PartialEq, Eq)]
pub(super) struct Identity {
    pub(super) role: String,
    subrole: Option<String>,
    id: Option<String>,
    pub(super) label: Option<String>,
}

impl Identity {
    pub(super) fn read(element: &Element) -> NativeResult<Option<Self>> {
        let Some(role) = element.string("AXRole")? else {
            return Ok(None);
        };
        let subrole = element.string("AXSubrole")?;
        let id = element.string("AXIdentifier")?;
        let title = element.string("AXTitle")?.filter(|t| !t.is_empty());
        let label = if title.is_some() {
            title
        } else {
            element.string("AXDescription")?.filter(|t| !t.is_empty())
        };
        Ok(Some(Self {
            role,
            subrole,
            id,
            label,
        }))
    }

    pub(super) fn secure(&self) -> bool {
        self.role == "AXSecureTextField"
            || self.subrole.as_deref() == Some("AXSecureTextField")
            || self.role == "AXProtectedContent"
    }
}
