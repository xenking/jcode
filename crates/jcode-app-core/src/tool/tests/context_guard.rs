//! Context-guard behavior of the registry: oversized output is withheld, truncated or
//! refused against the session's remaining context budget.

use super::*;

#[tokio::test]
async fn test_context_guard_never_spends_more_than_it_reports() {
    // State-space sweep over budget, fill level, and payload size. Two
    // invariants must hold in every combination, because the whole point of the
    // guard is that a caller can trust the accounting:
    //   1. Without the opt-in, the returned text is small. Refusing has to be
    //      cheap or it reproduces the bug it prevents.
    //   2. The returned text never exceeds the remaining safety headroom, with
    //      or without the opt-in. Otherwise "accept the cost" would silently
    //      overrun the window.
    for budget in [10_000usize, 50_000, 200_000] {
        for fill_percent in [0usize, 25, 50, 80, 89, 95] {
            for payload_tokens in [1usize, 500, 5_000, 100_000] {
                for accept in [false, true] {
                    let compaction =
                        Arc::new(RwLock::new(CompactionManager::new().with_budget(budget)));
                    let used = budget * fill_percent / 100;
                    if used > 0 {
                        let mut mgr = compaction.write().await;
                        mgr.update_observed_input_tokens(used as u64);
                    }
                    let registry = Registry {
                        mcp_policy: Arc::default(),
                        tools: Arc::new(RwLock::new(HashMap::new())),
                        skills: Arc::new(RwLock::new(crate::skill::SkillRegistry::default())),
                        compaction,
                    };

                    let payload = "x".repeat(payload_tokens * 4);
                    let result = registry
                        .guard_context_overflow("test", ToolOutput::new(payload.clone()), accept)
                        .await;
                    let returned_tokens = result.output.len() / 4;

                    let threshold = (budget as f32 * 0.90) as usize;
                    let headroom = threshold.saturating_sub(used);
                    let passed_through = result.output == payload;

                    if !accept && !passed_through {
                        assert!(
                            result.output.len() < 1_500,
                            "refusal must stay cheap: budget={budget} fill={fill_percent} \
                             payload={payload_tokens} returned {} chars",
                            result.output.len()
                        );
                    }

                    // Allow a small slack for the notice text appended after the slice.
                    assert!(
                        returned_tokens <= headroom.max(1_000) + 500,
                        "returned ~{returned_tokens}k tokens with only {headroom} headroom: \
                         budget={budget} fill={fill_percent} payload={payload_tokens} \
                         accept={accept}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn test_context_guard_refusal_reads_clearly_for_todays_regression() {
    // The exact shape that motivated this change: a 233k-token agentgrep result
    // against a 200k budget with 18k already used. Printed so the wording stays
    // reviewable, and asserted so it keeps naming the cost and the escape hatch.
    let compaction = Arc::new(RwLock::new(CompactionManager::new().with_budget(200_000)));
    {
        let mut mgr = compaction.write().await;
        mgr.update_observed_input_tokens(18_000);
    }
    let registry = Registry {
        mcp_policy: Arc::default(),
        tools: Arc::new(RwLock::new(HashMap::new())),
        skills: Arc::new(RwLock::new(crate::skill::SkillRegistry::default())),
        compaction,
    };

    let result = registry
        .guard_context_overflow("agentgrep", ToolOutput::new("x".repeat(932_000)), false)
        .await;
    println!("---\n{}\n---", result.output);

    assert!(result.output.contains("233k tokens"));
    assert!(result.output.contains("116%"), "got: {}", result.output);
    assert!(result.output.contains("18k is already used"));
    assert!(result.output.contains("accept_large_output"));
    assert!(result.output.contains("paths_only"));
}

/// Tool that returns a fixed-size payload, for exercising the guard through the
/// real `execute()` path rather than by calling the guard directly.
struct BigOutputTool {
    chars: usize,
}

#[async_trait]
impl Tool for BigOutputTool {
    fn name(&self) -> &str {
        "big_output"
    }

    fn description(&self) -> &str {
        "Returns a large fixed payload for context guard tests."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: Value, _ctx: ToolContext) -> Result<ToolOutput> {
        Ok(ToolOutput::new("x".repeat(self.chars)))
    }
}

async fn execute_big_output(input: Value) -> String {
    // A fresh session per call: repeating a withheld call accepts its cost, so
    // tests sharing a session would accept each other's refusals.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    execute_big_output_in(&format!("test-context-guard-execute-{id}"), input).await
}

async fn execute_big_output_in(session_id: &str, input: Value) -> String {
    // Every call gets its own invocation id, like the model issuing a fresh tool
    // call. A repeat is a *later* invocation: if two calls shared an id they would
    // be one invocation retried, which is not what "repeat the same call" means.
    static CALL_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider).await;
    {
        let mut mgr = registry.compaction.write().await;
        *mgr = CompactionManager::new().with_budget(10_000);
    }
    registry
        .register(
            "big_output".to_string(),
            Arc::new(BigOutputTool { chars: 400_000 }),
        )
        .await;

    let ctx = ToolContext {
        session_id: session_id.to_string(),
        message_id: "test".to_string(),
        tool_call_id: format!(
            "test-call-{}",
            CALL_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ),
        working_dir: Some(std::env::temp_dir()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::Direct,
    };

    registry
        .execute("big_output", input, ctx)
        .await
        .expect("tool should succeed")
        .output
}

#[tokio::test]
async fn test_execute_withholds_oversized_output_by_default() {
    // The guard is only useful if it runs on the real call path. Every other
    // test calls guard_context_overflow directly, which would still pass if the
    // flag were never plumbed through execute().
    let output = execute_big_output(serde_json::json!({ "intent": "test" })).await;
    assert!(
        output.contains("OUTPUT WITHHELD"),
        "execute() must apply the guard, got: {output}"
    );
    assert!(
        output.len() < 1_500,
        "withheld output should be cheap, got {} chars",
        output.len()
    );
}

#[tokio::test]
async fn test_execute_honors_accept_large_output_from_raw_input() {
    // Proves the flag survives the trip through execute(): the tool itself never
    // declares or reads `accept_large_output`, so this only works because the
    // registry reads it off the raw input.
    let output =
        execute_big_output(serde_json::json!({ "intent": "test", "accept_large_output": true }))
            .await;
    assert!(
        output.contains("OUTPUT TRUNCATED"),
        "opt-in should return truncated payload, got: {}",
        &output[..output.len().min(200)]
    );
    assert!(
        output.starts_with(&"x".repeat(200)),
        "opt-in must actually return payload"
    );
}

#[tokio::test]
async fn test_execute_ignores_a_non_boolean_accept_flag() {
    // A truthy-looking value must not spend the window.
    let output =
        execute_big_output(serde_json::json!({ "intent": "test", "accept_large_output": 1 })).await;
    assert!(
        output.contains("OUTPUT WITHHELD"),
        "numeric 1 must not opt in, got: {}",
        &output[..output.len().min(200)]
    );
}

#[tokio::test]
async fn test_no_tool_schema_carries_the_large_output_flag() {
    // A property on every schema is paid on every request, forever. The refusal
    // message names the flag instead, and repeating the call also accepts, so
    // no schema needs to advertise it.
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider).await;
    registry.register_ambient_tools().await;

    let defs = registry.definitions(None).await;
    assert!(
        defs.len() > 20,
        "expected the full tool set, got {}",
        defs.len()
    );

    let advertising: Vec<&str> = defs
        .iter()
        .filter(|def| {
            def.input_schema["properties"]
                .get(jcode_tool_core::ACCEPT_LARGE_OUTPUT_KEY)
                .is_some()
        })
        .map(|def| def.name.as_str())
        .collect();
    assert!(
        advertising.is_empty(),
        "tools paying for the accept_large_output property: {advertising:?}"
    );
}

#[tokio::test]
async fn test_repeating_a_withheld_call_accepts_its_cost() {
    // Strict-schema providers cannot send an undeclared key, so the repeat is
    // their only way to opt in. The reworded intent must not break the match.
    let session = "test-context-guard-repeat";
    let first = execute_big_output_in(session, serde_json::json!({ "intent": "look" })).await;
    assert!(first.contains("OUTPUT WITHHELD"), "got: {first}");
    assert!(
        first.contains("repeat the same call"),
        "refusal must explain the repeat: {first}"
    );

    let second =
        execute_big_output_in(session, serde_json::json!({ "intent": "look again" })).await;
    assert!(
        second.contains("OUTPUT TRUNCATED"),
        "repeat should return the payload, got: {}",
        &second[..second.len().min(200)]
    );

    // One refusal buys one accepted repeat, not a standing permission.
    let third = execute_big_output_in(session, serde_json::json!({ "intent": "look" })).await;
    assert!(third.contains("OUTPUT WITHHELD"), "got: {third}");
}

#[tokio::test]
async fn test_a_different_call_does_not_accept_a_withheld_one() {
    let session = "test-context-guard-different";
    let first = execute_big_output_in(session, serde_json::json!({ "intent": "a" })).await;
    assert!(first.contains("OUTPUT WITHHELD"));
    let other =
        execute_big_output_in(session, serde_json::json!({ "intent": "a", "query": "x" })).await;
    assert!(
        other.contains("OUTPUT WITHHELD"),
        "changed arguments are a new call, got: {}",
        &other[..other.len().min(200)]
    );
}

#[tokio::test]
async fn test_batch_guards_both_its_subcalls_and_its_own_aggregate() {
    // Batch is how oversized results actually arrive in practice: several
    // searches fan out at once. Two separate guard applications matter here, and
    // the aggregate one is the load-bearing case: batch concatenates every
    // sub-result, so even if each sub-call were individually acceptable the
    // combined output can blow the window. That aggregate is what withheld
    // today's regression.
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider).await;
    {
        let mut mgr = registry.compaction.write().await;
        *mgr = CompactionManager::new().with_budget(10_000);
    }
    registry
        .register(
            "big_output".to_string(),
            Arc::new(BigOutputTool { chars: 400_000 }),
        )
        .await;

    let ctx = |name: &str| ToolContext {
        session_id: format!("test-batch-context-guard-{name}"),
        message_id: "test".to_string(),
        tool_call_id: "test".to_string(),
        working_dir: Some(std::env::temp_dir()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::Direct,
    };
    let calls = serde_json::json!([
        { "tool": "big_output", "intent": "one" },
        { "tool": "big_output", "intent": "two" },
    ]);

    // Without an opt-in anywhere, nothing large escapes: no payload reaches the
    // transcript, only the refusal.
    let withheld = registry
        .execute(
            "batch",
            serde_json::json!({ "intent": "test", "tool_calls": calls }),
            ctx("withheld"),
        )
        .await
        .expect("batch should succeed")
        .output;
    assert!(
        withheld.contains("OUTPUT WITHHELD"),
        "batch output must be guarded, got: {}",
        &withheld[..withheld.len().min(300)]
    );
    assert!(
        !withheld.contains(&"x".repeat(100)),
        "no payload should survive when nothing opted in"
    );

    // Opting in at the batch level returns the aggregate, which is the level a
    // caller reads. The sub-calls' own refusals are inside it, since each was
    // guarded separately and neither sub-call opted in.
    let accepted = registry
        .execute(
            "batch",
            serde_json::json!({
                "intent": "test",
                "accept_large_output": true,
                "tool_calls": calls,
            }),
            ctx("accepted"),
        )
        .await
        .expect("batch should succeed")
        .output;
    // The aggregate is now returned rather than withheld: it carries the
    // per-subcall section headers, which the withheld version never reaches.
    assert!(
        !accepted.starts_with("⚠️ OUTPUT WITHHELD"),
        "batch-level opt-in should return the aggregate, got: {}",
        &accepted[..accepted.len().min(200)]
    );
    assert!(
        accepted.contains("--- [1] big_output ---"),
        "aggregate should contain per-subcall sections, got: {}",
        &accepted[..accepted.len().min(300)]
    );
    // The batch-level opt-in is forwarded to the sub-calls, so they return
    // truncated payloads instead of refusals, and no sub-call is withheld.
    // Sub-call-level guarding is pinned by
    // `batch_subcalls_are_guarded_with_the_session_budget`.
    assert_eq!(
        accepted.matches("OUTPUT WITHHELD").count(),
        0,
        "a batch-level opt-in covers its sub-calls: {}",
        &accepted[..accepted.len().min(300)]
    );
    assert!(
        accepted.contains("OUTPUT TRUNCATED"),
        "opt-in returns truncated sub-call payloads, got tail: {}",
        &accepted[accepted.len().saturating_sub(300)..]
    );
}

/// Sub-calls are guarded against the *session's* context, not a default one.
/// This used to be decided by a fresh `CompactionManager` per sub-call, so a
/// batch admitted outputs that the session could not afford, and the outcome
/// depended on the default budget rather than the caller's.
#[tokio::test]
async fn batch_subcalls_are_guarded_with_the_session_budget() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider).await;
    {
        let mut mgr = registry.compaction.write().await;
        *mgr = CompactionManager::new().with_budget(20_000);
    }
    // ~7.5k tokens: affordable for a 200k default budget, too big for this
    // session's 20k budget (single-output cap 6k tokens).
    registry
        .register(
            "big_output".to_string(),
            Arc::new(BigOutputTool { chars: 30_000 }),
        )
        .await;

    let output = registry
        .execute(
            "batch",
            serde_json::json!({
                "intent": "test",
                "tool_calls": [
                    { "tool": "big_output", "intent": "one" },
                    { "tool": "big_output", "intent": "two" },
                ],
            }),
            ToolContext {
                session_id: "test-batch-session-budget".to_string(),
                message_id: "test".to_string(),
                tool_call_id: "test".to_string(),
                working_dir: Some(std::env::temp_dir()),
                stdin_request_tx: None,
                graceful_shutdown_signal: None,
                execution_mode: ToolExecutionMode::Direct,
            },
        )
        .await
        .expect("batch should succeed")
        .output;

    assert_eq!(
        output.matches("OUTPUT WITHHELD").count(),
        2,
        "both sub-calls must be judged against the session budget, got: {}",
        &output[..output.len().min(400)]
    );
    assert!(
        !output.contains(&"x".repeat(100)),
        "no payload may survive a refused sub-call"
    );
}

/// Two identical sub-calls in one batch are two calls, not a refusal plus its
/// accepted repeat. They share a repeat key (only `intent` differs, which is not
/// identifying), so the second one used to spend the first one's record and slip
/// through as an opt-in. Which one lost that race depended on completion order,
/// which is what made the aggregate test flaky.
#[tokio::test]
async fn a_sibling_subcall_is_not_a_repeated_withheld_call() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider).await;
    {
        let mut mgr = registry.compaction.write().await;
        *mgr = CompactionManager::new().with_budget(10_000);
    }
    registry
        .register(
            "big_output".to_string(),
            Arc::new(BigOutputTool { chars: 400_000 }),
        )
        .await;

    let mut counts = Vec::new();
    for attempt in 0..8 {
        let output = registry
            .execute(
                "batch",
                serde_json::json!({
                    "intent": "test",
                    "tool_calls": [
                        { "tool": "big_output", "intent": "one" },
                        { "tool": "big_output", "intent": "two" },
                    ],
                }),
                ToolContext {
                    session_id: format!("test-batch-sibling-repeat-{attempt}"),
                    message_id: "test".to_string(),
                    tool_call_id: format!("call-{attempt}"),
                    working_dir: Some(std::env::temp_dir()),
                    stdin_request_tx: None,
                    graceful_shutdown_signal: None,
                    execution_mode: ToolExecutionMode::Direct,
                },
            )
            .await
            .expect("batch should succeed")
            .output;
        counts.push(output.matches("OUTPUT WITHHELD").count());
    }

    assert!(
        counts.iter().all(|count| *count == 2),
        "both siblings must be withheld on every run, got {counts:?}"
    );
}

#[tokio::test]
async fn test_guard_withholds_large_output_on_a_million_token_window() {
    // The regression that made every other test in this file misleading. They
    // all pinned budgets of 1k to 200k, where 30% of the budget is a small
    // number. Production reported a 1M-token window, so 30% permitted a 300k
    // single result and a repo-wide grep costing 233k tokens sailed straight
    // through a guard that had unit tests passing.
    let compaction = Arc::new(RwLock::new(CompactionManager::new().with_budget(1_000_000)));
    {
        let mut mgr = compaction.write().await;
        mgr.update_observed_input_tokens(21_000);
    }
    let registry = Registry {
        mcp_policy: Arc::default(),
        tools: Arc::new(RwLock::new(HashMap::new())),
        skills: Arc::new(RwLock::new(crate::skill::SkillRegistry::default())),
        compaction,
    };

    // ~233k tokens: the real size of the agentgrep result that started this.
    let output = ToolOutput::new("x".repeat(932_000));
    let result = registry
        .guard_context_overflow("agentgrep", output, false)
        .await;

    assert!(
        result.output.contains("OUTPUT WITHHELD"),
        "a 233k-token result must be withheld even on a 1M window, got: {}",
        &result.output[..result.output.len().min(200)]
    );
    assert!(
        result.output.len() < 1_500,
        "refusal should cost ~120 tokens, not {} chars",
        result.output.len()
    );
}

#[tokio::test]
async fn test_single_output_ceiling_is_absolute_not_only_proportional() {
    // Guards the invariant directly: however large the window, one tool result
    // may never exceed the absolute ceiling. Without this, raising a model's
    // advertised context window silently raises the per-call blast radius.
    for budget in [200_000usize, 1_000_000, 2_000_000, 10_000_000] {
        let compaction = Arc::new(RwLock::new(CompactionManager::new().with_budget(budget)));
        let registry = Registry {
            mcp_policy: Arc::default(),
            tools: Arc::new(RwLock::new(HashMap::new())),
            skills: Arc::new(RwLock::new(crate::skill::SkillRegistry::default())),
            compaction,
        };

        // Just over the absolute ceiling, but a trivial fraction of a huge window.
        let over_ceiling_tokens = Registry::SINGLE_OUTPUT_MAX_TOKENS + 10_000;
        let result = registry
            .guard_context_overflow(
                "test",
                ToolOutput::new("x".repeat(over_ceiling_tokens * 4)),
                false,
            )
            .await;
        assert!(
            result.output.contains("OUTPUT WITHHELD"),
            "budget={budget}: {over_ceiling_tokens} tokens must exceed the absolute ceiling"
        );
    }
}
