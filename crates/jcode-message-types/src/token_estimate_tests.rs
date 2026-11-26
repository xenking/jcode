use super::*;
use serde_json::Value;

const FAMILIES: [(&str, TokenFamily); 3] = [
    ("anthropic", TokenFamily::Anthropic),
    ("openai", TokenFamily::OpenAi),
    ("deepseek", TokenFamily::DeepSeek),
];

fn fixtures() -> Value {
    serde_json::from_str(include_str!("token_estimate_fixtures.json")).expect("fixtures parse")
}

fn counts_of(text: &str) -> [u64; CLASSES] {
    let mut estimator = Estimator::new(TokenFamily::Generic);
    estimator.text(text);
    estimator.counts
}

fn fixture_text(fixtures: &Value, name: &str) -> String {
    fixtures["strings"][name]["text"]
        .as_str()
        .expect("fixture string")
        .to_string()
}

#[test]
fn model_names_map_to_families() {
    for (model, family) in [
        ("claude-opus-5-5", TokenFamily::Anthropic),
        ("anthropic/claude-sonnet-5", TokenFamily::Anthropic),
        ("gpt-6.2-sol", TokenFamily::OpenAi),
        ("openai/gpt-5-codex", TokenFamily::OpenAi),
        ("o3-mini", TokenFamily::OpenAi),
        ("gemini-3-pro", TokenFamily::Gemini),
        ("deepseek-v4-pro", TokenFamily::DeepSeek),
        ("openrouter/deepseek/deepseek-chat", TokenFamily::DeepSeek),
        ("kimi-k3", TokenFamily::Generic),
        ("ollama/orca-mini", TokenFamily::Generic),
        ("", TokenFamily::Generic),
    ] {
        assert_eq!(TokenFamily::from_model(model), family, "{model}");
    }
}

#[test]
fn classifier_matches_the_script_that_fitted_the_weights() {
    let fixtures = fixtures();
    for (name, sample) in fixtures["strings"].as_object().unwrap() {
        let expected: Vec<u64> = sample["counts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_u64().unwrap())
            .collect();
        let text = sample["text"].as_str().unwrap();
        assert_eq!(counts_of(text).to_vec(), expected, "{name}");
    }
}

/// Reference values come from the fitted model evaluated in the calibration
/// script, so this pins the port, not the tokenizer. The o200k tokenizer, an
/// independent public proxy for the OpenAI family, counts 77/99/122/63 tokens
/// for the same four strings.
#[test]
fn representative_strings_per_family() {
    let fixtures = fixtures();
    let cases: [(&str, [usize; 5]); 4] = [
        ("anthropic", [144, 149, 167, 120, 64]),
        ("openai", [97, 101, 117, 73, 52]),
        ("deepseek", [96, 120, 112, 80, 40]),
        ("default", [104, 139, 120, 82, 54]),
    ];
    let names = [
        "english_prose",
        "russian_prose",
        "rust_code",
        "json_tool_output",
        "cjk_and_symbols",
    ];
    for (family, expected) in cases {
        let family = match family {
            "anthropic" => TokenFamily::Anthropic,
            "openai" => TokenFamily::OpenAi,
            "deepseek" => TokenFamily::DeepSeek,
            _ => TokenFamily::Generic,
        };
        for (name, expected) in names.iter().zip(expected) {
            let got = estimate_text(family, &fixture_text(&fixtures, name)) as f64;
            let tolerance = (expected as f64 * 0.03).max(2.0);
            assert!(
                (got - expected as f64).abs() <= tolerance,
                "{family:?} {name}: got {got}, expected {expected} +/- {tolerance}"
            );
        }
    }
}

#[test]
fn families_disagree_where_tokenizers_do() {
    let fixtures = fixtures();
    for name in ["english_prose", "rust_code", "json_tool_output"] {
        let text = fixture_text(&fixtures, name);
        assert!(
            estimate_text(TokenFamily::Anthropic, &text)
                > estimate_text(TokenFamily::OpenAi, &text),
            "Claude's tokenizer is denser than OpenAI's on {name}"
        );
    }
}

#[test]
fn cyrillic_is_not_charged_by_utf8_bytes() {
    let fixtures = fixtures();
    let russian = fixture_text(&fixtures, "russian_prose");
    let english = fixture_text(&fixtures, "english_prose");
    for (_, family) in FAMILIES {
        let ratio = estimate_text(family, &russian) as f64 / estimate_text(family, &english) as f64;
        // 398 Cyrillic chars (732 bytes) cost about as much as 418 English
        // chars; a byte-based rule would charge 1.75x.
        assert!((0.8..1.5).contains(&ratio), "{family:?}: {ratio}");
    }
}

#[test]
fn empty_is_free_and_anything_else_costs_a_token() {
    for family in [TokenFamily::Anthropic, TokenFamily::Generic] {
        assert_eq!(estimate_text(family, ""), 0);
        assert_eq!(Estimator::new(family).tokens(), 0);
        assert_eq!(estimate_text(family, " "), 1);
        assert_eq!(estimate_text(family, "a"), 1);
    }
}

#[test]
fn growing_text_never_shrinks_and_concatenation_is_additive() {
    let fixtures = fixtures();
    for (_, family) in FAMILIES {
        let mut previous = 0;
        let prose = fixture_text(&fixtures, "english_prose");
        for end in (0..=prose.len()).filter(|i| prose.is_char_boundary(*i)) {
            let tokens = estimate_text(family, &prose[..end]);
            assert!(tokens >= previous, "{family:?} at {end}");
            previous = tokens;
        }
        for (a, b) in [
            ("english_prose", "rust_code"),
            ("russian_prose", "json_tool_output"),
        ] {
            let (a, b) = (fixture_text(&fixtures, a), fixture_text(&fixtures, b));
            let joined = estimate_text(family, &format!("{a}\n{b}"));
            let apart = estimate_text(family, &a) + estimate_text(family, &b);
            // One newline between them, plus rounding of each part.
            assert!(
                joined.abs_diff(apart) <= 3,
                "{family:?}: {joined} vs {apart}"
            );
        }
    }
}

#[test]
fn streaming_a_serializer_equals_estimating_its_output() {
    use std::fmt::Write as _;
    let value = serde_json::json!({"a": [1, 2, 3], "b": {"c": "d e f"}});
    let mut streamed = Estimator::new(TokenFamily::OpenAi);
    write!(streamed, "{value}").unwrap();
    assert_eq!(
        streamed.tokens(),
        estimate_text(TokenFamily::OpenAi, &value.to_string())
    );
}

#[test]
fn budget_bytes_round_trip_below_the_estimate() {
    let fixtures = fixtures();
    let text = fixture_text(&fixtures, "rust_code").repeat(20);
    for (_, family) in FAMILIES {
        let cap = family.bytes_for_tokens(1_000);
        let tokens = estimate_text(family, &text[..cap.min(text.len())]);
        assert!(
            tokens <= 1_300,
            "{family:?}: a 1000-token cap held {tokens}"
        );
    }
}

/// Class counts and reported token deltas from held-out recorded sessions.
/// This guards the weight tables against edits that stop matching real usage.
#[test]
fn held_out_recorded_usage_stays_within_measured_error() {
    let fixtures = fixtures();
    // 60 random held-out samples per family: median 4-5%, p90 15-20%. The full
    // held-out set has a p95 of 36-50% from content the estimator cannot see.
    for (name, family) in FAMILIES {
        let mut errors: Vec<f64> = fixtures["samples"][name]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let row = row.as_array().unwrap();
                let n = |i: usize| row[i].as_u64().unwrap();
                let mut counts = [0u64; CLASSES];
                for (slot, value) in counts.iter_mut().zip(row[0].as_array().unwrap()) {
                    *slot = value.as_u64().unwrap();
                }
                let mut estimator = Estimator::from_counts(family, counts);
                estimator.bytes(n(1) as usize, family.calibration().reasoning);
                estimator.signature(n(2) as usize).encrypted(n(3) as usize);
                estimator.native_json(n(4) as usize);
                for _ in 0..n(5) {
                    estimator.text_block();
                }
                for _ in 0..n(6) {
                    estimator.tool_call();
                }
                let reported = n(7) as f64;
                (estimator.tokens() as f64 - reported).abs() / reported
            })
            .collect();
        errors.sort_by(f64::total_cmp);
        let median = errors[errors.len() / 2];
        assert!(median < 0.10, "{name}: median error {median:.3}");
        let p90 = errors[errors.len() * 9 / 10];
        assert!(p90 < 0.30, "{name}: p90 error {p90:.3}");
    }
}

#[test]
fn unknown_families_fall_back_to_the_generic_table() {
    let text = "plain text with some, punctuation. And 42 numbers";
    assert_eq!(
        estimate_text(TokenFamily::Gemini, text),
        estimate_text(TokenFamily::Generic, text)
    );
    assert_eq!(TokenFamily::default(), TokenFamily::Generic);
}

#[test]
fn prefix_within_is_the_longest_prefix_under_the_budget() {
    let fixtures = fixtures();
    for (_, family) in FAMILIES {
        for name in ["russian_prose", "rust_code", "cjk_and_symbols"] {
            let text = fixture_text(&fixtures, name);
            for budget in [0, 1, 7, 40, 10_000] {
                let end = family.prefix_within(&text, budget);
                assert!(text.is_char_boundary(end), "{family:?} {name} {budget}");
                assert!(estimate_text(family, &text[..end]) <= budget);
                if end < text.len() {
                    let next = end + text[end..].chars().next().unwrap().len_utf8();
                    assert!(estimate_text(family, &text[..next]) > budget);
                }
            }
        }
    }
}
