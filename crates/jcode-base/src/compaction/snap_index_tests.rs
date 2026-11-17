use super::*;
use std::time::{Duration, Instant};

#[path = "snap_index_legacy.rs"]
mod legacy;

fn message(role: Role, content: Vec<ContentBlock>) -> Message {
    Message {
        role,
        content: content.into(),
        timestamp: None,
        tool_duration_ms: None,
    }
}

fn text(role: Role, value: &str) -> Message {
    message(
        role,
        vec![ContentBlock::Text {
            text: value.into(),
            cache_control: None,
        }],
    )
}

fn ranges(source: &str) -> Vec<FrameRange> {
    let len = source.chars().count();
    (0..len.div_ceil(701))
        .map(|i| FrameRange {
            index: i as u32,
            source_start: i * 701,
            source_end: ((i + 1) * 701).min(len),
        })
        .collect()
}

#[test]
fn snap_index_golden_matches_legacy() {
    let mut messages = vec![
        text(
            Role::User,
            "Never change λ/cache = 42.\nRepeated value = 12345\nRepeated value = 12345\nabababa\n     \nTiny",
        ),
        text(
            Role::Assistant,
            "Must preserve\twhitespace = 77\nWorker ━━│┏ done /tmp/path\nMissing value = 69",
        ),
        message(
            Role::Assistant,
            vec![
                ContentBlock::ToolUse {
                    id: "call".into(),
                    name: "worker".into(),
                    thought_signature: None,
                    input: serde_json::json!({"path":"/tmp/λ/facts", "value": 12345}),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call".into(),
                    content: "result = 88\nDo not lose commit abcdef123".into(),
                    is_error: None,
                },
            ],
        ),
    ];
    for i in 0..900 {
        messages.push(text(
            if i % 3 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            &format!("worker {i:05}: {}\nresult = {i}", "λx".repeat(210)),
        ));
    }
    let source = normalize(
        &jcode_compaction_core::snapcompact::serialize::serialize_messages(&messages).unwrap(),
    )
    .replace("Missing value = 69", "older summary");
    let mut page_ranges = ranges(&source);
    for ranges in [&[][..], &page_ranges[..]] {
        let expected = legacy::build_snapcompact_index(&messages, 17, &source, ranges);
        assert_eq!(
            build_snapcompact_index(&messages, 17, &source, ranges),
            expected
        );
    }
    // Legacy lookup honors the first range, even with overlapping, unsorted or invalid ranges.
    page_ranges.reverse();
    page_ranges.push(FrameRange {
        index: 9999,
        source_start: 0,
        source_end: usize::MAX,
    });
    assert_eq!(
        build_snapcompact_index(&messages, 0, &source, &page_ranges),
        legacy::build_snapcompact_index(&messages, 0, &source, &page_ranges)
    );
}

#[test]
fn snap_index_large_source_is_bounded() {
    let messages: Vec<_> = (0..3000)
        .map(|i| {
            text(
                Role::User,
                &format!("worker {i:05} must preserve {} end={i:05}", "x".repeat(480)),
            )
        })
        .collect();
    let source = normalize(
        &jcode_compaction_core::snapcompact::serialize::serialize_messages(&messages).unwrap(),
    );
    assert!(source.len() >= 1_500_000);
    let page_ranges = ranges(&source);
    let start = Instant::now();
    let index = build_snapcompact_index(&messages, 0, &source, &page_ranges);
    let elapsed = start.elapsed();
    eprintln!(
        "snap_index synthetic: {elapsed:?}, source={} bytes, index={} bytes",
        source.len(),
        index.len()
    );
    assert!(!index.is_empty());
    assert!(
        elapsed < Duration::from_secs(2),
        "index rescans source for every excerpt: {elapsed:?}"
    );
}

#[test]
fn snap_index_overlaps_and_prefix_patterns_match_legacy() {
    let messages = vec![text(
        Role::User,
        "aaaaa\naaaaaa\naaaaaaa\nabcde\nabcdef\nabcdefghi\nλ界λ界λ\n0123456789abcdef completely missing\n0123456789abcdef present",
    )];
    for source in [
        "aaaaaaaaa",
        "aaaaaaaaaa",
        "abcdefghi abcdef",
        "λ界λ界λ界λ",
        "0123456789abcdef present",
    ] {
        let page_ranges = vec![FrameRange {
            index: 0,
            source_start: 0,
            source_end: source.chars().count(),
        }];
        assert_eq!(
            build_snapcompact_index(&messages, 0, source, &page_ranges),
            legacy::build_snapcompact_index(&messages, 0, source, &page_ranges)
        );
    }
}

#[test]
fn snap_index_reuses_only_unchanged_prefix_pages_and_rechecks_suffix() {
    let messages = vec![text(
        Role::User,
        "worker abcdef\naaaaa\nresult = 42\nabcdefghi",
    )];
    let prefix = "worker abcdef|aaaaaa|abcdef";
    let prior_ranges = vec![FrameRange {
        index: 0,
        source_start: 0,
        source_end: prefix.len(),
    }];
    let previous = SnapcompactArchive {
        source: prefix.into(),
        pages: vec![],
        render_key: String::new(),
        decisions: String::new(),
        page_ranges: vec![(0, prefix.len())],
        index: legacy::build_snapcompact_index(&messages, 0, prefix, &prior_ranges),
    };
    for suffix in ["", "ghi|result = 42", "|worker abcdef", "aaaaa", "|aaaaaa"] {
        let source = format!("{prefix}{suffix}");
        let mut page_ranges = prior_ranges.clone();
        page_ranges.push(FrameRange {
            index: 1,
            source_start: prefix.len(),
            source_end: source.len(),
        });
        let expected = legacy::build_snapcompact_index(&messages, 0, &source, &page_ranges);
        assert_eq!(
            build_snapcompact_index_with_previous(
                &messages,
                0,
                &source,
                &page_ranges,
                Some(&previous)
            ),
            expected,
            "suffix={suffix:?}"
        );
        let mut ids = HashMap::new();
        ids.insert("worker abcdef".to_string(), 0);
        let mut found = [Occurrence::Missing];
        assert_eq!(
            prefix_occurrences(
                &previous,
                &source,
                &page_ranges,
                &PageLookup::new(&source, &page_ranges),
                &ids,
                &mut found
            ),
            prefix.len()
        );
        assert!(matches!(found[0], Occurrence::Unique { .. }));
        // A repaginated archive cannot borrow old page provenance.
        let changed = vec![FrameRange {
            index: 0,
            source_start: 0,
            source_end: source.len(),
        }];
        assert_eq!(
            build_snapcompact_index_with_previous(&messages, 0, &source, &changed, Some(&previous)),
            legacy::build_snapcompact_index(&messages, 0, &source, &changed)
        );
    }
    let source = "changed worker abcdef|aaaaaa";
    assert_eq!(
        build_snapcompact_index_with_previous(&messages, 0, source, &prior_ranges, Some(&previous)),
        legacy::build_snapcompact_index(&messages, 0, source, &prior_ranges)
    );
    // A new non-overlapping occurrence can straddle the old source boundary.
    let prefix = "aaaaaaaaa";
    let previous = SnapcompactArchive {
        source: prefix.into(),
        pages: vec![],
        render_key: String::new(),
        decisions: String::new(),
        page_ranges: vec![(0, 9)],
        index: legacy::build_snapcompact_index(
            &messages,
            0,
            prefix,
            &[FrameRange {
                index: 0,
                source_start: 0,
                source_end: 9,
            }],
        ),
    };
    let source = "aaaaaaaaaa";
    let page_ranges = vec![
        FrameRange {
            index: 0,
            source_start: 0,
            source_end: 9,
        },
        FrameRange {
            index: 1,
            source_start: 9,
            source_end: 10,
        },
    ];
    assert_eq!(
        build_snapcompact_index_with_previous(&messages, 0, source, &page_ranges, Some(&previous)),
        legacy::build_snapcompact_index(&messages, 0, source, &page_ranges)
    );
}

#[test]
fn snap_index_randomized_literal_and_page_equivalence() {
    let mut seed = 0x12345678u64;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        seed
    };
    for _ in 0..200 {
        let source: String = (0..160)
            .map(|_| ['a', 'b', 'c', 'λ'][(next() % 4) as usize])
            .collect();
        let chars: Vec<_> = source.chars().collect();
        let messages: Vec<_> = (0..20)
            .map(|_| {
                let start = (next() % 150) as usize;
                text(
                    Role::User,
                    &chars[start..start + 5 + (next() % 6) as usize]
                        .iter()
                        .collect::<String>(),
                )
            })
            .collect();
        let mut page_ranges = vec![
            FrameRange {
                index: 3,
                source_start: 0,
                source_end: 81,
            },
            FrameRange {
                index: 7,
                source_start: 81,
                source_end: 160,
            },
        ];
        if next() % 2 == 0 {
            page_ranges.reverse();
        }
        assert_eq!(
            build_snapcompact_index(&messages, 9, &source, &page_ranges),
            legacy::build_snapcompact_index(&messages, 9, &source, &page_ranges)
        );
    }
}

/// Run only against an explicitly supplied private scratch COPY, never live history.
#[test]
#[ignore = "requires JCODE_SNAPCOMPACT_FIXTURE inside JCODE_SCRATCH_DIR"]
fn snap_index_copied_real_session_golden_and_timing() {
    use jcode_compaction_core::snapcompact::{
        CompactOptions, PreviousArchive, compact, prepare_messages_with,
    };
    use sha2::{Digest, Sha256};
    let scratch = std::path::PathBuf::from(std::env::var("JCODE_SCRATCH_DIR").unwrap())
        .canonicalize()
        .unwrap();
    let fixture = std::path::PathBuf::from(std::env::var("JCODE_SNAPCOMPACT_FIXTURE").unwrap())
        .canonicalize()
        .unwrap();
    assert!(
        fixture.starts_with(&scratch),
        "benchmark refuses live session storage"
    );
    let session: crate::session::Session =
        serde_json::from_slice(&std::fs::read(&fixture).unwrap()).unwrap();
    let messages: Vec<_> = session.messages.iter().map(|m| m.to_message()).collect();
    let state = session.compaction.unwrap();
    let prior = state.snapcompact.as_ref().unwrap();
    let prepared = prepare_messages_with(
        &messages[state.compacted_count..],
        Some(PreviousArchive {
            text: &prior.source,
            is_summary: false,
        }),
        usize::MAX,
    )
    .unwrap();
    let rendered = compact(
        &prepared,
        &CompactOptions {
            model_id: "claude-opus-5-5".into(),
            provider: "vision-test".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let page_ranges: Vec<_> = if rendered.truncated_chars == 0 {
        rendered
            .frames
            .iter()
            .map(|f| FrameRange {
                index: f.index,
                source_start: f.source_start,
                source_end: f.source_end,
            })
            .collect()
    } else {
        Vec::new()
    };
    let started = Instant::now();
    let expected =
        legacy::build_snapcompact_index(&messages, 0, &rendered.source_text, &page_ranges);
    let old_elapsed = started.elapsed();
    let started = Instant::now();
    let actual = build_snapcompact_index(&messages, 0, &rendered.source_text, &page_ranges);
    let cold_elapsed = started.elapsed();
    assert!(
        actual.as_bytes() == expected.as_bytes(),
        "real-session cold index differs from legacy (contents remain private)"
    );
    let started = Instant::now();
    let reused = build_snapcompact_index_with_previous(
        &messages,
        0,
        &rendered.source_text,
        &page_ranges,
        Some(prior),
    );
    let reused_elapsed = started.elapsed();
    assert!(
        reused.as_bytes() == expected.as_bytes(),
        "real-session reused index differs from legacy (contents remain private)"
    );
    eprintln!(
        "snap_index real: old_ms={} cold_ms={} reused_ms={} messages={} source_chars={} index_bytes={} sha256={:x}",
        old_elapsed.as_millis(),
        cold_elapsed.as_millis(),
        reused_elapsed.as_millis(),
        messages.len(),
        rendered.source_text.chars().count(),
        actual.len(),
        Sha256::digest(actual.as_bytes())
    );
    assert!(
        cold_elapsed < Duration::from_millis(500),
        "cold real-session index: {cold_elapsed:?}"
    );
    assert!(
        reused_elapsed < Duration::from_millis(500),
        "reused real-session index: {reused_elapsed:?}"
    );
}

#[test]
#[ignore = "requires JCODE_SNAPCOMPACT_FIXTURE inside JCODE_SCRATCH_DIR"]
fn snap_index_copied_archive_profile() {
    let scratch = std::path::PathBuf::from(std::env::var("JCODE_SCRATCH_DIR").unwrap())
        .canonicalize()
        .unwrap();
    let fixture = std::path::PathBuf::from(std::env::var("JCODE_SNAPCOMPACT_FIXTURE").unwrap())
        .canonicalize()
        .unwrap();
    assert!(fixture.starts_with(scratch));
    let session: crate::session::Session =
        serde_json::from_slice(&std::fs::read(fixture).unwrap()).unwrap();
    let state = session.compaction.unwrap();
    let archive = state.snapcompact.as_ref().unwrap();
    let page_ranges = jcode_compaction_core::snapcompact::legacy_page_ranges(
        &archive.source,
        &state.summary_text,
        archive.pages.len(),
    );
    let messages: Vec<_> = session.messages[..state.compacted_count]
        .iter()
        .map(|m| m.to_message())
        .collect();
    let started = Instant::now();
    let actual = build_snapcompact_index(&messages, 0, &archive.source, &page_ranges);
    eprintln!(
        "stored archive index {:?}, bytes={}",
        started.elapsed(),
        actual.len()
    );
}

#[test]
fn snap_index_classifier_and_normalization_match_legacy() {
    let mut boundaries: Vec<char> = (0..=127).map(char::from).collect();
    boundaries.extend(['λ', '界', 'Ж', '١', '３', 'ſ', 'K', '\u{2003}', '─', '█']);
    for &left in &boundaries {
        for right in ['a', '_', ' ', 'λ', '١', '\u{2003}'] {
            for token in [
                "worker", "should", "12345", "abcdefa", "key = x", "- = x", "/tmp", "~path",
                "no facts",
            ] {
                let line = format!("{left}{token}{right}");
                assert_eq!(
                    interesting(&line, false),
                    legacy::interesting(&line, false),
                    "{line:?}"
                );
                let trimmed = line.trim();
                assert_eq!(
                    normalized_excerpt(trimmed).as_ref(),
                    normalize(trimmed),
                    "{line:?}"
                );
            }
        }
    }
    for line in [
        format!("{} trailing", "a".repeat(399)),
        format!("{}λ trailing", "a".repeat(399)),
        format!("{} trailing", "λ".repeat(399)),
        "\x1b[31mworker = 42\x1b[0m".into(),
    ] {
        let short = excerpt(&line);
        assert_eq!(short, line.chars().take(400).collect::<String>());
        assert_eq!(normalized_excerpt(short).as_ref(), normalize(short));
    }
}
