use super::*;

/// Best-effort LLM extraction, never a replacement for the verbatim index.
pub(super) async fn extract_snapcompact_decisions(provider: &dyn Provider, index: &str) -> String {
    if index.is_empty() {
        return String::new();
    }
    let budget = provider
        .context_window()
        .saturating_sub(4_000)
        .saturating_mul(CHARS_PER_TOKEN)
        .min(120_000);
    let input: String = index.chars().take(budget).collect();
    let prompt = format!(
        "Extract decisions with their reasons, user rules and preferences from the following indexed excerpts. Preserve negations, uncertainty, and who said what. Do not infer missing reasons. Each line must begin with the exact [message N, page P | role] tag from its supporting excerpt. Do not invent facts. Output only supported lines.\n\n{input}"
    );
    let started = Instant::now();
    crate::logging::info(&format!(
        "[TIMING] snapcompact_decisions_start: model={}, input_chars={}",
        provider.model(),
        input.chars().count()
    ));
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        provider.complete_simple_with_usage(
            &prompt,
            "Extract grounded decisions and rules, without guessing.",
        ),
    )
    .await;
    crate::logging::info(&format!(
        "[TIMING] snapcompact_decisions: model={}, elapsed_ms={}, success={}",
        provider.model(),
        started.elapsed().as_millis(),
        matches!(&result, Ok(Ok(_)))
    ));
    let Ok(Ok((response, usage))) = result else {
        return String::new();
    };
    crate::telemetry::record_simple_completion_usage(
        None,
        provider.name(),
        &provider.model(),
        crate::telemetry::UsageSource::Compaction,
        usage,
    );
    let mut output = String::new();
    for line in response.lines() {
        let line = line.trim();
        let Some(end) = line.find(']') else {
            continue;
        };
        let tag = &line[..=end];
        if tag.starts_with("[message ")
            && index.lines().any(|source| source.starts_with(tag))
            && output.len() + line.len() < 24_000
        {
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

/// Generate a compaction artifact for a standalone transcript.
pub(super) async fn generate_compaction_artifact(
    provider: Arc<dyn Provider>,
    messages: Vec<Message>,
    existing_summary: Option<Summary>,
    snapcompact_enabled: bool,
    snapcompact_shape: Option<String>,
    shake_config: Option<ShakeConfig>,
) -> Result<CompactionResult> {
    generate_compaction_artifact_indexed(
        provider,
        messages.clone(),
        messages,
        existing_summary,
        snapcompact_enabled,
        snapcompact_shape,
        shake_config,
    )
    .await
}

/// Reject results which don't materially reduce the actual estimated request.
/// Snapshot-based comparison includes the unchanged suffix on both sides.
#[allow(clippy::too_many_arguments)]
pub(super) async fn generate_effective_compaction_artifact(
    provider: Arc<dyn Provider>,
    messages: Vec<Message>,
    archive_history: Vec<Message>,
    existing_summary: Option<Summary>,
    snapcompact_enabled: bool,
    snapcompact_shape: Option<String>,
    shake_config: Option<ShakeConfig>,
    remaining_chars: usize,
    token_budget: usize,
) -> Result<CompactionResult> {
    let before_chars = messages
        .iter()
        .map(message_char_count)
        .sum::<usize>()
        .saturating_add(remaining_chars);
    let before = estimate_compaction_tokens(existing_summary.as_ref(), before_chars, token_budget);
    let post = |result: &CompactionResult| {
        let summary = Summary {
            text: result.summary_text.clone(),
            openai_encrypted_content: result.openai_encrypted_content.clone().map(Into::into),
            snapcompact: result.snapcompact.clone(),
            covers_up_to_turn: result.covers_up_to_turn,
            original_turn_count: result.summarized_messages,
        };
        estimate_compaction_tokens(Some(&summary), remaining_chars, token_budget)
    };
    let insufficient = |after: usize| before.saturating_sub(after) < before / 10;
    let target = token_budget / 5 * 2;
    let mut result = generate_compaction_artifact_indexed(
        provider.clone(),
        messages.clone(),
        archive_history.clone(),
        existing_summary.clone(),
        snapcompact_enabled,
        snapcompact_shape,
        shake_config,
    )
    .await?;
    let mut after = post(&result);
    if result.snapcompact.is_some() && (insufficient(after) || (before > target && after > target))
    {
        crate::logging::warn(&format!(
            "[compaction] Snapcompact insufficient: estimated {before} -> {after} tokens; escalating to LLM summary"
        ));
        result = generate_compaction_artifact_indexed(
            provider,
            messages,
            archive_history,
            existing_summary,
            false,
            None,
            shake_config,
        )
        .await?;
        after = post(&result);
    }
    if insufficient(after) {
        anyhow::bail!(
            "Compaction failed to free at least 10% of estimated request tokens ({before} -> {after})"
        );
    }
    if before > target && after > target {
        anyhow::bail!(
            "Compaction failed to reach 40% of the estimated context budget ({before} -> {after}, budget {token_budget}); the protected tail may be too large"
        );
    }
    Ok(result)
}

pub(super) async fn generate_compaction_artifact_indexed(
    provider: Arc<dyn Provider>,
    mut messages: Vec<Message>,
    archive_history: Vec<Message>,
    mut existing_summary: Option<Summary>,
    snapcompact_enabled: bool,
    snapcompact_shape: Option<String>,
    shake_config: Option<ShakeConfig>,
) -> Result<CompactionResult> {
    let start = Instant::now();
    if let Some(summary) = existing_summary.as_mut()
        && let Some(encrypted_content) = summary.openai_encrypted_content.as_ref()
        && !openai_encrypted_content_is_sendable(encrypted_content)
    {
        let encrypted_content_len = encrypted_content.len();
        crate::logging::warn(&format!(
            "[compaction] Existing OpenAI native compaction payload is oversized ({} chars); falling back to text summary",
            encrypted_content_len,
        ));
        summary.openai_encrypted_content = None;
        let fallback = openai_encrypted_content_fallback_summary(encrypted_content_len);
        if summary.text.trim().is_empty() {
            summary.text = fallback;
        } else if !summary
            .text
            .contains("OpenAI native compaction state was discarded")
        {
            summary.text.push_str("\n\n");
            summary.text.push_str(&fallback);
        }
    }

    if existing_summary
        .as_ref()
        .is_none_or(|s| s.snapcompact.is_none())
        && let Ok(native) = provider
            .native_compact(
                &messages,
                existing_summary
                    .as_ref()
                    .map(|summary| summary.text.as_str()),
                existing_summary
                    .as_ref()
                    .and_then(|summary| summary.openai_encrypted_content.as_deref()),
            )
            .await
    {
        if let Some(usage) = native.usage {
            crate::telemetry::record_simple_completion_usage(
                None,
                provider.name(),
                &provider.model(),
                crate::telemetry::UsageSource::Compaction,
                usage,
            );
        }
        if let Some(encrypted_content) = native.openai_encrypted_content.as_ref()
            && !openai_encrypted_content_is_sendable(encrypted_content)
        {
            crate::logging::warn(&format!(
                "[compaction] OpenAI native compaction returned oversized encrypted_content ({} chars); falling back to text summary",
                encrypted_content.len(),
            ));
        } else if native.openai_encrypted_content.is_some() || native.summary_text.is_some() {
            return Ok(CompactionResult {
                completed_at: Instant::now(),
                decisions_job: None,
                summary_text: native.summary_text.unwrap_or_default(),
                openai_encrypted_content: native.openai_encrypted_content,
                snapcompact: None,
                covers_up_to_turn: messages.len(),
                duration_ms: start.elapsed().as_millis() as u64,
                summarized_messages: messages.len(),
            });
        }
    }

    let mut snapcompact = None;
    let mut snapcompact_summary = None;
    let mut decisions_job = None;
    if snapcompact_enabled
        && existing_summary
            .as_ref()
            .is_none_or(|s| s.openai_encrypted_content.is_none())
    {
        let input = messages.clone();
        let message_offset = archive_history.len().saturating_sub(input.len());
        let prior_archive = existing_summary
            .as_ref()
            .and_then(|s| s.snapcompact.as_ref());
        let previous_decisions = prior_archive
            .map(|a| a.decisions.clone())
            .unwrap_or_default();
        let cache = prior_archive
            .filter(|a| !a.render_key.is_empty() && a.pages.len() == a.page_ranges.len())
            .map(|a| (a.render_key.clone(), a.page_ranges.clone(), a.pages.clone()));
        // A previous archive source is re-folded as history; a previous text
        // summary is labelled (`[Summary of earlier history]`) the way omp
        // folds a pre-snapcompact summary into the archive head.
        let previous = existing_summary.as_ref().map(|summary| {
            summary.snapcompact.as_ref().map_or_else(
                || {
                    (
                        jcode_compaction_core::snapcompact::normalize(&summary.text),
                        true,
                    )
                },
                |archive| (archive.source.clone(), false),
            )
        });
        let model_id = provider.model();
        let provider_name = provider.name().to_string();
        let options = jcode_compaction_core::snapcompact::CompactOptions {
            model_id,
            provider: provider_name,
            shape: snapcompact_shape
                .as_ref()
                .and_then(|name| jcode_compaction_core::snapcompact::shape_by_name(name)),
            ..Default::default()
        };
        let rendered = tokio::task::spawn_blocking(move || {
            use jcode_compaction_core::snapcompact::{compact_reusing, prepare_messages_with, FrameCache, FrameRef};
            let previous = previous.as_ref().map(|(text, is_summary)| {
                jcode_compaction_core::snapcompact::PreviousArchive {
                    text,
                    is_summary: *is_summary,
                }
            });
            let phase = Instant::now();
            let preparation = prepare_messages_with(&input, previous, usize::MAX)?;
            crate::logging::info(&format!("[TIMING] snapcompact_prepare: elapsed_ms={}, source_chars={}", phase.elapsed().as_millis(), preparation.bounded_text.chars().count()));
            let phase = Instant::now();
            let cached_frames = cache.as_ref().and_then(|(_, ranges, pages)| {
                pages.iter().zip(ranges).enumerate().map(|(index, (page, &(source_start, source_end)))| {
                    base64::engine::general_purpose::STANDARD.decode(page).map(|bytes| FrameRef { index: index as u32, source_start, source_end, bytes })
                }).collect::<Result<Vec<_>, _>>().ok()
            });
            let frame_cache = cache.as_ref().zip(cached_frames.as_ref()).and_then(|((key, _, _), frames)| {
                previous.map(|p| FrameCache { source: p.text, render_key: key.clone(), frames })
            });
            crate::logging::info(&format!("[TIMING] snapcompact_cache_decode: elapsed_ms={}, cached_pages={}", phase.elapsed().as_millis(), cached_frames.as_ref().map_or(0, Vec::len)));
            let phase = Instant::now();
            let result = compact_reusing(&preparation, &options, frame_cache)?;
            crate::logging::info(&format!("[TIMING] snapcompact_render: elapsed_ms={}, pages={}, new_pages={}, font_ms={}, raster_ms={}, png_encode_ms={}", phase.elapsed().as_millis(), result.frames.len(), result.rendered_frames, result.render_timings.font.as_millis(), result.render_timings.raster.as_millis(), result.render_timings.encode.as_millis()));
            let ranges =
                result
                    .frames
                    .iter()
                    .map(|f| jcode_compaction_core::snapcompact::FrameRange {
                        index: f.index,
                        source_start: f.source_start,
                        source_end: f.source_end,
                    })
                    .collect::<Vec<_>>();
            let phase = Instant::now();
            let index = build_snapcompact_index(&archive_history, 0, &result.source_text, &ranges);
            crate::logging::info(&format!("[TIMING] snapcompact_index: elapsed_ms={}, index_chars={}, history_messages={}", phase.elapsed().as_millis(), index.chars().count(), archive_history.len()));
            Ok::<_, String>((result, ranges, index))
        })
        .await;
        match rendered {
            Ok(Ok((result, ranges, index))) => {
                let page_ranges = ranges
                    .iter()
                    .map(|r| (r.source_start, r.source_end))
                    .collect();
                let decisions = snap_decisions::retain_supported(&previous_decisions, &index);
                let new_index = snap_decisions::index_since(&index, message_offset);
                if !new_index.is_empty() {
                    decisions_job = Some(snap_decisions::DecisionsJob {
                        provider: provider.clone(),
                        index: new_index,
                    });
                }
                let render_key = format!("v1:{:?}", result.shape);
                let phase = Instant::now();
                let pages = result
                    .frames
                    .into_iter()
                    .map(|frame| base64::engine::general_purpose::STANDARD.encode(frame.bytes))
                    .collect();
                crate::logging::info(&format!(
                    "[TIMING] snapcompact_base64: elapsed_ms={}",
                    phase.elapsed().as_millis()
                ));
                snapcompact_summary = Some(result.summary);
                snapcompact = Some(jcode_message_types::SnapcompactArchive {
                    source: result.source_text,
                    pages,
                    index,
                    decisions,
                    page_ranges,
                    render_key,
                });
            }
            Ok(Err(error)) => {
                crate::logging::warn(&format!(
                    "[compaction] Snapcompact failed; falling back to LLM: {error}"
                ));
                // omp advances to its `shake` method after snapcompact fails, so
                // the text summarizer at least sees a mechanically reduced
                // prompt instead of the raw full history. The configured shake
                // settings decide: `[compaction] shake = false` (or
                // `JCODE_COMPACTION_SHAKE=0`) means the user disabled shake, and
                // the thresholds are the configured ones, not the defaults. The
                // warm-cache guard does not apply — snapcompact already gave up
                // on this prefix.
                if let Some(shake) = shake_config {
                    let outcome = jcode_compaction_core::shake::shake(&mut messages, &shake);
                    if outcome != jcode_compaction_core::shake::ShakeOutcome::NoChange {
                        crate::logging::info(&format!(
                            "[compaction] post-snapcompact shake saved ~{} tokens",
                            outcome.tokens_saved()
                        ));
                    }
                }
            }
            Err(error) => crate::logging::warn(&format!(
                "[compaction] Snapcompact worker failed; falling back to LLM: {error}"
            )),
        }
    }

    // The image archive replaces the full-context summary for vision models.
    // Decision extraction is best effort, and text-only models also get a summary.
    if let Some(mut summary) = snapcompact_summary
        && snapcompact.is_some()
    {
        if !snapcompact_pages_usable(provider.supports_image_input(), &provider.model()) {
            let mut prior = existing_summary.clone();
            if let Some(previous) = prior.as_mut()
                && let Some(archive) = &previous.snapcompact
            {
                previous.text = archive
                    .source
                    .replace('\u{2588}', "\n")
                    .replace(['\u{e}', '\u{f}'], "");
            }
            let max_chars = provider.context_window().saturating_sub(4000) * CHARS_PER_TOKEN;
            let prompt = build_compaction_prompt(&messages, prior.as_ref(), max_chars);
            if let Ok(Ok((text, usage))) = tokio::time::timeout(
                Duration::from_secs(60),
                provider.complete_simple_with_usage(
                    &prompt,
                    "You are a helpful assistant that summarizes conversations.",
                ),
            )
            .await
            {
                crate::telemetry::record_simple_completion_usage(
                    None,
                    provider.name(),
                    &provider.model(),
                    crate::telemetry::UsageSource::Compaction,
                    usage,
                );
                if let Ok(text) = validate_compaction_summary(text) {
                    summary = text;
                }
            }
        }
        return Ok(CompactionResult {
            completed_at: Instant::now(),
            decisions_job,
            summary_text: summary,
            openai_encrypted_content: None,
            snapcompact,
            covers_up_to_turn: messages.len(),
            duration_ms: start.elapsed().as_millis() as u64,
            summarized_messages: messages.len(),
        });
    }

    // Include the archive source when falling back to text-only compaction.
    if snapcompact.is_none()
        && let Some(summary) = existing_summary.as_mut()
        && let Some(archive) = summary.snapcompact.take()
    {
        summary.text = archive
            .source
            .replace('\u{2588}', "\n")
            .replace(['\u{e}', '\u{f}'], "");
    }

    let max_prompt_chars = provider.context_window().saturating_sub(4000) * CHARS_PER_TOKEN;
    let prompt = build_compaction_prompt(&messages, existing_summary.as_ref(), max_prompt_chars);

    // Generate summary using simple completion
    let (summary, usage) = provider
        .complete_simple_with_usage(
            &prompt,
            "You are a helpful assistant that summarizes conversations.",
        )
        .await
        .inspect_err(|error| {
            crate::telemetry::record_simple_completion_failure_usage(
                None,
                crate::telemetry::UsageSource::Compaction,
                error,
            );
        })?;
    crate::telemetry::record_simple_completion_usage(
        None,
        provider.name(),
        &provider.model(),
        crate::telemetry::UsageSource::Compaction,
        usage,
    );

    Ok(CompactionResult {
        completed_at: Instant::now(),
        decisions_job: None,
        summary_text: validate_compaction_summary(summary)?,
        openai_encrypted_content: None,
        snapcompact,
        covers_up_to_turn: messages.len(),
        duration_ms: start.elapsed().as_millis() as u64,
        summarized_messages: messages.len(),
    })
}

/// Reject summaries that cannot stand in for the messages they replace, such
/// as empty output or a provider refusal (issue #2089). Returning an error makes
/// compaction keep the original messages instead of dropping them.
pub(super) fn validate_compaction_summary(summary: String) -> Result<String> {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        anyhow::bail!("compaction summary rejected: provider returned empty text");
    }
    if looks_like_refusal(trimmed) {
        let preview: String = trimmed.chars().take(120).collect();
        anyhow::bail!(
            "compaction summary rejected: provider refusal: {:?}",
            preview
        );
    }
    Ok(summary)
}

fn looks_like_refusal(text: &str) -> bool {
    // Only inspect the opening so a real summary that mentions an earlier
    // refusal later on is still accepted.
    let head: String = text.chars().take(160).collect::<String>().to_lowercase();
    let head = head.replace('\u{2019}', "'");
    const PATTERNS: [&str; 8] = [
        "i'm sorry, but i can",
        "i am sorry, but i can",
        "sorry, but i can't",
        "sorry, i can't",
        "i cannot assist with",
        "i can't assist with",
        "i can't help with",
        "i'm unable to help with",
    ];
    // Short replies are refusals if they contain a pattern anywhere. Longer
    // ones only if they open with it.
    if text.chars().count() < 300 {
        PATTERNS.iter().any(|pattern| head.contains(pattern))
    } else {
        PATTERNS.iter().any(|pattern| head.starts_with(pattern))
    }
}
