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
                    .and_then(|entry| entry.context.as_ref()?.activity?.last_message_at_ms)
                    .is_none()
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
                    diag!("harness API bridge: read legacy activity for {id}: {error}");
                    continue;
                }
            };
            let Some(entry) = indexed.get_mut(id) else {
                unindexed.insert(id.clone(), activity);
                continue;
            };
            let context = entry.context.get_or_insert_with(Default::default);
            context.activity.get_or_insert(activity).last_message_at_ms =
                activity.last_message_at_ms;
            // Only fill absent activity. A concurrent save that already wrote
            // the new field, including an explicit null, wins.
            if connection.is_none()
                && let Some(path) = Self::recent_session_index_path()
            {
                match Connection::open(&path) {
                    Ok(opened) => connection = Some(opened),
                    Err(error) => diag!(
                        "harness API bridge: open activity index {}: {error}",
                        path.display()
                    ),
                }
            }
            if let Some(connection) = &connection {
                let value = match serde_json::to_string(&activity) {
                    Ok(value) => value,
                    Err(error) => {
                        diag!("harness API bridge: encode legacy activity for {id}: {error}");
                        continue;
                    }
                };
                if let Err(error) = connection.execute(
                    "UPDATE recent_sessions SET catalog_context = CASE
                         WHEN NOT json_valid(catalog_context) THEN json_object('activity', json(?2))
                         WHEN json_type(catalog_context, '$.activity') IS NOT 'object'
                         THEN json_set(catalog_context, '$.activity', json(?2))
                         ELSE json_set(catalog_context, '$.activity.last_message_at_ms',
                                       json_extract(?2, '$.last_message_at_ms')) END
                     WHERE session_id = ?1 AND (CASE WHEN json_valid(catalog_context)
                         THEN json_type(catalog_context, '$.activity.last_message_at_ms') IS NULL
                         ELSE 1 END)",
                    params![id, value],
                ) {
                    diag!("harness API bridge: cache legacy activity for {id}: {error}");
                }
            }
        }
        unindexed
    }
}
