//! Isolation flag and geometry for secondary renders (for example the resume
//! preview), which must not publish layout, scroll, copy, or status state that
//! the live session's input handling reads back.

thread_local! {
    static ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static GEOMETRY: std::cell::Cell<SecondaryGeometry> =
        const { std::cell::Cell::new(SecondaryGeometry { max_scroll: 0, scroll: 0, total_lines: 0, viewport_height: 0 }) };
}

/// Transcript geometry a secondary render resolved. The live session's
/// globals stay untouched, so embedders read their own results from here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SecondaryGeometry {
    pub max_scroll: usize,
    pub scroll: usize,
    pub total_lines: usize,
    pub viewport_height: usize,
}

pub(crate) fn note_geometry(update: impl FnOnce(&mut SecondaryGeometry)) {
    GEOMETRY.with(|cell| {
        let mut geometry = cell.get();
        update(&mut geometry);
        cell.set(geometry);
    });
}

/// True while drawing a non-live view.
pub(crate) fn active() -> bool {
    ACTIVE.with(std::cell::Cell::get)
}

/// Run `f` with secondary-render isolation enabled on this thread and return
/// the geometry it resolved alongside the result.
pub(crate) fn run<R>(f: impl FnOnce() -> R) -> (R, SecondaryGeometry) {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            ACTIVE.with(|c| c.set(self.0));
        }
    }
    GEOMETRY.with(|cell| cell.set(SecondaryGeometry::default()));
    let _reset = Reset(ACTIVE.with(|c| c.replace(true)));
    let result = f();
    (result, GEOMETRY.with(std::cell::Cell::get))
}
