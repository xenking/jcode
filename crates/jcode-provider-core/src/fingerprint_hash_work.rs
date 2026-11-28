use super::*;
use serde_json::json;
use std::cell::Cell;
use std::sync::Arc;

thread_local! {
    static BYTES: Cell<usize> = const { Cell::new(0) };
}

pub(super) fn record(bytes: usize) {
    BYTES.with(|counter| counter.set(counter.get() + bytes));
}

fn measured<T>(f: impl FnOnce() -> T) -> (T, usize) {
    BYTES.with(|counter| counter.set(0));
    let result = f();
    (result, BYTES.with(Cell::get))
}

fn request(items: Vec<Value>) -> Arc<Value> {
    Arc::new(json!({"include": ["reasoning.encrypted_content"], "input": items, "model": "future"}))
}

fn history() -> Vec<Value> {
    (0..128)
        .map(|i| json!({"role": "user", "id": i, "content": "chunk\n東京\t🧪".repeat(256)}))
        .collect()
}

#[test]
fn appended_turn_hashes_only_new_input_bytes() {
    let cache = ResponsesFingerprintCache::default();
    let mut items = history();
    let stable = items.len();
    items.push(json!({"role": "user", "content": "one-shot"}));
    let first = request(items.clone());
    let (first_hashes, cold_bytes) = measured(|| cache.fingerprints(&first, stable));
    assert_eq!(first_hashes, legacy(&first));
    items.pop();
    items.push(json!({"role": "user", "content": "continue"}));
    items.push(json!({"role": "user", "content": "different one-shot"}));
    let second = request(items);
    let (expected, old_bytes) = measured(|| legacy(&second));
    let (actual, warm_bytes) = measured(|| cache.fingerprints(&second, stable + 1));
    assert_eq!(actual, expected);
    assert!(
        warm_bytes * 10 < old_bytes,
        "old={old_bytes} warm={warm_bytes}"
    );
    eprintln!(
        "SHA bytes: previous={old_bytes}, checkpoint cold={cold_bytes}, checkpoint warm={warm_bytes}"
    );
}

fn frozen_request(request: &Value) -> Value {
    json!({
        "model": request.get("model"),
        "instructions": request.get("instructions"),
        "input": request.get("input"),
        "tools": request.get("tools"),
        "tool_choice": request.get("tool_choice"),
        "parallel_tool_calls": request.get("parallel_tool_calls"),
        "reasoning": request.get("reasoning"),
        "context_management": request.get("context_management"),
        "include": request.get("include"),
        "service_tier": request.get("service_tier"),
        "prompt_cache_key": request.get("prompt_cache_key"),
        "prompt_cache_retention": request.get("prompt_cache_retention"),
    })
}

fn legacy(request: &Value) -> CanonicalFingerprints {
    canonical_fingerprints(
        &frozen_request(request),
        request["input"].as_array().map_or(&[], Vec::as_slice),
        request.get("instructions"),
        Some(request.get("tools").unwrap_or(&Value::Array(Vec::new()))),
    )
}

#[test]
fn checkpoint_preserves_legacy_json_hashes_on_replay_and_option_changes() {
    let cache = ResponsesFingerprintCache::default();
    let mut original = json!({
        "context_management": [{"type":"compaction","threshold":100}],
        "include":["reasoning.encrypted_content"],
        "input":[{"role":"user","content":["日本語\n🦀", -0.0]}],
        "instructions":"system", "model":"future-model",
        "parallel_tool_calls":false, "prompt_cache_key":"cache",
        "prompt_cache_retention":"24h", "reasoning":{"effort":"high"},
        "service_tier":"auto", "tool_choice":{"type":"function","name":"read"},
        "tools":[{"name":"read","parameters":{"type":"object"}}],
        "stream":true,"previous_response_id":"excluded"
    });
    let check = |value: Value| {
        let request = Arc::new(value);
        let count = request["input"].as_array().map_or(0, Vec::len);
        let actual = cache.fingerprints(&request, count);
        let expected = legacy(&request);
        assert_eq!(actual, expected);
        assert_eq!(
            actual.request,
            stable_json_fingerprint(&frozen_request(&request))
        );
        actual
    };
    let first = check(original.clone());
    original["input"][0]["content"][1] = json!(0.0);
    original["input"]
        .as_array_mut()
        .unwrap()
        .push(json!("appended"));
    let changed = check(original.clone());
    assert_ne!(first.items.0[0], changed.items.0[0]);
    original["tools"] = Value::Null;
    let options = check(original.clone());
    assert_eq!(changed.items, options.items);
    assert_ne!(changed.request, options.request);
    original["context_management"][0]["threshold"] = json!(200);
    assert_ne!(options.request, check(original.clone()).request);
    original["input"] = json!([{"role": "user", "content": "compacted"}]);
    check(original);
    for request in [
        json!({}),
        json!({"input": [], "instructions": null, "tools": null}),
        json!({"input": 42, "tools": []}),
    ] {
        check(request);
    }
}

#[test]
#[ignore = "local wall-clock benchmark"]
fn benchmark_responses_fingerprint_checkpoint() {
    let mut items = history();
    let stable = items.len();
    let first = request(items.clone());
    items.push(json!({"role": "user", "content": "continue"}));
    let second = request(items);
    let cache = ResponsesFingerprintCache::default();
    cache.fingerprints(&first, stable);
    let cold = ResponsesFingerprintCache::default();
    let run = |f: &dyn Fn(usize) -> CanonicalFingerprints| {
        let mut durations = (0..101)
            .map(|iteration| {
                let now = Instant::now();
                std::hint::black_box(f(iteration));
                now.elapsed()
            })
            .collect::<Vec<_>>();
        durations.sort_unstable();
        durations[50]
    };
    // Prepare the legacy object outside the timing, as the old path borrowed
    // these fields and did not clone the history.
    let old = frozen_request(&second);
    let input = second["input"].as_array().unwrap();
    let tools = json!([]);
    let legacy = run(&|_| canonical_fingerprints(&old, input, None, Some(&tools)));
    let streamed = run(&|_| cold.fingerprints(&second, 0));
    // Alternate distinct allocations, so equality cannot measure one buffer
    // against itself. Keep the checkpoint before the changing suffix.
    let cached = run(&|i| cache.fingerprints(if i % 2 == 0 { &second } else { &first }, stable));
    eprintln!(
        "{stable} stable items, {} JSON bytes: legacy (ring) {legacy:?}, streamed {streamed:?}, checkpoint {cached:?}",
        serde_json::to_vec(&old).unwrap().len()
    );
}
