use super::{BTreeMap, BTreeSet, BridgeState, Connection, RecentSessionIndexEntry, params};

pub(super) fn context_for_session(
    id: &str,
    indexed_metadata: &BTreeMap<String, RecentSessionIndexEntry>,
    legacy_activity: &BTreeMap<String, jcode_harness_api::SessionCatalogActivity>,
) -> Option<jcode_harness_api::SessionCatalogContext> {
    indexed_metadata
        .get(id)
        .and_then(|entry| entry.context.clone())
        .or_else(|| {
            legacy_activity
                .get(id)
                .map(|activity| jcode_harness_api::SessionCatalogContext {
                    activity: Some(*activity),
                    ..Default::default()
                })
        })
}

impl BridgeState {
    // Titles are deliberately not cached. A rename is persisted
    // before `SessionRenamed` is broadcast, and every list call
    // should reflect that newest canonical value even on another
    // API connection.
    pub(super) fn indexed_session_metadata() -> BTreeMap<String, RecentSessionIndexEntry> {
        Self::recent_session_index_entries()
            .into_iter()
            .map(|entry| (entry.session_id.clone(), entry))
            .collect()
    }

    pub(super) fn backfill_session_activity(
        ids: &BTreeSet<String>,
        indexed: &mut BTreeMap<String, RecentSessionIndexEntry>,
    ) -> BTreeMap<String, jcode_harness_api::SessionCatalogActivity> {
        let mut pending: Vec<_> = ids
            .iter()
            .filter(|id| {
                indexed
                    .get(*id)
                    .and_then(|entry| entry.context.as_ref())
                    .is_none_or(|context| context.activity.is_none())
            })
            .collect();
        pending
            .sort_by_key(|id| std::cmp::Reverse(indexed.get(*id).map(|entry| entry.updated_at_ms)));
        let mut unindexed = BTreeMap::new();
        let started = std::time::Instant::now();
        let mut remaining = 16 * 1024 * 1024;
        let mut connection = None;
        for id in pending {
            if started.elapsed() >= std::time::Duration::from_millis(50) {
                break;
            }
            let Some(path) = Self::session_record_path(id) else {
                continue;
            };
            let Some(bytes) = Self::transcript_bytes(id).filter(|bytes| *bytes <= remaining) else {
                continue;
            };
            remaining -= bytes;
            let activity = match jcode_session_types::read_legacy_catalog_activity(&path) {
                Ok(activity) => activity,
                Err(error) => {
                    eprintln!("harness API bridge: read legacy activity for {id}: {error}");
                    continue;
                }
            };
            let Some(entry) = indexed.get_mut(id) else {
                unindexed.insert(id.clone(), activity);
                continue;
            };
            entry.context.get_or_insert_with(Default::default).activity = Some(activity);
            // Patch just the missing field. A concurrent session save that
            // published fresh activity wins, even if its timestamp didn't change.
            if connection.is_none()
                && let Some(path) = Self::recent_session_index_path()
            {
                match Connection::open(&path) {
                    Ok(opened) => connection = Some(opened),
                    Err(error) => eprintln!(
                        "harness API bridge: open activity index {}: {error}",
                        path.display()
                    ),
                }
            }
            if let Some(connection) = &connection {
                let value = match serde_json::to_string(&activity) {
                    Ok(value) => value,
                    Err(error) => {
                        eprintln!("harness API bridge: encode legacy activity for {id}: {error}");
                        continue;
                    }
                };
                if let Err(error) = connection.execute(
                    "UPDATE recent_sessions
                     SET catalog_context = json_set(COALESCE(catalog_context, '{}'), '$.activity', json(?2))
                     WHERE session_id = ?1 AND json_extract(catalog_context, '$.activity') IS NULL",
                    params![id, value],
                ) {
                    eprintln!("harness API bridge: cache legacy activity for {id}: {error}");
                }
            }
        }
        unindexed
    }
}
