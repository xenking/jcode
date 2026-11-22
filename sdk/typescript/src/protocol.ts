/**
 * Wire types for the jcode harness API (protocol v1).
 *
 * Mirrors `crates/jcode-harness-api` exactly: request tags live under `req`,
 * event tags under `ev`, and every frame carries `v`. Keep this file in sync
 * with the Rust enums; `test/schema-parity.test.ts` fails the build if the
 * tag sets drift apart.
 */

export const API_VERSION_MAJOR = 1;
export const API_VERSION_MINOR = 22;

export type RuntimeCommand = "mode" | "skills" | "context" | "info" | "cache" | "memory" | "swarm" | "agents" | "subagent_model" | "autoreview" | "autojudge" | "transport" | "fast" | "telemetry" | "subscription" | "overnight" | "productivity" | "initiatives" | "premium" | "provider_test_coverage" | "refresh_model_list" | "continue" | "resume_all" | "remote" | "server_reload" | "review" | "judge" | "subagent" | "auth" | "logout" | "background_output";

export type PermissionDecision = "allow" | "allow_always" | "deny";

export type ErrorCode =
  | "unsupported_version"
  | "unknown_request"
  | "unknown_session"
  | "unknown_skill"
  | "invalid_request"
  | "internal";

/** Cumulative built-in file-tool changes, not net worktree diff. */
export interface SessionEditStats {
  added: number;
  removed: number;
  approximate: boolean;
}

export interface SessionCatalogContext {
  model?: string;
  reasoning_effort?: string;
  modes?: string[];
  /** Estimated prompt-cache expiry in Unix milliseconds, not a guaranteed hit. */
  cache_expires_at_ms?: number;
}

export interface SessionInfo {
  edit_stats?: SessionEditStats;
  context?: SessionCatalogContext;
  session_id: string;
  /** Swarm owner, never the transcript's ordinary fork parent. */
  parent_session_id?: string;
  /** Stable task/role label, separate from the canonical display title. */
  agent_label?: string;
  /** Last persisted swarm lifecycle status, not connection status. */
  swarm_status?: string;
  working_dir?: string;
  title?: string;
  status: string;
  /** Whether a user client is attached. Omitted by older servers, meaning false. */
  user_connected?: boolean;
  /** Approximate size of the stored transcript, in bytes. */
  transcript_bytes?: number;
  archived?: boolean;
  archived_at_ms?: number;
}

/** Live lifecycle fact from one daemon publication. */
export interface SwarmMemberInfo {
  session_id: string;
  status: string;
  parent_session_id?: string;
  agent_label?: string;
}

/** What a session's own record says: model, effort, spend and live phase. */
export interface WorkerMeta {
  model?: string;
  provider?: string;
  effort?: string;
  /** Provider-reported prompt tokens, summed over the session. */
  input_tokens?: number;
  output_tokens?: number;
  cache_read_tokens?: number;
  cache_creation_tokens?: number;
  phase?: "thinking" | "tool" | "waiting" | "idle";
  /** Tool the worker is running, when `phase` is "tool". */
  tool?: string;
  /** Unix epoch millis of the newest message. */
  last_activity_ms?: number;
}

/** Tracked turns with a persisted response, separate from historical picker selections. */
export interface ModelUsage {
  count: number;
  last_used_unix_secs?: number | null;
  tracking_started_unix_secs?: number | null;
  selection_count: number;
  last_selected_unix_secs?: number | null;
}

/** Best-first usage ordering. Apply search relevance first and stable identity last. */
export function compareModelUsage(a?: ModelUsage | null, b?: ModelUsage | null): number {
  if (!a || !b) return a ? -1 : b ? 1 : 0;
  return b.count - a.count
    || (b.last_used_unix_secs ?? -1) - (a.last_used_unix_secs ?? -1)
    || b.selection_count - a.selection_count
    || (b.last_selected_unix_secs ?? -1) - (a.last_selected_unix_secs ?? -1);
}

export interface ModelRouteInfo {
  model: string;
  provider: string;
  api_method: string;
  available: boolean;
  detail: string;
  usage?: ModelUsage;
}

export interface TextMatch {
  path: string;
  line: number;
  column: number;
  preview: string;
}

/** Which half of the corpus a global-search hit came from. */
export type SessionSearchRole = "title" | "user" | "assistant";

/** A highlighted span of `SessionSearchHit.snippet`, in char indices. */
export interface SessionSearchHighlight {
  start: number;
  end: number;
}

/** One matching session title or transcript message. */
export interface SessionSearchHit {
  session_id: string;
  title: string;
  role: SessionSearchRole;
  snippet: string;
  highlights: SessionSearchHighlight[];
  /** Unix epoch milliseconds: the message time, or the session's last activity. */
  timestamp_ms: number;
  message_id?: string;
}

/** A session's persisted todo state, as read by the follow-up policy.
 * Todos, plan, goals, and observations use the runtime's todo schema. */
export interface TodoSnapshot {
  todos: Array<Record<string, unknown>>;
  plan: Record<string, unknown>;
  goals: Array<Record<string, unknown>>;
  gate_observations: Array<Record<string, unknown>>;
  long_session_review_due: boolean;
  /** Session-scoped background task IDs still running. Absent on older servers. */
  running_background_tasks?: string[];
  /** A wake-enabled swarm await is pending; absent on older bridges. */
  pending_swarm_await?: boolean;
}

/** Persistent effects of a follow-up decision. */
export interface TodoEffects {
  clear_gate_observations?: boolean;
  mark_long_session_review_delivered?: boolean;
}

/** Durable raw provider counts summed over all assistant rounds in one user turn.
 * Missing metrics are unknown, not zero. Cache accounting differs by provider.
 * Restored duration is currently unavailable. */
export interface ResponseStats {
  duration_secs?: number;
  input_tokens?: number;
  output_tokens?: number;
  cache_read_tokens?: number;
  cache_creation_tokens?: number;
}

export interface HistoryFragment { index: number; start: number; total: number; text: string }

/** SHA-256 bytes of the row's role, a zero byte, and its trimmed UTF-8 text. */
export interface RewindAnchor {
  selected: readonly number[];
  before?: readonly number[] | null;
  after?: readonly number[] | null;
}

export interface HistoryMessage {
  /** Only on the final assistant row. Preview/old-server history may omit it. */
  response_stats?: ResponseStats;
  /** "user" | "assistant" | "tool" */
  role: string;
  content: string;
  attachments?: StoredAttachment[];
}

export type RenderedImageSource =
  | { kind: "user_input" }
  | { kind: "tool_result"; tool_name: string }
  | { kind: "other"; role: string };

export type RenderedImageAnchor =
  | { kind: "tool_call"; id: string }
  | { kind: "user_prompt"; ordinal: number };

export interface RenderedImage {
  media_type: string;
  data: string;
  label?: string;
  source: RenderedImageSource;
  anchor?: RenderedImageAnchor;
  /** Insert before this History.messages index (including hidden rows). Length means append. */
  history_message_index?: number;
}

/** Base64 image attachment: [mediaType, base64Data]. */
export type ImageAttachment = [string, string];

export type Attachment =
  | { type: "file"; name: string; mime: string; size: number; data: string; path?: never }
  | { type: "file"; name: string; mime: string; size: number; path: string; data?: never }
  | { type: "pasted_text"; text: string };

export type StoredAttachment =
  | { type: "file"; name: string; mime: string; size: number; path?: string }
  | { type: "pasted_text"; text: string };

export type MemoryState =
  | "idle" | "embedding"
  | { sidecar_checking: { count: number } }
  | { found_relevant: { count: number } }
  | { extracting: { reason: string } }
  | { maintaining: { phase: string } }
  | { tool_action: { action: string; detail: string } };

type MemoryStepStatus = "pending" | "running" | "done" | "error" | "skipped";
type MemoryStepResult = { summary: string; latency_ms: number };

export interface MemoryActivitySnapshot {
  state: MemoryState;
  state_age_ms: number;
  pipeline?: {
    search: MemoryStepStatus;
    search_result?: MemoryStepResult;
    verify: MemoryStepStatus;
    verify_result?: MemoryStepResult;
    verify_progress?: [number, number];
    inject: MemoryStepStatus;
    inject_result?: MemoryStepResult;
    maintain: MemoryStepStatus;
    maintain_result?: MemoryStepResult;
  };
}

/** Delivery point of a daemon-owned prompt. */
export type QueuedPromptMode = "steer" | "after_yield";
export type QueuedPromptRemovalOutcome = "removed" | "in_flight" | "already_delivered" | "not_found";
export type QueuedPromptEditOutcome = "edited" | "in_flight" | "already_delivered" | "not_found";
export interface QueuedPrompt {
  client_id: string;
  content: string;
  images?: ImageAttachment[];
  attachments?: Attachment[];
  mode: QueuedPromptMode;
  active_skill?: string;
}

/** A tool definition exposed to the model. */
export interface SessionToolDefinition {
  name: string;
  description: string;
  parameters: Record<string, unknown>;
}

/** Wire-level session tool policy. Callback functions never cross the wire. */
export interface ToolConfiguration {
  enabled?: string[] | null;
  disabled?: string[];
  custom?: SessionToolDefinition[];
}

export interface BackgroundTaskProgress {
  kind: "determinate" | "indeterminate";
  percent: number | null;
  message: string | null;
  current: number | null;
  total: number | null;
  unit: string | null;
  eta_seconds: number | null;
  updated_at: string;
  source: "reported" | "parsed_output" | "heuristic";
}

export interface BackgroundTaskSummary {
  task_id: string;
  label: string;
  status: "running" | "completed" | "superseded" | "failed" | "interrupted";
  started_at: string;
  duration_secs?: number;
  stalled: boolean;
  can_cancel: boolean;
  progress?: BackgroundTaskProgress;
  error?: string;
  exit_code?: number;
  cancelled?: boolean;
  last_output_line?: string;
}

export interface BackgroundTaskOutput {
  task_id: string;
  tail: string;
  truncated: boolean;
}

export type ApiRequest =
  | { req: "list_draft_models" }
  | { req: "watch_background_tasks"; session_id: string; task_id?: string }
  | { req: "cancel_background_task"; session_id: string; task_id: string }
  | { req: "hello"; min_version: number; max_version: number; client: string; supports_user_response?: boolean }
  | { req: "list_sessions"; include_archived?: boolean; limit?: number }
  | { req: "archive_session"; session_id: string }
  | { req: "restore_session"; session_id: string }
  | { req: "set_retention_policy"; archive_after_days?: number }
  | { req: "create_session"; working_dir?: string; system_prompt?: string }
  | { req: "attach_session"; session_id: string }
  | { req: "fork_session"; session_id: string }
  | { req: "transfer_session"; session_id: string }
  | { req: "detach_session"; session_id: string }
  | { req: "configure_tools"; session_id: string; tools: ToolConfiguration }
  | { req: "list_tools"; session_id: string }
  | { req: "tool_result"; session_id: string; call_id: string; output: string; error?: string }
  | {
      req: "send_message";
      session_id: string;
      content: string;
      images?: ImageAttachment[];
      attachments?: Attachment[];
      no_reply?: boolean;
      system_reminder?: string;
    }
  | { req: "send_skill_message"; session_id: string; content: string; active_skill: string; system_reminder?: string; images?: ImageAttachment[]; attachments?: Attachment[] }
  | { req: "cancel"; session_id: string }
  | { req: "force_stop_turn"; session_id: string }
  | {
      req: "soft_interrupt";
      session_id: string;
      content: string;
      images?: ImageAttachment[];
      urgent?: boolean;
      /** Opaque id echoed in `soft_interrupt_injected` and accepted by a filtered cancel. */
      client_id?: string;
    }
  | { req: "queue_prompt"; session_id: string; client_id: string; content: string; images?: ImageAttachment[]; attachments?: Attachment[]; mode: QueuedPromptMode; system_reminder?: string; active_skill?: string }
  | { req: "edit_queued_prompt"; session_id: string; client_id: string; prompt_id: string; content: string; images?: ImageAttachment[]; attachments?: Attachment[] }
  | { req: "set_queued_prompt_editing"; session_id: string; client_id: string; prompt_id: string; editing: boolean }
  | { req: "set_queued_prompt_mode"; session_id: string; client_id: string; mode: QueuedPromptMode }
  | { req: "interrupt_queued_prompt"; session_id: string; client_id: string }
  | { req: "interrupt_queued_prompts"; session_id: string; ids: string[] }
  | { req: "get_queued_prompts"; session_id: string }
  | { req: "remove_queued_prompt"; session_id: string; client_id: string; prompt_id: string }
  | { req: "reorder_queued_prompts"; session_id: string; client_id: string; order: string[] }
  | { req: "get_history"; session_id: string }
  | { req: "get_compacted_history"; session_id: string; visible_messages: number }
  | { req: "get_history_page"; session_id: string; before: string | null; limit: number }
  | { req: "get_history_image_chunk"; session_id: string; key: string; offset: number }
  | { req: "list_session_images"; session_id: string }
  | { req: "get_session_image"; session_id: string; image_id: string; offset: number }
  | { req: "session_context_breakdown"; session_id?: string | null; model?: string | null; cwd?: string | null; summary_only?: boolean; include_snapcompact?: boolean }
  | { req: "session_shake_tools"; session_id: string; action: ShakeToolsAction }
  | { req: "peek_session"; session_id: string; limit?: number }
  | { req: "clear"; session_id: string }
  | { req: "rewind"; session_id: string; message_index: number }
  | { req: "rewind_transcript"; session_id: string; target: RewindAnchor }
  | {
      req: "ask_response";
      session_id: string;
      request_id: string;
      /** Omit answers to decline. */
      response: AskResponse;
    }
  | {
      req: "permission_response";
      session_id: string;
      request_id: string;
      decision: PermissionDecision;
    }
  | { req: "list_models"; session_id: string }
  | { req: "get_runtime_info"; session_id: string }
  | { req: "runtime_command"; session_id: string; command: RuntimeCommand; args?: string[] }
  | { req: "set_api_key"; provider: string; api_key: string }
  | { req: "notify_auth_changed"; provider: string }
  | { req: "invalidate_usage"; provider: string; account_label?: string }
  | { req: "clear_api_key"; provider: string }
  | { req: "read_file"; session_id: string; path: string; max_bytes?: number }
  | { req: "find_files"; session_id: string; query: string; limit?: number }
  | { req: "search_text"; session_id: string; query: string; path?: string; limit?: number }
  | { req: "file_status"; session_id: string; path: string }
  | { req: "get_todo_state"; session_id: string }
  | { req: "ack_todo_follow_up"; session_id: string; effects: TodoEffects }
  | { req: "set_model"; session_id: string; model: string }
  | { req: "set_context_cap"; session_id: string; max_context_tokens: number | null }
  | { req: "set_reasoning_effort"; session_id: string; effort: string }
  | { req: "compact"; session_id: string }
  | { req: "rename_session"; session_id: string; title?: string }
  | { req: "set_session_env"; session_id: string; env: Record<string, string> }
  | { req: "set_session_saved"; session_id: string; saved: boolean; label?: string }
  | {
      req: "applet_action";
      session_id: string;
      instance: string;
      action: AppletAction;
      state?: Record<string, unknown>;
      source_key?: string;
    }
  | { req: "close_applet"; session_id: string; instance: string }
  | { req: "rewind_undo"; session_id: string }
  /** Without `client_ids`, clears the queue (replies `ok`). With them, replies `soft_interrupts_cancelled`. */
  | { req: "cancel_soft_interrupts"; session_id: string; client_ids?: string[] }
  /** Replies `background_tool_result` when the `background_tool_result` capability is advertised, else `ok`. */
  | { req: "background_tool"; session_id: string }
  /** Cross-session search over titles and transcripts. Replies `session_search`. */
  | { req: "search_sessions"; query: string; limit?: number }
  | { req: "ping" };

/** Markdown/PDF panel state, shared with the native runtime. */
export type SidePanelPageFormat = "markdown" | "pdf";
export type SidePanelPageSource = "managed" | "linked_file" | "ephemeral";
export interface SidePanelPage {
  id: string;
  title: string;
  file_path: string;
  format: SidePanelPageFormat;
  source: SidePanelPageSource;
  content: string;
  /** Base64 PDF bytes, separate from the human-readable Markdown fallback. */
  pdf_data?: string;
  updated_at_ms: number;
}
/** A user intent from an applet node (`jcode.applet/1`). */
export interface AppletAction {
  action: string;
  args?: unknown;
}

/** One mounted applet instance. `document` follows the `jcode.applet/1` schema. */
export interface AppletInstance {
  id: string;
  applet: string;
  placement: { kind: string; [key: string]: unknown };
  scope?: { kind: string; [key: string]: unknown };
  lifetime?: "ephemeral" | "session" | "persistent";
  document: {
    revision: number;
    title: string;
    view: { type: string; [key: string]: unknown };
    state?: Record<string, unknown>;
    assets?: unknown[];
  };
}

/** All agent-mounted applet instances for one session, in mount order. */
export interface AgentApplets {
  instances: AppletInstance[];
}

export interface SidePanelSnapshot {
  /** Monotonic explicit-focus intent. Absent on older servers. */
  focus_revision?: number;
  focused_page_id: string | null;
  /** Omitted by the runtime when empty. */
  pages?: SidePanelPage[];
}

export interface ContextMeter {
  state: "live" | "saved" | "draft" | "unavailable";
  occupied_tokens: number | null;
  source: "provider_reported" | "historical_report" | "compaction_estimate" | "local_estimate" | "unknown";
  measured_at_ms: number | null;
  route: string | null;
  model_window_tokens: number | null;
  effective_budget_tokens: number | null;
  session_cap_tokens: number | null;
  compaction_mode: string | null;
  compaction_threshold_tokens: number | null;
  idle_precompaction_tokens: number | null;
  /** Published threshold above which one request's input costs more. Not an occupancy cap. */
  long_context_pricing_above_input_tokens?: number;
}

export type ContextBreakdownKind =
  | "system_prompt"
  | "builtin_tools"
  | "skills_modes"
  | "dynamic_tools"
  | "agents"
  | "conversation"
  /** The snapcompact checkpoint projected for the serving route, not the stored archive. */
  | "snapcompact"
  | "unattributed";

export interface ContextBreakdownCategory {
  kind: ContextBreakdownKind;
  /** null means unavailable, not empty. */
  tokens: number | null;
}

export type ContextActionSavings =
  | { kind: "no_change" }
  | { kind: "estimate"; tokens: number }
  | { kind: "upper_bound"; tokens: number };

export type ContextActionStatus =
  | "no_eligible"
  | "unsupported"
  | "busy"
  | "savings_unknown"
  | "unknown";

export interface ContextActionForecast {
  current_tokens: number | null;
  compact: ContextActionSavings | null;
  shake: ContextActionSavings | null;
  compact_status?: ContextActionStatus;
  shake_status?: ContextActionStatus;
}

export interface ContextActionMeasurement {
  before_tokens: number;
  after_tokens: number;
}

export type ShakeToolsAction =
  | { kind: "preview" }
  | { kind: "apply"; confirmation: string };

export interface ShakeToolsStats {
  outputs_elided: number;
  estimated_tokens_saved: number;
}

export type ShakeToolsResult =
  | { kind: "no_change" }
  | { kind: "preview"; confirmation: string; cache_at_risk: boolean; stats: ShakeToolsStats }
  | { kind: "applied"; stats: ShakeToolsStats; measurement?: ContextActionMeasurement };

export type ApiEvent =
  | { ev: "draft_models"; current: string | null; routes: ModelRouteInfo[] }
  | { ev: "hello_ok"; version: number; server: string; capabilities?: string[]; runtime_identity?: { pid: number; binary_path: string; git_hash: string; version_label?: string; reload_id?: string; generation: number } }
  | { ev: "ok" }
  | { ev: "runtime_command_result"; session_id: string; command: RuntimeCommand; data: unknown; message: string }
  | { ev: "error"; code: ErrorCode; message: string }
  | { ev: "sessions"; sessions: SessionInfo[]; workers?: Record<string, WorkerMeta> }
  | { ev: "swarm_status"; session_id: string; updated_at_unix_ms: number; members: SwarmMemberInfo[] }
  | { ev: "attached"; session: SessionInfo }
  | { ev: "session_forked"; session: SessionInfo }
  | { ev: "history"; session_id: string; messages: HistoryMessage[]; images?: RenderedImage[] }
  | { ev: "memory_injected"; session_id: string; count: number; content: string }
  | { ev: "memory_activity"; session_id: string; activity: MemoryActivitySnapshot }
  | { ev: "compacted_history"; session_id: string; messages: HistoryMessage[]; images?: RenderedImage[]; compacted_total: number; compacted_visible: number; compacted_remaining: number; compacted_hidden_prompts?: number }
  | { ev: "history_page"; session_id: string; fragments: HistoryFragment[]; before: string | null }
  | { ev: "history_image_chunk"; session_id: string; key: string; offset: number; total: number; data: string }
  | { ev: "pong" }
  | { ev: "text_delta"; session_id: string; text: string; message_id?: string }
  | { ev: "text_done"; session_id: string; message_id?: string }
  | { ev: "text_replace"; session_id: string; message_id?: string; text: string }
  | { ev: "reasoning_delta"; session_id: string; text: string }
  | { ev: "reasoning_done"; session_id: string; duration_secs?: number }
  | { ev: "tool_start"; session_id: string; call_id: string; name: string }
  | { ev: "tool_input_delta"; session_id: string; call_id: string; delta: string }
  | { ev: "tool_exec"; session_id: string; call_id: string; name: string }
  | { ev: "queued_prompts"; session_id: string; prompts: QueuedPrompt[] }
  | { ev: "queued_prompt_removed"; session_id: string; prompt_id: string; outcome: QueuedPromptRemovalOutcome }
  | { ev: "queued_prompt_edited"; session_id: string; prompt_id: string; outcome: QueuedPromptEditOutcome }
  | { ev: "session_images"; session_id: string; images: unknown[] }
  | { ev: "session_image"; session_id: string; image_id: string; media_type: string; offset: number; total: number; data: string }
  | {
      ev: "context_breakdown";
      session_id: string | null;
      model: string | null;
      total_tokens: number | null;
      limit_tokens: number | null;
      estimated: boolean;
      categories: ContextBreakdownCategory[];
      action_forecast?: ContextActionForecast;
      meter?: ContextMeter;
      /** Occupancy minus estimated categories. A signed discrepancy, not a category. */
      residual_tokens?: number;
    }
  | { ev: "shake_tools"; session_id: string; result: ShakeToolsResult }
  | { ev: "user_image_ids"; session_id: string; original_content: string; content: string; image_ids: string[] }
  | { ev: "usage_limit"; session_id: string; provider: string; model: string; account: string | null; window: string; reset: string; action: string; target: string | null; message: string }
  | { ev: "tools"; session_id: string; tools: SessionToolDefinition[] }
  | { ev: "tool_call"; session_id: string; call_id: string; name: string; input: unknown }
  | {
      ev: "tool_done";
      session_id: string;
      call_id: string;
      name: string;
      output: string;
      error?: string;
    }
  | { ev: "side_panel_state"; session_id: string; snapshot: SidePanelSnapshot }
  | { ev: "applet_state"; session_id: string; snapshot: AgentApplets }
  | { ev: "side_pane_images"; session_id: string; images: RenderedImage[] }
  | {
      ev: "token_usage";
      session_id: string;
      input: number;
      output: number;
      cache_read_input?: number;
      cache_creation_input?: number;
      effective_budget?: number;
    }
  | {
      /** Final counters for one completed provider request. Not replayed with history. */
      ev: "request_usage";
      session_id: string;
      request_id: string;
      provider: string;
      model: string;
      input: number | null;
      output: number | null;
      cache_read_input?: number;
      cache_creation_input?: number;
    }
  | { ev: "context_cap_changed"; session_id: string; max_context_tokens: number | null; effective_budget: number }
  /** Persisted prompt occupancy replayed on attach, so a context meter is filled before the next turn reports usage. Not a turn's usage: no cost or turn stats. */
  | { ev: "context_occupancy"; session_id: string; prompt_tokens: number }
  | {
      ev: "kv_cache_miss";
      session_id: string;
      reason: string;
      harness_caused: boolean;
      missed_tokens: number;
      expected_tokens: number;
      read_tokens: number;
      documented_cause?: string;
      message: string;
    }
  | { ev: "turn_stopped"; session_id: string; reason: TurnStopReason; message: string; provider_stop_reason?: string }
  | { ev: "provider_guardrail_retry"; session_id: string; turn_id: string; attempt: number; max_attempts: number }
  | { ev: "peer_user_message"; session_id: string; content: string }
  | { ev: "agent_message"; session_id: string; from_session: string; from_name: string | null; message: string }
  /** `pending_soft_interrupts`: queued messages this turn never saw (only when nonzero). */
  | { ev: "turn_done"; session_id: string; pending_soft_interrupts?: number }
  /**
   * Queued soft interrupts reached the model. Output after this event was written
   * having seen them. Several queued messages arrive as one user message, so
   * `client_ids` lists every id delivered, in queue order. `point` is B, C, D, or
   * `turn_start` (sent while idle, so it started a turn).
   */
  | {
      ev: "soft_interrupt_injected";
      session_id: string;
      content?: string;
      client_ids: string[];
      point: string;
      tools_skipped?: number;
      display_role?: string;
    }
  /** Reply to a filtered `cancel_soft_interrupts`. Decided atomically against delivery. */
  | { ev: "soft_interrupts_cancelled"; cancelled: string[]; not_queued: string[] }
  /** Reply to `background_tool`. */
  | { ev: "background_tool_result"; moved: boolean; tool_call_id?: string; tool_name?: string }
  | { ev: "focus_requested"; session_id: string; from_session?: string; from_name?: string }
  | {
      ev: "wake_requested";
      session_id: string;
      reason: string;
      notification: string;
    }
  | {
      ev: "background_progress";
      session_id: string;
      task_id: string;
      label: string;
      percent?: number;
      summary: string;
      detail?: string;
      done?: boolean;
    }
  | { ev: "background_tasks"; session_id: string; snapshot: { tasks: BackgroundTaskSummary[]; output?: BackgroundTaskOutput } }
  | { ev: "message_accepted"; session_id: string }
  | {
      ev: "ask_request";
      session_id: string;
      request_id: string;
      tool_call_id: string;
      questions: AskQuestion[];
    }
  | {
      ev: "permission_request";
      session_id: string;
      request_id: string;
      tool_name: string;
      description: string;
    }
  /** Attachment recovery intent. Can precede attached. Never auto-sent by the bridge. */
  | { ev: "session_recovery"; session_id: string; continuation_message: string; reconnect_notice?: string }
  | { ev: "session_status"; session_id: string; status: string; pending_soft_interrupts?: number }
  | { ev: "connection_phase"; session_id: string; phase: string }
  | {
      ev: "model_info";
      session_id: string;
      provider?: string;
      model?: string;
      reasoning_effort?: string;
      auth_method?: string;
      /** OAuth account the daemon's account router selected. */
      account_label?: string;
    }
  | { ev: "models"; session_id: string; models: string[]; current?: string }
  | {
      ev: "runtime_info";
      session_id: string;
      provider?: string;
      model?: string;
      reasoning_effort?: string;
      auth_method?: string;
      /** OAuth account the daemon's account router selected. */
      account_label?: string;
      routes: ModelRouteInfo[];
    }
  | { ev: "credential_updated"; provider: string; configured: boolean }
  | {
      ev: "file_content";
      session_id: string;
      path: string;
      content: string;
      size: number;
      truncated: boolean;
    }
  | { ev: "files"; session_id: string; paths: string[] }
  | { ev: "text_matches"; session_id: string; matches: TextMatch[] }
  | {
      ev: "file_status";
      session_id: string;
      path: string;
      exists: boolean;
      kind: string;
      size?: number;
      modified_ms?: number;
    }
  | { ev: "todo_state"; session_id: string; state: TodoSnapshot }
  | {
      ev: "session_search";
      query: string;
      titles: SessionSearchHit[];
      messages: SessionSearchHit[];
      indexed_sessions: number;
      truncated?: boolean;
    }
  | { ev: "compacted"; session_id: string; message: string }
  | {
      ev: "compaction_divider";
      session_id: string;
      /** Divider body: the label, then the expandable detail after a blank line. */
      content: string;
      /** The compaction is still summarizing: show the running divider. */
      running?: boolean;
      /** summary | snapcompact | native | drop */
      method?: string;
      /** Trigger label, e.g. reactive or idle_precompact. */
      trigger?: string;
      pre_tokens?: number;
      post_tokens?: number;
    }
  | {
      ev: "session_renamed";
      session_id: string;
      title?: string;
      display_title: string;
    };

/**
 * An event kind this SDK does not know about.
 *
 * The harness may add events at any time within protocol v1, so one can arrive
 * at runtime. It is deliberately *not* part of `ApiEvent`: a member with
 * `ev: string` widens the discriminant, and TypeScript then refuses to narrow
 * `event.ev === "text_delta"` to the text-delta member, leaving every field
 * typed `unknown`. Forward compatibility is a runtime property, and paying for
 * it with the type safety of the ninety-nine percent case is a bad trade.
 *
 * Handle these with a `default` branch, or filter with `isKnownEvent`.
 */
export interface UnknownApiEvent {
  ev: string;
  [key: string]: unknown;
}

/** Any frame off the wire, known or not. Narrow with `isKnownEvent`. */
export type AnyApiEvent = ApiEvent | UnknownApiEvent;

export type ApiEventKind = ApiEvent["ev"];

export interface ClientFrame {
  v: number;
  id: number;
  [key: string]: unknown;
}

export type ServerFrame = { v: number; reply_to?: number } & UnknownApiEvent;

/** Every event tag the SDK knows about, for drift checks and routing. */
export const KNOWN_EVENT_KINDS = [
  "runtime_command_result",
  "draft_models",
  "hello_ok",
  "ok",
  "error",
  "sessions",
  "swarm_status",
  "attached",
  "session_forked",
  "history",
  "compacted_history",
  "history_page",
  "history_image_chunk",
  "queued_prompts",
  "queued_prompt_removed",
  "queued_prompt_edited",
  "session_images",
  "session_image",
  "context_breakdown",
  "shake_tools",
  "user_image_ids",
  "usage_limit",
  "side_pane_images",
  "side_panel_state",
  "applet_state",
  "pong",
  "text_delta",
  "text_done",
  "text_replace",
  "reasoning_delta",
  "reasoning_done",
  "tool_start",
  "tools",
  "tool_call",
  "tool_input_delta",
  "tool_exec",
  "tool_done",
  "token_usage",
  "request_usage",
  "context_cap_changed",
  "context_occupancy",
  "kv_cache_miss",
  "memory_injected",
  "memory_activity",
  "peer_user_message",
  "agent_message",
  "turn_done",
  "provider_guardrail_retry",
  "soft_interrupt_injected",
  "soft_interrupts_cancelled",
  "background_tool_result",
  "turn_stopped",
  "wake_requested",
  "focus_requested",
  "background_progress",
  "background_tasks",
  "message_accepted",
  "ask_request",
  "permission_request",
  "session_status",
  "session_recovery",
  "connection_phase",
  "model_info",
  "models",
  "runtime_info",
  "credential_updated",
  "file_content",
  "files",
  "text_matches",
  "file_status",
  "todo_state",
  "session_search",
  "compacted",
  "compaction_divider",
  "session_renamed",
] as const;

/** Every request tag the SDK can send. */
export const KNOWN_REQUEST_KINDS = [
  "runtime_command",
  "hello",
  "list_draft_models",
  "watch_background_tasks",
  "cancel_background_task",
  "list_sessions",
  "archive_session",
  "restore_session",
  "set_retention_policy",
  "create_session",
  "attach_session",
  "fork_session",
  "transfer_session",
  "detach_session",
  "configure_tools",
  "list_tools",
  "tool_result",
  "send_message",
  "send_skill_message",
  "cancel",
  "force_stop_turn",
  "soft_interrupt",
  "queue_prompt",
  "edit_queued_prompt",
  "set_queued_prompt_editing",
  "set_queued_prompt_mode",
  "interrupt_queued_prompt",
  "interrupt_queued_prompts",
  "get_queued_prompts",
  "remove_queued_prompt",
  "reorder_queued_prompts",
  "list_session_images",
  "get_session_image",
  "session_context_breakdown",
  "session_shake_tools",
  "get_history",
  "get_compacted_history",
  "get_history_page",
  "get_history_image_chunk",
  "peek_session",
  "clear",
  "rewind",
  "rewind_transcript",
  "ask_response",
  "permission_response",
  "list_models",
  "get_runtime_info",
  "set_api_key",
  "clear_api_key",
  "notify_auth_changed",
  "invalidate_usage",
  "read_file",
  "find_files",
  "search_text",
  "file_status",
  "get_todo_state",
  "ack_todo_follow_up",
  "set_model",
  "set_context_cap",
  "set_reasoning_effort",
  "compact",
  "rename_session",
  "set_session_env",
  "set_session_saved",
  "applet_action",
  "close_applet",
  "rewind_undo",
  "cancel_soft_interrupts",
  "background_tool",
  "search_sessions",
  "ping",
] as const;

export function isKnownEvent(frame: AnyApiEvent): frame is ApiEvent {
  return (KNOWN_EVENT_KINDS as readonly string[]).includes(frame.ev);
}

/** Natural completion has no stop reason. Transport loss is not proof of a crash. */
export type TurnStopReason = "interrupted" | "failure" | "crash" | "provider_guardrail" | "limit_reached" | "unknown";

/** `ask` tool question. */
export interface AskQuestion {
  question: string;
  header?: string;
  options: { label: string; description?: string }[];
  multi_select?: boolean;
  allow_other?: boolean;
}

export interface AskAnswer {
  selected: string[];
  other?: string;
  comments?: Record<string, string>;
  /** Images as [media type, base64], matching QueuePrompt. */
  other_images?: [string, string][];
  comment_images?: Record<string, [string, string][]>;
}

/** `answers` absent means the user declined. */
export interface AskResponse {
  answers?: AskAnswer[];
}
