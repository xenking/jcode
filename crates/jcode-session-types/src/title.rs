//! Fallback session titles derived from the first real user prompt.
//!
//! Sessions without a rename, bookmark label, or todo goal would otherwise
//! show up as an anonymous "New session" everywhere. The first prompt is the
//! most recognizable summary available without spending a model call.

const MAX_TITLE_CHARS: usize = 64;

/// Longest title the model may produce, in words. The contract asks for 3-6;
/// anything longer is clipped so one verbose answer cannot become a paragraph.
pub const TITLE_WORD_MAX: usize = 6;
/// Hard character ceiling for a model-produced title.
pub const TITLE_CHAR_MAX: usize = 64;
/// Characters of each transcript excerpt sent to the title model.
const TITLE_INPUT_CLIP_CHARS: usize = 600;

/// Model chain used when the user has not configured `[agents] title_model`.
///
/// Deliberately non-OpenAI: session naming must keep working while OpenAI is
/// paused or rate limited. Both entries are cheap and fast, and the second is
/// the default swarm model on this install.
pub const DEFAULT_TITLE_MODELS: &[&str] = &["claude-haiku-5-5", "opencode-go:deepseek-v4.1-flash"];

/// Prompt contract for model-generated session titles.
///
/// The transcript printed by a coordinator session is full of internal
/// scaffolding (model directives, repository paths, todo lane assignments,
/// system reminders). The contract tells the model to name the work, not the
/// scaffolding, and `sanitize_title` rejects answers that ignore it.
pub const TITLE_SYSTEM_PROMPT: &str = "\
Write the session title for this conversation.

Rules:
- 3-6 words, specific to this conversation's work.
- Use the same language as the user's messages, even when file names and code are English.
- Name the work (feature, bug, file, or topic), never the tooling.
- Never copy internal scaffolding: model directives, repository paths, system reminder text, todo lane labels, an \"Authoritative assigned todo lane\" block, or agent poke text.
- No quotes, no markdown, no trailing period, no explanation.
- Output only the title.";

/// Which moment a title is being generated for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleStage {
    /// Right after the first real user turn.
    FirstTurn,
    /// Once the topic is clear, at the end of the third real user turn.
    ThirdTurn,
}

/// Why a turn ends without spending a title-model call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleSkipReason {
    /// The user renamed the session or bookmarked it with a label.
    ManualTitle,
    /// Every user message so far is internal (reminders, pokes, commands).
    NoUsablePrompt,
    /// Only the first and third turns generate titles.
    NotATitleTurn,
    /// The stored title was chosen by a caller (review/judge sessions) and is
    /// not the heuristic first-prompt title, so it is not ours to replace.
    CallerTitle,
}

/// What to do with the session title after a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleAction {
    Generate(TitleStage),
    Skip(TitleSkipReason),
}

/// Inputs to the pure title decision.
pub struct TitleDecisionInput<'a> {
    /// Number of real (non-internal) user messages in the transcript.
    pub real_user_turns: usize,
    /// Title currently stored on the session.
    pub current_title: Option<&'a str>,
    /// Title the heuristic first-prompt fallback would have produced.
    pub heuristic_title: Option<&'a str>,
    /// User rename.
    pub custom_title: Option<&'a str>,
    /// Bookmark label, which doubles as a user-chosen name.
    pub save_label: Option<&'a str>,
    /// Whether this process generated the stored title (in-memory).
    pub auto_owned: bool,
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// Decide whether this turn should generate a session title.
///
/// Titles are generated after the first real user turn and refined once at the
/// third. Matching the turn count exactly means a resumed session cannot
/// regenerate on every turn, and a manual rename or bookmark label always wins.
pub fn plan_title_update(input: &TitleDecisionInput<'_>) -> TitleAction {
    if non_empty(input.custom_title).is_some() || non_empty(input.save_label).is_some() {
        return TitleAction::Skip(TitleSkipReason::ManualTitle);
    }
    let Some(heuristic) = non_empty(input.heuristic_title) else {
        return TitleAction::Skip(TitleSkipReason::NoUsablePrompt);
    };
    let owned = input.auto_owned
        || match non_empty(input.current_title) {
            None => true,
            Some(current) => current == heuristic,
        };
    if !owned {
        return TitleAction::Skip(TitleSkipReason::CallerTitle);
    }
    match input.real_user_turns {
        1 => TitleAction::Generate(TitleStage::FirstTurn),
        3 => TitleAction::Generate(TitleStage::ThirdTurn),
        _ => TitleAction::Skip(TitleSkipReason::NotATitleTurn),
    }
}

/// Build the model chain for session titles.
///
/// An explicit `[agents] title_model` leads; the cheap defaults follow as
/// fallbacks. Placeholder values ("inherit", "auto", "coordinator") are not
/// models and are ignored.
pub fn title_model_candidates(configured: Option<&str>) -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    let mut push = |model: &str| {
        let model = model.trim();
        if model.is_empty()
            || matches!(
                model.to_ascii_lowercase().as_str(),
                "inherit" | "auto" | "coordinator"
            )
            || models
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(model))
        {
            return;
        }
        models.push(model.to_string());
    };
    if let Some(configured) = configured {
        push(configured);
    }
    for model in DEFAULT_TITLE_MODELS {
        push(model);
    }
    models
}

/// Whether a stored title is the old first-prompt truncation or internal text
/// rather than something worth keeping.
///
/// Used by `jcode titles backfill` to pick work, and by anyone deciding whether
/// a session needs naming. A missing title always needs work.
pub fn needs_title_backfill(title: Option<&str>) -> bool {
    let Some(title) = non_empty(title) else {
        return true;
    };
    title.chars().count() > 40
        || title.ends_with('…')
        || is_poke_text(title)
        || [
            "<system-reminder>",
            "[auto]",
            "[Scheduled task]",
            "Use model ",
            "Authoritative assigned todo lane",
            "Your todo list is this lane",
            "Repo ~/",
            "(skill selfdev)",
        ]
        .iter()
        .any(|marker| title.contains(marker))
}

/// Synthetic first line used when a worker starts a lane without a prompt.
pub const AUTO_LANE_START: &str = "Begin your assigned todo lane.";

/// Strip internal wrappers, swarm lane boilerplate, and commands from a prompt
/// so neither the heuristic title nor the title model sees them.
///
/// Returns `None` when nothing user-authored remains.
pub fn clean_prompt_text(text: &str) -> Option<String> {
    let without_reminders = strip_blocks(text, "<system-reminder>", "</system-reminder>", false);
    let with_transcripts = strip_blocks(
        &without_reminders,
        "<transcription>",
        "</transcription>",
        true,
    );
    let trimmed = with_transcripts.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.starts_with("[Scheduled task]")
        || trimmed.starts_with("[auto]")
        || trimmed.starts_with("[automated follow-up")
        || trimmed.lines().next() == Some(AUTO_LANE_START)
        || is_poke_text(trimmed)
    {
        return None;
    }

    // A swarm worker's assignment embeds the coordinator's lane bookkeeping
    // after the actual task. Keep the task text, drop the lane block so lane
    // labels never become titles.
    let mut visible = trimmed;
    for marker in [
        "Authoritative assigned todo lane:",
        "Your todo list is this lane",
    ] {
        if let Some(index) = visible.find(marker) {
            visible = &visible[..index];
        }
    }
    // Coordinators routinely prepend "Use model <id>. " to worker tasks.
    if let Some(rest) = visible.trim_start().strip_prefix("Use model ")
        && let Some((model, after)) = rest.split_once('.')
        && !model.trim().is_empty()
        && !model.contains(' ')
    {
        visible = after;
    }

    let normalized = visible.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() || normalized.starts_with('/') {
        return None;
    }
    Some(truncate_chars(&normalized, TITLE_INPUT_CLIP_CHARS))
}

/// Agent poke/continuation prompts persisted as user messages. They describe
/// the mechanism, not the work, so they must never name a session.
fn is_poke_text(text: &str) -> bool {
    (text.starts_with("You have ") && text.contains(" incomplete todo"))
        || text.starts_with("All todos are done.")
        || text.starts_with("Your completion confidence")
        || text.contains("Todo confidence summary")
}

/// Sanitize one model answer into a displayable title.
///
/// Returns `None` when the answer is empty, a single vague word, a lane label,
/// a system reminder, or poke boilerplate. That keeps a bad model answer from
/// becoming the title instead of leaving the heuristic fallback in place.
pub fn sanitize_title(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|line| !line.is_empty())?;
    let mut text = line;
    for prefix in ["- ", "* ", "# "] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest.trim();
            break;
        }
    }
    let lower = text.to_ascii_lowercase();
    for prefix in ["session title:", "title:"] {
        if lower.starts_with(prefix) {
            text = text[prefix.len()..].trim();
            break;
        }
    }
    let text = text
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '«' | '»' | '“' | '”' | '‘' | '’'))
        .trim();
    let lower = text.to_lowercase();
    if lower.contains('<')
        || lower.contains('>')
        || lower.contains("system-reminder")
        || lower.contains("todo lane")
        || lower.contains("todo list is this lane")
    {
        return None;
    }
    let text = text
        .trim_end_matches(['.', ',', ':', ';', '!', '?', '…', ' '])
        .trim();
    if text.is_empty() {
        return None;
    }
    let words = text.split_whitespace().collect::<Vec<_>>();
    if words.len() == 1 {
        // One Latin word is too vague ("investigate"), but CJK titles are
        // legitimately written without spaces.
        return (contains_wide_chars(text) && text.chars().count() <= 24).then(|| text.to_string());
    }
    let mut clipped = words
        .iter()
        .take(TITLE_WORD_MAX)
        .copied()
        .collect::<Vec<_>>();
    // A clipped title must not end mid-phrase ("... hide models and").
    while clipped.len() > 1
        && clipped.last().is_some_and(|word| {
            TITLE_DANGLING_WORDS
                .iter()
                .any(|dangling| word.eq_ignore_ascii_case(dangling))
        })
    {
        clipped.pop();
    }
    let joined = clipped.join(" ");
    let joined = truncate_chars(joined.trim(), TITLE_CHAR_MAX);
    let lower = joined.to_lowercase();
    if lower.starts_with("authoritative assigned") || lower.starts_with("your todo list") {
        return None;
    }
    Some(joined)
}

/// Build the user message sent alongside [`TITLE_SYSTEM_PROMPT`].
pub fn build_title_user_message(
    first_prompt: &str,
    latest_prompt: Option<&str>,
    assistant_gist: Option<&str>,
) -> String {
    let mut message = String::from("First user message:\n");
    message.push_str(&clip_excerpt(first_prompt));
    if let Some(latest) = latest_prompt
        .map(str::trim)
        .filter(|latest| !latest.is_empty() && !latest.eq_ignore_ascii_case(first_prompt.trim()))
    {
        message.push_str("\n\nLatest user message:\n");
        message.push_str(&clip_excerpt(latest));
    }
    if let Some(gist) = assistant_gist
        .map(str::trim)
        .filter(|gist| !gist.is_empty())
    {
        message.push_str("\n\nWork so far:\n");
        message.push_str(&clip_excerpt(gist));
    }
    message
}

fn clip_excerpt(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&normalized, TITLE_INPUT_CLIP_CHARS)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        format!(
            "{}…",
            text.chars()
                .take(max_chars.saturating_sub(1))
                .collect::<String>()
        )
    }
}

fn contains_wide_chars(text: &str) -> bool {
    text.chars().any(|c| {
        // 0x2E80..=0x9FFF already covers the kana block, so the ranges stay
        // mutually exclusive: every arm is reachable and testable.
        matches!(c as u32,
            0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF)
    })
}

/// Function words that make a clipped title read unfinished.
const TITLE_DANGLING_WORDS: &[&str] = &[
    "a", "an", "and", "at", "but", "by", "for", "from", "in", "into", "is", "of", "on", "or",
    "the", "to", "was", "were", "with", "в", "во", "для", "и", "из", "к", "ко", "на", "о", "об",
    "от", "по", "с", "со", "что", "или",
];

/// Turn a user prompt into a compact one-line title.
///
/// Internal wrappers (`<system-reminder>` blocks, voice `<transcription>`
/// tags) are removed, whitespace is collapsed, and slash commands produce no
/// title because they describe an action rather than the conversation.
pub fn prompt_title(prompt: &str) -> Option<String> {
    let normalized = clean_prompt_text(prompt)?;
    let mut chars = normalized.chars();
    let title: String = chars.by_ref().take(MAX_TITLE_CHARS).collect();
    Some(if chars.next().is_some() {
        format!("{}…", title.trim_end())
    } else {
        title
    })
}

/// Remove complete `open`..`close` segments. With `keep_inner`, only the tags
/// are dropped. Unbalanced text is kept verbatim.
fn strip_blocks(text: &str, open: &str, close: &str, keep_inner: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        let after_open = &rest[start + open.len()..];
        let Some(end) = after_open.find(close) else {
            break;
        };
        out.push_str(&rest[..start]);
        if keep_inner {
            out.push(' ');
            out.push_str(&after_open[..end]);
            out.push(' ');
        }
        rest = &after_open[end + close.len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::{
        TITLE_SYSTEM_PROMPT, TitleAction, TitleDecisionInput, TitleSkipReason, TitleStage,
        build_title_user_message, clean_prompt_text, needs_title_backfill, plan_title_update,
        prompt_title, sanitize_title, title_model_candidates,
    };

    #[test]
    fn prompt_becomes_a_compact_single_line_title() {
        assert_eq!(
            prompt_title("  help me   improve the session sidebar\nplease ").as_deref(),
            Some("help me improve the session sidebar please")
        );
        let long = prompt_title(&"word ".repeat(40)).unwrap();
        assert!(long.ends_with('…'));
        assert!(long.chars().count() <= 65);
    }

    #[test]
    fn wrappers_and_commands_are_not_titles() {
        assert_eq!(
            prompt_title("<transcription>\nRename the sidebar rows.\n</transcription>").as_deref(),
            Some("Rename the sidebar rows.")
        );
        assert_eq!(
            prompt_title("<system-reminder>\n# Session Context\n</system-reminder>"),
            None
        );
        assert_eq!(
            prompt_title("<system-reminder>ctx</system-reminder>\nFix the build").as_deref(),
            Some("Fix the build")
        );
        assert_eq!(prompt_title(" /model gpt-5 "), None);
        assert_eq!(prompt_title("[Scheduled task]\nrun it"), None);
        assert_eq!(prompt_title("   "), None);
    }

    fn decision(turns: usize) -> TitleDecisionInput<'static> {
        TitleDecisionInput {
            real_user_turns: turns,
            current_title: None,
            heuristic_title: Some("fix the flaky title generation please"),
            custom_title: None,
            save_label: None,
            auto_owned: false,
        }
    }

    #[test]
    fn generates_after_the_first_real_user_turn() {
        assert_eq!(
            plan_title_update(&decision(1)),
            TitleAction::Generate(TitleStage::FirstTurn)
        );
        assert_eq!(
            plan_title_update(&decision(0)),
            TitleAction::Skip(TitleSkipReason::NotATitleTurn)
        );
    }

    #[test]
    fn refines_once_the_topic_is_clear_at_turn_three() {
        let mut input = decision(3);
        input.current_title = Some("Fix flaky session titles");
        input.auto_owned = true;
        assert_eq!(
            plan_title_update(&input),
            TitleAction::Generate(TitleStage::ThirdTurn)
        );

        input.auto_owned = false;
        assert_eq!(
            plan_title_update(&input),
            TitleAction::Skip(TitleSkipReason::CallerTitle)
        );
    }

    #[test]
    fn other_turns_do_not_spend_a_model_call() {
        for turns in [2usize, 4, 5, 40] {
            let mut input = decision(turns);
            input.current_title = Some("Fix flaky session titles");
            input.auto_owned = true;
            assert_eq!(
                plan_title_update(&input),
                TitleAction::Skip(TitleSkipReason::NotATitleTurn),
                "turn {turns} must not regenerate"
            );
        }
    }

    #[test]
    fn a_manual_rename_or_bookmark_label_is_never_overwritten() {
        let mut input = decision(1);
        input.custom_title = Some("My name");
        assert_eq!(
            plan_title_update(&input),
            TitleAction::Skip(TitleSkipReason::ManualTitle)
        );

        let mut input = decision(3);
        input.save_label = Some("Bookmark name");
        input.auto_owned = true;
        assert_eq!(
            plan_title_update(&input),
            TitleAction::Skip(TitleSkipReason::ManualTitle)
        );
    }

    #[test]
    fn sessions_without_a_real_prompt_are_left_alone() {
        let mut input = decision(1);
        input.heuristic_title = None;
        assert_eq!(
            plan_title_update(&input),
            TitleAction::Skip(TitleSkipReason::NoUsablePrompt)
        );
    }

    #[test]
    fn caller_chosen_titles_survive_but_heuristic_ones_are_replaced() {
        let mut input = decision(1);
        input.current_title = Some("Structured Review");
        assert_eq!(
            plan_title_update(&input),
            TitleAction::Skip(TitleSkipReason::CallerTitle)
        );

        let mut input = decision(1);
        input.current_title = input.heuristic_title;
        assert_eq!(
            plan_title_update(&input),
            TitleAction::Generate(TitleStage::FirstTurn)
        );
    }

    #[test]
    fn sanitizer_returns_a_short_clean_title() {
        assert_eq!(
            sanitize_title("  \"Fix session title generation.\"  ").as_deref(),
            Some("Fix session title generation")
        );
        assert_eq!(
            sanitize_title("Title: Исправление заголовков сессий").as_deref(),
            Some("Исправление заголовков сессий")
        );
        assert_eq!(
            sanitize_title("`Fix flaky session titles in jcode`").as_deref(),
            Some("Fix flaky session titles in jcode")
        );
    }

    #[test]
    fn sanitizer_truncates_to_six_words() {
        assert_eq!(
            sanitize_title("Fix the flaky session title generation bug now please").as_deref(),
            Some("Fix the flaky session title generation")
        );
        assert_eq!(
            sanitize_title("Desktop model picker: hide models and").as_deref(),
            Some("Desktop model picker: hide models")
        );
        assert_eq!(
            sanitize_title("Fix the flaky session titles and").as_deref(),
            Some("Fix the flaky session titles")
        );
    }

    #[test]
    fn sanitizer_rejects_lane_labels_pokes_and_prose() {
        assert_eq!(sanitize_title("session-titles"), None);
        assert_eq!(sanitize_title("investigate"), None);
        assert_eq!(sanitize_title("Authoritative assigned todo lane"), None);
        assert_eq!(
            sanitize_title("<system-reminder>keep going</system-reminder>"),
            None
        );
        assert_eq!(
            sanitize_title("Your todo list is this lane, in order."),
            None
        );
        assert_eq!(sanitize_title("   "), None);
        assert_eq!(sanitize_title("\"\"''"), None);
    }

    #[test]
    fn sanitizer_keeps_short_cjk_titles() {
        assert_eq!(
            sanitize_title("修复会话标题生成").as_deref(),
            Some("修复会话标题生成")
        );
    }

    #[test]
    fn prompt_contract_forbids_boilerplate_and_enforces_shape() {
        let prompt = TITLE_SYSTEM_PROMPT.to_lowercase();
        assert!(prompt.contains("3-6 words"), "{TITLE_SYSTEM_PROMPT}");
        assert!(prompt.contains("same language"), "{TITLE_SYSTEM_PROMPT}");
        assert!(prompt.contains("only the title"), "{TITLE_SYSTEM_PROMPT}");
        assert!(
            prompt.contains("system reminder"),
            "the contract must call out internal wrappers: {TITLE_SYSTEM_PROMPT}"
        );
        assert!(
            prompt.contains("todo lane"),
            "the contract must call out lane labels: {TITLE_SYSTEM_PROMPT}"
        );
        assert!(!prompt.contains("translate"), "{TITLE_SYSTEM_PROMPT}");
    }

    #[test]
    fn title_input_strips_swarm_lane_boilerplate_and_reminders() {
        let lane = "Fix the build\n\nAuthoritative assigned todo lane:\nYour todo list is this lane, in order. do work";
        let cleaned = clean_prompt_text(lane).unwrap();
        assert_eq!(cleaned, "Fix the build");
        let auto_lane = "Begin your assigned todo lane.\n\nYour todo list is this lane, in order.\n- [pending] Open side questions";
        assert_eq!(prompt_title(auto_lane), None);

        let reminder = "<system-reminder>\n# Session Context\n</system-reminder>";
        assert_eq!(clean_prompt_text(reminder), None);
        assert_eq!(clean_prompt_text("[Scheduled task]\nrun it"), None);
        assert_eq!(clean_prompt_text("/model gpt-5"), None);
        assert_eq!(
            clean_prompt_text("[auto] <system-reminder>x</system-reminder>"),
            None
        );
        assert_eq!(
            clean_prompt_text(
                "You have 3 incomplete todos. Continue working, or update the todo tool."
            ),
            None
        );
        assert_eq!(
            clean_prompt_text("All todos are done. Todo confidence summary: ..."),
            None
        );
        assert_eq!(
            clean_prompt_text("[automated follow-up - not a user message] recheck"),
            None
        );

        let message = build_title_user_message(
            "Почини генерацию заголовков",
            Some("Ещё учитывай ручные переименования"),
            Some("Разобрал текущую логику"),
        );
        assert!(message.contains("First user message:"));
        assert!(message.contains("Latest user message:"));
        assert!(message.contains("Work so far:"));
        assert!(!message.contains("<system-reminder>"));
    }

    #[test]
    fn default_title_models_avoid_openai_and_prefer_cheap_ones() {
        let models = title_model_candidates(None);
        assert_eq!(models.first().map(String::as_str), Some("claude-haiku-5-5"));
        assert!(
            models
                .iter()
                .any(|model| model == "opencode-go:deepseek-v4.1-flash"),
            "{models:?}"
        );
        assert!(
            models.iter().all(|model| !model.starts_with("openai")),
            "OpenAI is paused; default chain must not route there: {models:?}"
        );
    }

    #[test]
    fn configured_title_model_leads_and_placeholders_are_ignored() {
        let models = title_model_candidates(Some("opencode-go:deepseek-v4.1-flash"));
        assert_eq!(
            models.first().map(String::as_str),
            Some("opencode-go:deepseek-v4.1-flash")
        );
        assert_eq!(models.len(), 2, "duplicates must collapse: {models:?}");

        for placeholder in ["inherit", "auto", "coordinator", "  "] {
            let models = title_model_candidates(Some(placeholder));
            assert_eq!(
                models.first().map(String::as_str),
                Some("claude-haiku-5-5"),
                "{placeholder} must not become a model"
            );
        }
    }

    #[test]
    fn backfill_flags_truncated_prompts_and_internal_text() {
        assert!(needs_title_backfill(None));
        assert!(needs_title_backfill(Some(
            "Use model claude-opus-5-5. Repo ~/projects/github.com/xenking/j…"
        )));
        assert!(needs_title_backfill(Some(
            "I'm starting a new web service from scratch and I want you to pi…"
        )));
        assert!(needs_title_backfill(Some(
            "<system-reminder>ctx</system-reminder>"
        )));
        assert!(needs_title_backfill(Some(
            "Authoritative assigned todo lane: session-titles"
        )));
        assert!(!needs_title_backfill(Some("Fix session title generation")));
        assert!(!needs_title_backfill(Some("seed session for resume test")));
    }
}
