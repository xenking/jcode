//! `jcode titles` — maintain generated session titles.
//!
//! `jcode titles backfill` regenerates titles that are old first-prompt
//! truncations or internal text. It is dry-run by default; `--apply` writes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use jcode_session_types::needs_title_backfill;

use super::super::args::TitlesCommand;
use crate::session::{Session, SessionStatus};
use crate::session_title;
use crate::storage;

/// One session's backfill outcome.
struct BackfillRow {
    session: String,
    user_turns: usize,
    old_title: Option<String>,
    new_title: Option<String>,
    outcome: &'static str,
    detail: Option<String>,
}

pub(crate) async fn run_titles_command(action: TitlesCommand) -> Result<()> {
    match action {
        TitlesCommand::Backfill {
            apply,
            all,
            limit,
            session,
            sessions_dir,
            include_active,
            json,
        } => {
            backfill(
                apply,
                all,
                limit,
                session.as_deref(),
                sessions_dir,
                include_active,
                json,
            )
            .await
        }
    }
}

async fn backfill(
    apply: bool,
    all: bool,
    limit: Option<usize>,
    only_session: Option<&str>,
    sessions_dir: Option<PathBuf>,
    include_active: bool,
    json: bool,
) -> Result<()> {
    let dir = match sessions_dir {
        Some(dir) => dir,
        None => storage::jcode_dir()?.join("sessions"),
    };
    let mut paths = session_files(&dir)?;
    paths.sort_by_cached_key(|path| {
        std::cmp::Reverse(
            match std::fs::metadata(path).and_then(|meta| meta.modified()) {
                Ok(modified) => Some(modified),
                Err(error) => {
                    crate::logging::warn(&format!(
                        "Cannot read modification time for session {}: {error}",
                        path.display()
                    ));
                    None
                }
            },
        )
    });
    if let Some(only_session) = only_session {
        paths.retain(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem == only_session)
        });
    }

    let provider = if apply || !paths.is_empty() {
        Some(
            super::super::provider_init::init_provider_quiet(
                &super::super::provider_init::ProviderChoice::Auto,
                None,
            )
            .await
            .context("initialising a provider for title generation")?,
        )
    } else {
        None
    };

    let mut processed = 0usize;
    let mut changed = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for path in paths {
        if limit.is_some_and(|limit| processed >= limit) {
            break;
        }
        let id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_string();
        let mut session = match Session::load_from_path(&path) {
            Ok(session) => session,
            Err(error) => {
                failed += 1;
                report(
                    json,
                    &BackfillRow {
                        session: id,
                        user_turns: 0,
                        old_title: None,
                        new_title: None,
                        outcome: "error",
                        detail: Some(format!("load failed: {error}")),
                    },
                );
                continue;
            }
        };

        let manual_title = session
            .custom_title
            .as_deref()
            .is_some_and(|title| !title.trim().is_empty())
            || (session.saved
                && session
                    .save_label
                    .as_deref()
                    .is_some_and(|label| !label.trim().is_empty()));
        if manual_title {
            skipped += 1;
            continue;
        }
        if matches!(session.status, SessionStatus::Active) && !include_active {
            skipped += 1;
            continue;
        }
        let Some(source) = session_title::collect_source(&session) else {
            skipped += 1;
            continue;
        };
        if !all && !needs_title_backfill(session.title.as_deref()) {
            skipped += 1;
            continue;
        }

        processed += 1;
        let old_title = session.title.clone();
        let Some(provider) = provider.as_ref() else {
            continue;
        };
        match session_title::generate_title(provider, &session, &source).await {
            Ok(new_title) => {
                if apply {
                    if !session_title::set_title_if_allowed(&mut session, &new_title) {
                        skipped += 1;
                        continue;
                    }
                    // The canonical directory also updates the shared
                    // recent-session index; a copied directory is written back
                    // through the path it was loaded from.
                    let saved = if is_standard_sessions_dir(&path) {
                        session.save()
                    } else {
                        session.save_snapshot_to(&path)
                    };
                    if let Err(error) = saved {
                        failed += 1;
                        report(
                            json,
                            &BackfillRow {
                                session: id,
                                user_turns: source.real_user_turns,
                                old_title,
                                new_title: Some(new_title),
                                outcome: "error",
                                detail: Some(format!("save failed: {error}")),
                            },
                        );
                        continue;
                    }
                    changed += 1;
                    report(
                        json,
                        &BackfillRow {
                            session: id,
                            user_turns: source.real_user_turns,
                            old_title,
                            new_title: Some(new_title),
                            outcome: "applied",
                            detail: None,
                        },
                    );
                } else {
                    changed += 1;
                    report(
                        json,
                        &BackfillRow {
                            session: id,
                            user_turns: source.real_user_turns,
                            old_title,
                            new_title: Some(new_title),
                            outcome: "would_change",
                            detail: None,
                        },
                    );
                }
            }
            Err(error) => {
                failed += 1;
                report(
                    json,
                    &BackfillRow {
                        session: id,
                        user_turns: source.real_user_turns,
                        old_title,
                        new_title: None,
                        outcome: "error",
                        detail: Some(format!("{error}")),
                    },
                );
            }
        }
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "summary": true,
                "apply": apply,
                "sessions_dir": dir,
                "processed": processed,
                "changed": changed,
                "skipped": skipped,
                "failed": failed,
            })
        );
    } else {
        println!(
            "{} {} session(s) in {}; changed {}, skipped {}, failed {}",
            if apply { "Applied" } else { "Dry run:" },
            processed,
            dir.display(),
            changed,
            skipped,
            failed
        );
        if !apply {
            println!("Nothing was written. Re-run with --apply to save these titles.");
        }
    }
    Ok(())
}

fn report(json: bool, row: &BackfillRow) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "session": row.session,
                "user_turns": row.user_turns,
                "old_title": row.old_title,
                "new_title": row.new_title,
                "outcome": row.outcome,
                "detail": row.detail,
            })
        );
        return;
    }
    println!(
        "{}\n  user turns: {}\n  before: {}\n  after:  {}{}",
        row.session,
        row.user_turns,
        row.old_title.as_deref().unwrap_or("(none)"),
        row.new_title.as_deref().unwrap_or("(unchanged)"),
        row.detail
            .as_deref()
            .map(|detail| format!("\n  note: {detail}"))
            .unwrap_or_default()
    );
}

fn session_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading sessions directory {}", dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    Ok(paths)
}

/// Whether the file lives under the active `JCODE_HOME` sessions directory.
///
/// Standard sessions are persisted through `Session::save`, which also updates
/// the shared recent-session index. Copies loaded through `--sessions-dir` are
/// written back explicitly.
fn is_standard_sessions_dir(path: &Path) -> bool {
    let Ok(standard) = storage::jcode_dir().map(|dir| dir.join("sessions")) else {
        return false;
    };
    path.parent() == Some(standard.as_path())
}
