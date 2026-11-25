//! The tool phase of a streamed turn: validate the calls the model made, run
//! each with a foreground budget (handing slow ones to the background), and
//! record the results before the next provider request.

use super::*;
use crate::agent::tool_handoff::{self, reload_interrupted_tool_result, wait_for_tool_handoff};
use crate::message::ToolDefinition;
use crate::stream_batching::BatchingSender;

/// What the caller's turn loop does after a tool round.
pub(super) enum ToolRoundEnd {
    /// Results are recorded; request the next completion.
    NextRequest,
    /// The turn ended inside the round (cancelled or interrupted by reload).
    TurnOver,
}

/// Everything one tool round needs from the streaming turn that produced it.
pub(super) struct StreamedToolRound<'a> {
    pub(super) event_tx: &'a BatchingSender,
    pub(super) tool_calls: &'a [ToolCall],
    pub(super) sdk_tool_results: &'a mut HashMap<String, (String, bool)>,
    pub(super) assistant_message_id: &'a Option<String>,
    pub(super) trace: bool,
    pub(super) inline_output_tap: bool,
    pub(super) turn_cancel_guard: &'a crate::turn_cancel_registry::ActiveTurnGuard,
    pub(super) tools: &'a [ToolDefinition],
    pub(super) generated_image_contexts: &'a mut Vec<Vec<ContentBlock>>,
    pub(super) consecutive_malformed_tool_rounds: &'a mut u32,
}

struct ForegroundToolAbortGuard(Option<tokio::task::AbortHandle>);

impl Drop for ForegroundToolAbortGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

impl Agent {
    pub(super) async fn run_streamed_tool_round(
        &mut self,
        round: StreamedToolRound<'_>,
    ) -> Result<ToolRoundEnd> {
        let StreamedToolRound {
            event_tx,
            tool_calls,
            sdk_tool_results,
            assistant_message_id,
            trace,
            inline_output_tap,
            turn_cancel_guard,
            tools,
            generated_image_contexts,
            consecutive_malformed_tool_rounds,
        } = round;
        // Execute tools and add results
        let tool_count = tool_calls.len();
        let mut malformed_calls: Vec<MalformedToolCallInfo> = Vec::new();
        let mut executed_valid_call = false;
        let mut tool_results_dirty = false;
        let mut parallel_reads = super::parallel_reads::ParallelReads::default();
        for tool_index in 0..tool_count {
            // === INJECTION POINT C (before): Check for urgent abort before each tool (except first) ===
            if tool_index > 0 && self.has_urgent_interrupt() {
                crate::telemetry::record_user_cancelled();
                // Add tool_results for all remaining skipped tools to maintain valid history
                for skipped_tc in &tool_calls[tool_index..] {
                    self.add_message(
                        Role::User,
                        vec![ContentBlock::ToolResult {
                            tool_use_id: skipped_tc.id.clone(),
                            content: "[Skipped: user interrupted]".to_string(),
                            is_error: Some(true),
                        }],
                    );
                }
                let tools_remaining = tool_count - tool_index;
                let injected = self.inject_soft_interrupts();
                if !injected.is_empty() {
                    for event in
                        Self::build_soft_interrupt_events(injected, "C", Some(tools_remaining))
                    {
                        let _ = event_tx.send(event);
                    }
                    // Add note about skipped tools for the AI
                    self.add_message(
                        Role::User,
                        vec![ContentBlock::Text {
                            text: format!(
                                "[User interrupted: {} remaining tool(s) skipped]",
                                tools_remaining
                            ),
                            cache_control: None,
                        }],
                    );
                }
                self.persist_session_best_effort("streamed tool output");
                break; // Skip remaining tools
            }
            let tc = &tool_calls[tool_index];

            let message_id = assistant_message_id
                .clone()
                .unwrap_or_else(|| self.session.id.clone());

            if parallel_reads.is_empty() {
                parallel_reads = self
                    .start_parallel_reads(&tool_calls[tool_index..], &sdk_tool_results, &message_id)
                    .await;
            }

            if let Some(error_msg) = tc.validation_error() {
                malformed_calls.push(MalformedToolCallInfo {
                    name: tc.name.clone(),
                    error: error_msg.clone(),
                });
                logging::warn(&error_msg);
                let _ = event_tx.send(ServerEvent::ToolDone {
                    codemode: None,
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    output: error_msg.clone(),
                    error: Some(error_msg.clone()),
                });
                self.add_message(
                    Role::User,
                    vec![ContentBlock::ToolResult {
                        tool_use_id: tc.id.clone(),
                        content: error_msg,
                        is_error: Some(true),
                    }],
                );
                tool_results_dirty = true;
                continue;
            }

            // A call that passed validation is genuinely valid even if
            // the tool itself later fails: it resets the malformed-round
            // bound in handle_malformed_tool_round below.
            executed_valid_call = true;
            self.validate_tool_allowed(&tc.name)?;

            let is_native_tool = JCODE_NATIVE_TOOLS.contains(&tc.name.as_str());

            if let Some((sdk_content, sdk_is_error)) = sdk_tool_results.remove(&tc.id) {
                // For native tools, ignore SDK errors and execute locally
                if !(is_native_tool && sdk_is_error) {
                    let sdk_content = cap_sdk_tool_content_for_history(&tc.name, sdk_content);
                    self.add_message(
                        Role::User,
                        vec![ContentBlock::ToolResult {
                            tool_use_id: tc.id.clone(),
                            content: sdk_content,
                            is_error: if sdk_is_error { Some(true) } else { None },
                        }],
                    );
                    tool_results_dirty = true;

                    // NOTE: No injection here - wait for Point D after all tools

                    continue;
                }
                // Fall through to local execution for native tools with SDK errors
            }

            let ctx = ToolContext {
                session_id: self.session.id.clone(),
                message_id: message_id.clone(),
                tool_call_id: tc.id.clone(),
                working_dir: self.working_dir().map(PathBuf::from),
                stdin_request_tx: self.stdin_request_tx.clone(),
                graceful_shutdown_signal: Some(self.graceful_shutdown.clone()),
                execution_mode: ToolExecutionMode::AgentTurn,
            };

            if trace {
                eprintln!("[trace] tool_exec_start name={} id={}", tc.name, tc.id);
            }

            logging::debug_lazy(|| format!("Tool starting: {}", tc.name));
            crate::session_metrics::record_activity(&self.session.id);
            if inline_output_tap {
                // Surface the tool execution on the coordinator's inline
                // viewport immediately: workers spend most wall-clock time
                // here, where no assistant text streams.
                self.inline_tail.start_tool(&tc.name, &tc.input);
                self.publish_inline_tail();
            }
            let tool_start = Instant::now();

            // Spawn independently so automatic and Alt+B handoffs keep it running.
            let registry_clone = self.registry.clone();
            let tool_name_for_spawn = tc.name.clone();
            let tool_input_for_spawn = tc.input.clone();
            let bash_handoff = crate::tool::BashHandoff::for_tool(&tc.name);
            let bash_handoff_for_tool = bash_handoff.clone();
            let sole_rollout_wait = tc.name == crate::tool::rollout_wait::NAME && tool_count == 1;
            if sole_rollout_wait {
                self.persist_session_now()?;
            }
            let (tool_handle, started) =
                if let Some((handle, started)) = parallel_reads.take(&tc.id) {
                    (handle, Some(started))
                } else {
                    (
                        crate::runtime_activity::spawn_session(&self.session.id, async move {
                            let execution = tool_handoff::run_tool(
                                registry_clone,
                                tool_name_for_spawn,
                                tool_input_for_spawn,
                                ctx,
                                bash_handoff_for_tool,
                            );
                            if sole_rollout_wait {
                                crate::tool::rollout_wait::SOLE_CALL
                                    .scope((), execution)
                                    .await
                            } else {
                                execution.await
                            }
                        })
                        .ok_or_else(|| {
                            anyhow::anyhow!("Session transfer in progress; tool not accepted")
                        })?,
                        None,
                    )
                };

            // Reset background signal before waiting
            self.background_tool_signal.reset();

            // All tools share the same default foreground budget, including
            // tools without their own background execution support.
            let bg_signal = self.background_tool_signal.clone();
            let shutdown_signal = self.graceful_shutdown.clone();
            let foreground_budget = tool_handoff::foreground_budget();
            let handoff = tool_handoff::foreground_limit(
                &self.registry,
                &tc.name,
                &tc.input,
                &self.session.id,
                foreground_budget,
            )
            .await
            .map(|budget| {
                started.map_or(budget, |started| budget.saturating_sub(started.elapsed()))
            });
            let allow_reload_handoff = tc.name == "bash";
            let tool_result;
            let mut automatic_background = false;
            let mut tool_handle = tool_handle;
            let mut abort_tool_on_drop = ForegroundToolAbortGuard(Some(tool_handle.abort_handle()));
            let running_tool_guard =
                crate::running_tool_registry::register(self.session_id(), &tc.id, &tc.name);
            tokio::select! {
                biased;
                res = &mut tool_handle => {
                    tool_result = Some(match res {
                        Ok(r) => r,
                        Err(e) => Err(anyhow::anyhow!("Tool task panicked: {}", e)),
                    });
                }
                automatic = wait_for_tool_handoff(&bg_signal, &shutdown_signal, handoff, bash_handoff.as_ref()) => {
                    automatic_background = automatic;
                    if !shutdown_signal.is_set() && let Some(handoff) = &bash_handoff {
                        handoff.request.fire();
                    }
                    if allow_reload_handoff && (!shutdown_signal.is_set() || turn_cancel_guard.interruption_reason() == Some(crate::turn_cancel_registry::TurnInterruptionReason::Reload)) {
                        tool_result = match tokio::time::timeout(
                            tool_handoff::CANCEL_TOOL_GRACE,
                            &mut tool_handle,
                        )
                        .await
                        {
                            Ok(res) => Some(match res {
                                Ok(r) => r,
                                Err(e) => Err(anyhow::anyhow!("Tool task panicked: {}", e)),
                            }),
                            Err(_) if !shutdown_signal.is_set() && cfg!(unix) => {
                                tool_handle.abort();
                                Some(Err(anyhow::anyhow!("Bash did not acknowledge background handoff")))
                            }
                            Err(_) => None,
                        };
                    } else {
                        tool_result = None;
                    }
                }
            };
            drop(running_tool_guard);

            self.unlock_tools_if_needed(&tc.name);
            let tool_elapsed = tool_start.elapsed();
            crate::session_metrics::record_activity(&self.session.id);

            if let Some(result) = tool_result {
                // Normal tool completion
                logging::debug_lazy(|| {
                    format!(
                        "Tool finished: {} in {:.2}s",
                        tc.name,
                        tool_elapsed.as_secs_f64()
                    )
                });
                if inline_output_tap {
                    // Update the tool marker in place with duration/error.
                    self.inline_tail
                        .finish_tool(tool_elapsed.as_secs_f64(), result.is_err());
                    self.publish_inline_tail();
                }

                match result {
                    Ok(output) => {
                        let output = cap_tool_output_for_history(&tc.name, output);
                        let codemode =
                            crate::tool::codemode_ledger::output_summary(&tc.name, &output);
                        let mut side_pane_images =
                            tool_output_side_pane_images(&tc.id, &tc.name, &tc.input, &output);
                        let mut blocks = tool_output_to_content_blocks(tc.id.clone(), output);
                        if self.provider.name().eq_ignore_ascii_case("openai")
                            && self.native_deferred_mcp()
                        {
                            crate::provider::openai_request::freeze_deferred_tools(
                                &mut blocks,
                                &tools,
                            );
                        }
                        let message_id = self.add_tool_result(
                            blocks,
                            tool_elapsed.as_millis() as u64,
                            codemode.clone(),
                        );
                        label_session_images(&self.session, &message_id, 0, &mut side_pane_images);
                        if !side_pane_images.is_empty() {
                            // RPC readers may load from disk while this turn holds the agent lock.
                            self.persist_session_now()?;
                        }
                        let output_text = self
                            .session
                            .messages
                            .last()
                            .and_then(|message| {
                                message.content.iter().find_map(|block| match block {
                                    ContentBlock::ToolResult {
                                        tool_use_id,
                                        content,
                                        ..
                                    } if tool_use_id == &tc.id => Some(content.clone()),
                                    _ => None,
                                })
                            })
                            .unwrap_or_default();
                        let _ = event_tx.send(ServerEvent::ToolDone {
                            codemode,
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            output: output_text,
                            error: None,
                        });

                        if !side_pane_images.is_empty() {
                            logging::info(&format!(
                                "SidePaneImages: emitting {} image(s) from tool '{}' (session={})",
                                side_pane_images.len(),
                                tc.name,
                                self.session.id
                            ));
                            let _ = event_tx.send(ServerEvent::SidePaneImages {
                                session_id: self.session.id.clone(),
                                images: side_pane_images,
                            });
                        }
                        tool_results_dirty = true;
                    }
                    Err(e) => {
                        let codemode = crate::tool::codemode_ledger::error_summary(&e);
                        let error_msg = format!("Error: {}", e);
                        let _ = event_tx.send(ServerEvent::ToolDone {
                            codemode: codemode.clone(),
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            output: error_msg.clone(),
                            error: Some(error_msg.clone()),
                        });

                        self.add_tool_result(
                            vec![ContentBlock::ToolResult {
                                tool_use_id: tc.id.clone(),
                                content: error_msg,
                                is_error: Some(true),
                            }],
                            tool_elapsed.as_millis() as u64,
                            codemode,
                        );
                        tool_results_dirty = true;
                    }
                }
            } else if self.is_graceful_shutdown() {
                let is_reload = turn_cancel_guard.interruption_reason()
                    == Some(crate::turn_cancel_registry::TurnInterruptionReason::Reload);
                logging::info(&format!(
                    "Tool '{}' interrupted by {} after {:.1}s",
                    tc.name,
                    if is_reload {
                        "server reload"
                    } else {
                        "user cancellation"
                    },
                    tool_elapsed.as_secs_f64()
                ));
                tool_handle.abort();

                // Leave the real tool_use unanswered: recovery runs the same
                // wait before calling the model. A placeholder result would
                // acknowledge it and make it disappear from the resume plan.
                if is_reload && tool_handoff::is_reload_replayable_wait(&tc.name, &tc.input) {
                    self.session.save()?;
                    return Ok(ToolRoundEnd::TurnOver);
                }

                let (interrupted_msg, is_error) = if is_reload {
                    reload_interrupted_tool_result(tc, tool_elapsed.as_secs_f64())
                } else {
                    (
                        format!(
                            "[Tool '{}' cancelled by the user after {:.1}s]",
                            tc.name,
                            tool_elapsed.as_secs_f64()
                        ),
                        true,
                    )
                };

                // An ask interrupted by reload is still pending, not a
                // completed decision. Keep its durable marker for recovery
                // but do not publish a false ToolDone to live clients.
                if !(is_reload && tc.name == "ask") {
                    let _ = event_tx.send(ServerEvent::ToolDone {
                        codemode: None,
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        output: interrupted_msg.clone(),
                        error: if is_error {
                            Some(
                                if is_reload {
                                    "interrupted by reload"
                                } else {
                                    "cancelled by the user"
                                }
                                .to_string(),
                            )
                        } else {
                            None
                        },
                    });
                }

                self.add_message_with_duration(
                    Role::User,
                    vec![ContentBlock::ToolResult {
                        tool_use_id: tc.id.clone(),
                        content: interrupted_msg,
                        is_error: Some(is_error),
                    }],
                    Some(tool_elapsed.as_millis() as u64),
                );
                self.session.save()?;

                // Add results for any remaining tools too
                for remaining_tc in &tool_calls[(tool_index + 1)..] {
                    if is_reload
                        && tool_handoff::is_reload_replayable_wait(
                            &remaining_tc.name,
                            &remaining_tc.input,
                        )
                    {
                        continue;
                    }
                    self.add_message(
                        Role::User,
                        vec![ContentBlock::ToolResult {
                            tool_use_id: remaining_tc.id.clone(),
                            content: if is_reload {
                                "[Skipped - server reloading]"
                            } else {
                                "[Skipped - cancelled by the user]"
                            }
                            .to_string(),
                            is_error: Some(true),
                        }],
                    );
                }
                self.session.save()?;
                return Ok(ToolRoundEnd::TurnOver);
            } else {
                // Foreground budget expired or user pressed Alt+B.
                logging::info(&format!(
                    "Tool '{}' moved to background after {:.1}s",
                    tc.name,
                    tool_elapsed.as_secs_f64()
                ));

                let bg_info = crate::background::global()
                    .adopt_with_options(
                        &tc.name,
                        tool_handoff::handoff_display_name(&tc.input),
                        &self.session.id,
                        true,
                        false,
                        tool_handle,
                    )
                    .await;
                abort_tool_on_drop.0 = None;

                let reason = if automatic_background {
                    tool_handoff::automatic_handoff_reason(foreground_budget)
                } else {
                    "by the user".to_string()
                };
                let bg_msg = format!(
                    "Tool '{}' was moved to background {} (task_id: {}).\n\n{}",
                    tc.name,
                    reason,
                    bg_info.task_id,
                    crate::tool::BACKGROUND_WAIT_GUIDANCE
                );

                let _ = event_tx.send(ServerEvent::ToolDone {
                    codemode: None,
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    output: bg_msg.clone(),
                    error: None,
                });

                self.add_message_with_duration(
                    Role::User,
                    vec![ContentBlock::ToolResult {
                        tool_use_id: tc.id.clone(),
                        content: bg_msg,
                        is_error: None,
                    }],
                    Some(tool_elapsed.as_millis() as u64),
                );
                self.session.save()?;

                self.background_tool_signal.reset();
            }

            // NOTE: We do NOT inject between tools (non-urgent) because that would
            // place user text between tool_results, which may violate API constraints.
            // All non-urgent injection happens at Point D after all tools are done.
        }

        if tool_results_dirty {
            self.session.save()?;
        }

        if !generated_image_contexts.is_empty() {
            for blocks in generated_image_contexts.drain(..) {
                self.add_message(Role::User, blocks);
            }
            self.session.save()?;
        }

        // Bounded recovery for repeated malformed tool calls (e.g. a
        // model emitting null arguments): correct with the expected
        // schema, then end the turn with an actionable error instead of
        // retrying the same malformed round forever. Mirrors the blocking
        // loop (turn_loops.rs) via the shared helper.
        self.handle_malformed_tool_round(
            &malformed_calls,
            executed_valid_call,
            consecutive_malformed_tool_rounds,
            &tools,
        )?;

        // === INJECTION POINT D: All tools done, before next API call ===
        // This is the safest point for non-urgent injection since all tool_results
        // have been added and the conversation is in a valid state.
        if let PostToolInterruptOutcome::SoftInterrupt { injected, point } =
            self.take_post_tool_soft_interrupt()
        {
            for event in Self::build_soft_interrupt_events(injected, point, None) {
                let _ = event_tx.send(event);
            }
        }
        Ok(ToolRoundEnd::NextRequest)
    }
}
