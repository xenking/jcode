use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct BodyCacheKey {
    pub(super) width: u16,
    pub(super) diff_mode: crate::config::DiffDisplayMode,
    pub(super) messages_version: u64,
    pub(super) diagram_mode: crate::config::DiagramDisplayMode,
    pub(super) centered: bool,
    pub(super) compact_thinking: bool,
    /// Mermaid render geometry depends on the scoped transcript/pane aspect
    /// profile as well as width. A vertical terminal resize can change this
    /// bucket without changing `width`, so it must invalidate the prepared body.
    pub(super) mermaid_aspect_bucket: Option<u16>,
    /// Whether inline images render at all (Alt+M hides them).
    pub(super) pin_images: bool,
    /// Whether inline images render expanded or as collapsed label stubs
    /// (Alt+Shift+I toggles; persisted).
    pub(super) inline_images_visible: bool,
    /// Signature of the inline image set; anchored images render inside the
    /// body, so the body must rebuild when images arrive or change.
    pub(super) images_signature: (usize, u64),
    /// Monotonic per-image expand-level version. Anchored images embed their
    /// expand-level geometry into the body, so a level change must rebuild the
    /// body exactly like an image-set change does.
    pub(super) expanded_images_version: u64,
    pub(super) latex_rendering: crate::config::LatexRenderingMode,
    pub(super) show_link_urls: bool,
}

#[derive(Clone)]
pub(super) struct BodyCacheEntry {
    pub(super) key: BodyCacheKey,
    pub(super) prepared: Arc<PreparedMessages>,
    pub(super) prepared_bytes: usize,
    pub(super) msg_count: usize,
    /// `compacted_hidden_user_prompts` at build time. Prepend (suffix) reuse
    /// renders absolute prompt numbers into the reused lines, so a rebuild that
    /// stitches older history above a cached suffix must verify numbering
    /// continuity against the offset the base was built with.
    pub(super) prompt_offset: usize,
}

pub(super) const BODY_CACHE_MAX_ENTRIES: usize = 8;
// Keep enough room for a single large transcript snapshot so long sessions do not
// fall off a hard per-entry cache cliff and get rebuilt every frame.
pub(super) const BODY_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;
pub(super) const BODY_OVERSIZED_CACHE_MAX_ENTRIES: usize = 2;

#[derive(Default)]
pub(super) struct BodyCacheState {
    pub(super) entries: VecDeque<BodyCacheEntry>,
    pub(super) oversized_entries: VecDeque<BodyCacheEntry>,
}

impl BodyCacheState {
    pub(super) fn total_bytes(&self) -> usize {
        self.entries.iter().map(|entry| entry.prepared_bytes).sum()
    }

    /// Returns the cached frame, its entry kind, and the byte estimate taken
    /// once at insert time. Re-estimating walks every wrapped line, which is
    /// milliseconds on long transcripts and used to run on every cache hit.
    pub(super) fn get_exact_with_kind(
        &mut self,
        key: &BodyCacheKey,
    ) -> Option<(Arc<PreparedMessages>, CacheEntryKind, usize)> {
        if let Some(pos) = self.entries.iter().position(|entry| &entry.key == key) {
            let entry = self.entries.remove(pos)?;
            let found = (
                entry.prepared.clone(),
                CacheEntryKind::Regular,
                entry.prepared_bytes,
            );
            self.entries.push_front(entry);
            Some(found)
        } else {
            let pos = self
                .oversized_entries
                .iter()
                .position(|entry| &entry.key == key)?;
            let entry = self.oversized_entries.remove(pos)?;
            let found = (
                entry.prepared.clone(),
                CacheEntryKind::Oversized,
                entry.prepared_bytes,
            );
            self.oversized_entries.push_front(entry);
            Some(found)
        }
    }

    #[cfg(test)]
    pub(super) fn get_exact(&mut self, key: &BodyCacheKey) -> Option<Arc<PreparedMessages>> {
        self.get_exact_with_kind(key)
            .map(|(prepared, _, _)| prepared)
    }

    #[cfg(test)]
    pub(super) fn best_incremental_base(
        &self,
        key: &BodyCacheKey,
        _msg_count: usize,
    ) -> Option<(Arc<PreparedMessages>, usize)> {
        let regular = self
            .entries
            .iter()
            .filter(|entry| {
                entry.msg_count > 0
                    && entry.key.width == key.width
                    && entry.key.diff_mode == key.diff_mode
                    && entry.key.diagram_mode == key.diagram_mode
                    && entry.key.centered == key.centered
                    && entry.key.compact_thinking == key.compact_thinking
                    && entry.key.mermaid_aspect_bucket == key.mermaid_aspect_bucket
                    // Anchored inline images render inside the body, and a
                    // late-arriving image may target an already-prepared
                    // message; only reuse bases built with the same image set.
                    && entry.key.pin_images == key.pin_images
                    && entry.key.inline_images_visible == key.inline_images_visible
                    && entry.key.images_signature == key.images_signature
                    && entry.key.expanded_images_version == key.expanded_images_version
                    && entry.key.latex_rendering == key.latex_rendering
                    && entry.key.show_link_urls == key.show_link_urls
            })
            .max_by_key(|entry| entry.msg_count)
            .map(|entry| (entry.prepared.clone(), entry.msg_count));
        let oversized = self
            .oversized_entries
            .iter()
            .filter(|entry| {
                entry.msg_count > 0
                    && entry.key.width == key.width
                    && entry.key.diff_mode == key.diff_mode
                    && entry.key.diagram_mode == key.diagram_mode
                    && entry.key.centered == key.centered
                    && entry.key.compact_thinking == key.compact_thinking
                    && entry.key.mermaid_aspect_bucket == key.mermaid_aspect_bucket
                    // Anchored inline images render inside the body, and a
                    // late-arriving image may target an already-prepared
                    // message; only reuse bases built with the same image set.
                    && entry.key.pin_images == key.pin_images
                    && entry.key.inline_images_visible == key.inline_images_visible
                    && entry.key.images_signature == key.images_signature
                    && entry.key.expanded_images_version == key.expanded_images_version
                    && entry.key.latex_rendering == key.latex_rendering
                    && entry.key.show_link_urls == key.show_link_urls
            })
            .max_by_key(|entry| entry.msg_count)
            .map(|entry| (entry.prepared.clone(), entry.msg_count));

        match (regular, oversized) {
            (Some(left), Some(right)) => {
                if left.1 >= right.1 {
                    Some(left)
                } else {
                    Some(right)
                }
            }
            (Some(entry), None) | (None, Some(entry)) => Some(entry),
            (None, None) => None,
        }
    }

    #[cfg(test)]
    pub(super) fn insert(
        &mut self,
        key: BodyCacheKey,
        prepared: Arc<PreparedMessages>,
        msg_count: usize,
        prompt_offset: usize,
    ) {
        let prepared_bytes = estimate_prepared_messages_bytes(&prepared);
        self.insert_with_bytes(key, prepared, prepared_bytes, msg_count, prompt_offset);
    }

    pub(super) fn insert_with_bytes(
        &mut self,
        key: BodyCacheKey,
        prepared: Arc<PreparedMessages>,
        prepared_bytes: usize,
        msg_count: usize,
        prompt_offset: usize,
    ) {
        if prepared_bytes > BODY_CACHE_MAX_BYTES {
            if let Some(pos) = self
                .oversized_entries
                .iter()
                .position(|entry| entry.key == key)
            {
                self.oversized_entries.remove(pos);
            }
            self.oversized_entries.push_front(BodyCacheEntry {
                key,
                prepared,
                prepared_bytes,
                msg_count,
                prompt_offset,
            });
            while self.oversized_entries.len() > BODY_OVERSIZED_CACHE_MAX_ENTRIES {
                self.oversized_entries.pop_back();
            }
            return;
        }
        if let Some(pos) = self
            .oversized_entries
            .iter()
            .position(|entry| entry.key == key)
        {
            self.oversized_entries.remove(pos);
        }
        if let Some(pos) = self.entries.iter().position(|entry| entry.key == key) {
            self.entries.remove(pos);
        }
        // An entry that differs only by an older `messages_version` can never
        // be an exact hit again, and the new entry is a better incremental
        // base for the same layout. Keeping it only holds memory.
        self.entries.retain(|entry| {
            !(entry.key.messages_version < key.messages_version
                && BodyCacheKey {
                    messages_version: key.messages_version,
                    ..entry.key.clone()
                } == key)
        });
        self.entries.push_front(BodyCacheEntry {
            key,
            prepared,
            prepared_bytes,
            msg_count,
            prompt_offset,
        });
        while self.entries.len() > BODY_CACHE_MAX_ENTRIES
            || self.total_bytes() > BODY_CACHE_MAX_BYTES
        {
            self.entries.pop_back();
        }
    }
}

pub(super) static BODY_CACHE: OnceLock<Mutex<BodyCacheState>> = OnceLock::new();

pub(super) fn body_cache() -> &'static Mutex<BodyCacheState> {
    BODY_CACHE.get_or_init(|| Mutex::new(BodyCacheState::default()))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct FullPrepCacheKey {
    pub(super) width: u16,
    pub(super) height: u16,
    pub(super) diff_mode: crate::config::DiffDisplayMode,
    pub(super) messages_version: u64,
    pub(super) diagram_mode: crate::config::DiagramDisplayMode,
    pub(super) centered: bool,
    pub(super) compact_thinking: bool,
    /// The scoped Mermaid profile can also change when pane geometry changes
    /// while the transcript rectangle stays the same.
    pub(super) mermaid_aspect_bucket: Option<u16>,
    pub(super) is_processing: bool,
    pub(super) streaming_text_len: usize,
    pub(super) streaming_text_hash: u64,
    pub(super) batch_progress_hash: u64,
    pub(super) inline_images_signature: (usize, u64),
    /// Whether inline images render expanded or as collapsed label stubs.
    pub(super) inline_images_visible: bool,
    /// Per-image expand-level version; anchored image geometry is embedded in
    /// the prepared frame, so a level change must invalidate it.
    pub(super) expanded_images_version: u64,
    pub(super) latex_rendering: crate::config::LatexRenderingMode,
    pub(super) show_link_urls: bool,
}

#[derive(Clone)]
pub(super) struct FullPrepCacheEntry {
    pub(super) key: FullPrepCacheKey,
    pub(super) prepared: Arc<PreparedChatFrame>,
    pub(super) prepared_bytes: usize,
}

pub(super) const FULL_PREP_CACHE_MAX_ENTRIES: usize = 4;
// Full prepared frames duplicate some body data, so give them enough headroom to
// retain the active large transcript instead of forcing full recomposition.
pub(super) const FULL_PREP_CACHE_MAX_BYTES: usize = 24 * 1024 * 1024;
pub(super) const FULL_PREP_OVERSIZED_CACHE_MAX_ENTRIES: usize = 2;

#[derive(Default)]
pub(super) struct FullPrepCacheState {
    pub(super) entries: VecDeque<FullPrepCacheEntry>,
    pub(super) oversized_entries: VecDeque<FullPrepCacheEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) enum CacheEntryKind {
    Regular,
    Oversized,
}

impl FullPrepCacheState {
    pub(super) fn total_bytes(&self) -> usize {
        self.entries.iter().map(|entry| entry.prepared_bytes).sum()
    }

    /// Returns the cached frame, its entry kind, and the byte estimate taken
    /// once at insert time. Re-estimating walks every wrapped line, which is
    /// milliseconds on long transcripts and used to run on every cache hit.
    pub(super) fn get_exact_with_kind(
        &mut self,
        key: &FullPrepCacheKey,
    ) -> Option<(Arc<PreparedChatFrame>, CacheEntryKind, usize)> {
        if let Some(pos) = self.entries.iter().position(|entry| &entry.key == key) {
            let entry = self.entries.remove(pos)?;
            let found = (
                entry.prepared.clone(),
                CacheEntryKind::Regular,
                entry.prepared_bytes,
            );
            self.entries.push_front(entry);
            Some(found)
        } else {
            let pos = self
                .oversized_entries
                .iter()
                .position(|entry| &entry.key == key)?;
            let entry = self.oversized_entries.remove(pos)?;
            let found = (
                entry.prepared.clone(),
                CacheEntryKind::Oversized,
                entry.prepared_bytes,
            );
            self.oversized_entries.push_front(entry);
            Some(found)
        }
    }

    #[cfg(test)]
    pub(super) fn get_exact(&mut self, key: &FullPrepCacheKey) -> Option<Arc<PreparedChatFrame>> {
        self.get_exact_with_kind(key)
            .map(|(prepared, _, _)| prepared)
    }

    #[cfg(test)]
    pub(super) fn insert(&mut self, key: FullPrepCacheKey, prepared: Arc<PreparedChatFrame>) {
        let prepared_bytes = estimate_prepared_chat_frame_bytes(&prepared);
        self.insert_with_bytes(key, prepared, prepared_bytes);
    }

    pub(super) fn insert_with_bytes(
        &mut self,
        key: FullPrepCacheKey,
        prepared: Arc<PreparedChatFrame>,
        prepared_bytes: usize,
    ) {
        if prepared_bytes > FULL_PREP_CACHE_MAX_BYTES {
            if let Some(pos) = self
                .oversized_entries
                .iter()
                .position(|entry| entry.key == key)
            {
                self.oversized_entries.remove(pos);
            }
            self.oversized_entries.push_front(FullPrepCacheEntry {
                key,
                prepared,
                prepared_bytes,
            });
            while self.oversized_entries.len() > FULL_PREP_OVERSIZED_CACHE_MAX_ENTRIES {
                self.oversized_entries.pop_back();
            }
            return;
        }
        if let Some(pos) = self
            .oversized_entries
            .iter()
            .position(|entry| entry.key == key)
        {
            self.oversized_entries.remove(pos);
        }
        if let Some(pos) = self.entries.iter().position(|entry| entry.key == key) {
            self.entries.remove(pos);
        }
        // A frame keyed on partial streaming text can never be hit again once
        // the stream advances: text only grows, and the turn ending bumps
        // `messages_version`. Holding them just pins multi-MB snapshots of the
        // whole transcript (measured: 4 entries, 13.6 MB on one client).
        self.entries
            .retain(|entry| entry.key.streaming_text_len == 0);
        self.entries.push_front(FullPrepCacheEntry {
            key,
            prepared,
            prepared_bytes,
        });
        while self.entries.len() > FULL_PREP_CACHE_MAX_ENTRIES
            || self.total_bytes() > FULL_PREP_CACHE_MAX_BYTES
        {
            self.entries.pop_back();
        }
    }
}

pub(super) static FULL_PREP_CACHE: OnceLock<Mutex<FullPrepCacheState>> = OnceLock::new();

pub(super) fn full_prep_cache() -> &'static Mutex<FullPrepCacheState> {
    FULL_PREP_CACHE.get_or_init(|| Mutex::new(FullPrepCacheState::default()))
}

/// Drop every frame-level reference to `base` except the caller's, so the
/// incremental body build can mutate it in place. Otherwise `Arc::make_mut`
/// deep-clones the whole wrapped transcript (tens of ms on long sessions) on
/// every appended message.
///
/// The base was just taken out of the body cache because no body entry matched
/// the current key, so any full-prep frame embedding it is keyed on an older
/// transcript and cannot be hit again. The last drawn frame is republished by
/// the draw that triggered this build.
pub(in super::super) fn release_frame_refs_to_body(base: &Arc<PreparedMessages>) {
    let holds = |frame: &Arc<PreparedChatFrame>| {
        frame
            .sections
            .iter()
            .any(|section| Arc::ptr_eq(&section.prepared, base))
    };
    if let Ok(mut cache) = full_prep_cache().lock() {
        cache.entries.retain(|entry| !holds(&entry.prepared));
        cache
            .oversized_entries
            .retain(|entry| !holds(&entry.prepared));
    }
    #[cfg(test)]
    TEST_LAST_CHAT_FRAME.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(holds) {
            *slot = None;
        }
    });
    #[cfg(not(test))]
    if let Ok(mut slot) = last_chat_frame_state().lock()
        && slot.as_ref().is_some_and(holds)
    {
        *slot = None;
    }
}

/// Drop only reconstructible presentation data. Call after retaining a bounded
/// terminal frame so idle repaints cannot immediately recreate these caches.
pub(crate) fn release_idle_render_caches() {
    jcode_tui_messages::clear_message_cache();
    jcode_tui_markdown::clear_highlight_cache();
    if let Ok(mut cache) = body_cache().lock() {
        *cache = BodyCacheState::default();
    }
    if let Ok(mut cache) = full_prep_cache().lock() {
        *cache = FullPrepCacheState::default();
    }
    if let Ok(mut cache) = file_diff_ui::file_diff_cache().lock() {
        *cache = Default::default();
    }
    #[cfg(test)]
    TEST_LAST_CHAT_FRAME.with(|slot| *slot.borrow_mut() = None);
    #[cfg(not(test))]
    if let Ok(mut frame) = last_chat_frame_state().lock() {
        *frame = None;
    }
    clear_copy_viewport_snapshot();
    clear_side_panel_render_caches();
    inline_image_ui::clear_staged_payloads();
    // Unlike clear_cache(), this retains on-disk image artifacts and registered
    // diagrams. They are needed to reconstruct exactly the same view on wake.
    crate::tui::mermaid::clear_image_state();
}

#[cfg(test)]
pub(crate) fn idle_render_caches_empty() -> bool {
    let body = body_cache().lock().unwrap();
    let full = full_prep_cache().lock().unwrap();
    body.entries.is_empty()
        && body.oversized_entries.is_empty()
        && full.entries.is_empty()
        && full.oversized_entries.is_empty()
        && last_chat_frame().is_none()
}

#[cfg(not(test))]
pub(super) static LAST_STATUS_AREA: OnceLock<Mutex<Option<Rect>>> = OnceLock::new();

#[cfg(not(test))]
pub(super) fn last_status_area_state() -> &'static Mutex<Option<Rect>> {
    LAST_STATUS_AREA.get_or_init(|| Mutex::new(None))
}

pub(crate) fn record_status_area(area: Rect) {
    if secondary_render_active() {
        return;
    }
    #[cfg(test)]
    {
        TEST_LAST_STATUS_AREA.with(|snapshot| {
            *snapshot.borrow_mut() = Some(area);
        });
        return;
    }
    #[cfg(not(test))]
    {
        if let Ok(mut snapshot) = last_status_area_state().lock() {
            *snapshot = Some(area);
        }
    }
}

pub(crate) fn last_status_area() -> Option<Rect> {
    #[cfg(test)]
    {
        return TEST_LAST_STATUS_AREA.with(|snapshot| *snapshot.borrow());
    }
    #[cfg(not(test))]
    {
        last_status_area_state()
            .lock()
            .ok()
            .and_then(|snapshot| *snapshot)
    }
}
