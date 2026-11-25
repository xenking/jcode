use super::*;

pub(crate) fn set_visible_expand_edit_badge_rect(rect: Option<Rect>) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_VISIBLE_EXPAND_EDIT_BADGE_RECT.with(|state| state.set(rect));
        return;
    }
    #[cfg(not(test))]
    {
        let mut state = visible_expand_edit_badge_rect_state()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *state = rect;
    }
}

pub(crate) fn visible_expand_edit_badge_at(column: u16, row: u16) -> bool {
    #[cfg(test)]
    let rect = TEST_VISIBLE_EXPAND_EDIT_BADGE_RECT.with(Cell::get);
    #[cfg(not(test))]
    let rect = *visible_expand_edit_badge_rect_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    rect.is_some_and(|rect| {
        column >= rect.x
            && column < rect.x.saturating_add(rect.width)
            && row >= rect.y
            && row < rect.y.saturating_add(rect.height)
    })
}

pub(crate) fn set_visible_expand_edit_badge(visible: bool, line: Option<usize>) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_VISIBLE_EXPAND_EDIT_BADGE.with(|state| state.set(visible));
        TEST_VISIBLE_EXPAND_EDIT_BADGE_LINE.with(|state| state.set(line));
        return;
    }
    #[cfg(not(test))]
    {
        let mut visible_state = match visible_expand_edit_badge_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *visible_state = visible;

        let mut line_state = match visible_expand_edit_badge_line_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *line_state = line;
    }
}

pub(crate) fn visible_expand_edit_badge() -> bool {
    #[cfg(test)]
    {
        return TEST_VISIBLE_EXPAND_EDIT_BADGE.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        let state = match visible_expand_edit_badge_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *state
    }
}

pub(crate) fn visible_expand_edit_badge_line() -> Option<usize> {
    #[cfg(test)]
    {
        return TEST_VISIBLE_EXPAND_EDIT_BADGE_LINE.with(Cell::get);
    }
    #[cfg(not(test))]
    {
        let state = match visible_expand_edit_badge_line_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *state
    }
}

pub(super) fn set_visible_copy_targets(targets: Vec<VisibleCopyTarget>) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_VISIBLE_COPY_TARGETS.with(|state| {
            *state.borrow_mut() = targets;
        });
        return;
    }
    #[cfg(not(test))]
    {
        let mut state = match visible_copy_targets_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *state = targets;
    }
}

pub(crate) fn visible_copy_target_for_key(key: char) -> Option<VisibleCopyTarget> {
    #[cfg(test)]
    {
        TEST_VISIBLE_COPY_TARGETS.with(|state| {
            state
                .borrow()
                .iter()
                .find(|target| target.key.eq_ignore_ascii_case(&key))
                .cloned()
        })
    }
    #[cfg(not(test))]
    {
        let state = match visible_copy_targets_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state
            .iter()
            .find(|target| target.key.eq_ignore_ascii_case(&key))
            .cloned()
    }
}

pub(crate) fn visible_copy_target_at(column: u16, row: u16) -> Option<VisibleCopyTarget> {
    let contains = |target: &&VisibleCopyTarget| {
        target.badge_rect.is_some_and(|rect| {
            column >= rect.x
                && column < rect.x.saturating_add(rect.width)
                && row >= rect.y
                && row < rect.y.saturating_add(rect.height)
        })
    };
    #[cfg(test)]
    {
        TEST_VISIBLE_COPY_TARGETS.with(|state| state.borrow().iter().find(contains).cloned())
    }
    #[cfg(not(test))]
    {
        let state = match visible_copy_targets_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.iter().find(contains).cloned()
    }
}

#[derive(Clone, Copy)]
pub(super) struct PromptViewportAnimation {
    pub(super) line_idx: usize,
    pub(super) start_ms: u64,
}

#[derive(Clone, Copy, Default)]
pub(super) struct PromptViewportState {
    pub(super) initialized: bool,
    pub(super) last_visible_start: usize,
    pub(super) last_visible_end: usize,
    pub(super) active: Option<PromptViewportAnimation>,
}

pub(super) const PROMPT_ENTRY_ANIMATION_MS: u64 = 450;

#[cfg(not(test))]
pub(super) static PROMPT_VIEWPORT_STATE: OnceLock<Mutex<PromptViewportState>> = OnceLock::new();

#[cfg(not(test))]
pub(super) fn prompt_viewport_state() -> &'static Mutex<PromptViewportState> {
    PROMPT_VIEWPORT_STATE.get_or_init(|| Mutex::new(PromptViewportState::default()))
}

pub(super) fn active_prompt_entry_animation(now_ms: u64) -> Option<PromptViewportAnimation> {
    #[cfg(test)]
    {
        TEST_PROMPT_VIEWPORT_STATE.with(|state| {
            let mut state = state.borrow_mut();
            if let Some(anim) = state.active {
                if now_ms.saturating_sub(anim.start_ms) <= PROMPT_ENTRY_ANIMATION_MS {
                    return Some(anim);
                }
                state.active = None;
            }
            None
        })
    }
    #[cfg(not(test))]
    {
        let mut state = match prompt_viewport_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if let Some(anim) = state.active {
            if now_ms.saturating_sub(anim.start_ms) <= PROMPT_ENTRY_ANIMATION_MS {
                return Some(anim);
            }
            state.active = None;
        }
        None
    }
}

pub(super) fn record_prompt_viewport(visible_start: usize, visible_end: usize) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_PROMPT_VIEWPORT_STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.initialized = true;
            state.last_visible_start = visible_start;
            state.last_visible_end = visible_end;
            state.active = None;
        });
        return;
    }
    #[cfg(not(test))]
    {
        let mut state = match prompt_viewport_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.initialized = true;
        state.last_visible_start = visible_start;
        state.last_visible_end = visible_end;
        state.active = None;
    }
}

pub(super) fn update_prompt_entry_animation(
    user_prompt_lines: &[usize],
    visible_start: usize,
    visible_end: usize,
    now_ms: u64,
) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_PROMPT_VIEWPORT_STATE.with(|state| {
            let mut state = state.borrow_mut();

            if !state.initialized {
                state.initialized = true;
                state.last_visible_start = visible_start;
                state.last_visible_end = visible_end;
                return;
            }

            let prev_visible_start = state.last_visible_start;
            let prev_visible_end = state.last_visible_end;
            let viewport_changed =
                prev_visible_start != visible_start || prev_visible_end != visible_end;

            if let Some(anim) = state.active {
                let still_fresh = now_ms.saturating_sub(anim.start_ms) <= PROMPT_ENTRY_ANIMATION_MS;
                let still_visible = anim.line_idx >= visible_start && anim.line_idx < visible_end;
                if still_fresh && still_visible {
                    state.last_visible_start = visible_start;
                    state.last_visible_end = visible_end;
                    return;
                }
                if !still_fresh || !still_visible {
                    state.active = None;
                }
            }

            if viewport_changed && state.active.is_none() {
                let newly_visible = user_prompt_lines.iter().copied().find(|line| {
                    *line >= visible_start
                        && *line < visible_end
                        && (*line < prev_visible_start || *line >= prev_visible_end)
                });
                if let Some(line_idx) = newly_visible {
                    state.active = Some(PromptViewportAnimation {
                        line_idx,
                        start_ms: now_ms,
                    });
                }
            }

            state.last_visible_start = visible_start;
            state.last_visible_end = visible_end;
        });
        return;
    }
    #[cfg(not(test))]
    {
        let mut state = match prompt_viewport_state().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        if !state.initialized {
            state.initialized = true;
            state.last_visible_start = visible_start;
            state.last_visible_end = visible_end;
            return;
        }

        let prev_visible_start = state.last_visible_start;
        let prev_visible_end = state.last_visible_end;
        let viewport_changed =
            prev_visible_start != visible_start || prev_visible_end != visible_end;

        if let Some(anim) = state.active {
            let still_fresh = now_ms.saturating_sub(anim.start_ms) <= PROMPT_ENTRY_ANIMATION_MS;
            let still_visible = anim.line_idx >= visible_start && anim.line_idx < visible_end;
            if still_fresh && still_visible {
                state.last_visible_start = visible_start;
                state.last_visible_end = visible_end;
                return;
            }
            if !still_fresh || !still_visible {
                state.active = None;
            }
        }

        if viewport_changed && state.active.is_none() {
            let newly_visible = user_prompt_lines.iter().copied().find(|line| {
                *line >= visible_start
                    && *line < visible_end
                    && (*line < prev_visible_start || *line >= prev_visible_end)
            });
            if let Some(line_idx) = newly_visible {
                state.active = Some(PromptViewportAnimation {
                    line_idx,
                    start_ms: now_ms,
                });
            }
        }

        state.last_visible_start = visible_start;
        state.last_visible_end = visible_end;
    }
}
