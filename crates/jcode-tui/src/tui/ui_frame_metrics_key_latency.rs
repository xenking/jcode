use std::sync::Mutex;

/// Milliseconds from reading a key event off the terminal to the frame carrying
/// it being flushed, for the most recent keystrokes.
///
/// This is the number the user actually feels, and nothing measured it: existing
/// stats cover render and flush cost *within* a draw, so a keystroke that waited
/// a long time before any draw started looked perfectly fast. Kept as a small
/// ring so a single unlucky frame cannot be mistaken for a trend.
static KEY_TO_PAINT_MS: Mutex<Vec<f64>> = Mutex::new(Vec::new());

/// When the pending keystroke was read, if a frame carrying it has not yet been
/// flushed. Only the oldest unpainted keystroke is tracked: that is the one whose
/// latency the user perceives, and it bounds the rest.
static PENDING_KEY_AT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Record that a key event was just read from the terminal.
pub(crate) fn note_key_event_read() {
    let mut slot = PENDING_KEY_AT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_none() {
        *slot = Some(std::time::Instant::now());
    }
}

/// Record that a frame has been flushed, closing out any pending keystroke.
pub(crate) fn note_frame_painted() {
    let pending = {
        let mut slot = PENDING_KEY_AT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slot.take()
    };
    if let Some(at) = pending {
        let mut samples = KEY_TO_PAINT_MS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        samples.push(at.elapsed().as_secs_f64() * 1000.0);
        const MAX_SAMPLES: usize = 64;
        if samples.len() > MAX_SAMPLES {
            let excess = samples.len() - MAX_SAMPLES;
            samples.drain(0..excess);
        }
    }
}

/// Key-to-paint latency summary for `draw-stats`.
pub(crate) fn key_to_paint_debug_json() -> serde_json::Value {
    let samples = KEY_TO_PAINT_MS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if samples.is_empty() {
        return serde_json::Value::Null;
    }
    let mut sorted = samples.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pick = |q: f64| sorted[((sorted.len() as f64 * q) as usize).min(sorted.len() - 1)];
    serde_json::json!({
        "samples": sorted.len(),
        "p50_ms": pick(0.5),
        "p95_ms": pick(0.95),
        "max_ms": sorted[sorted.len() - 1],
    })
}
