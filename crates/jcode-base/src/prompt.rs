//! System prompt management

use std::path::{Path, PathBuf};
use std::process::Command;

/// Default system prompt for jcode (embedded at compile time)
pub const DEFAULT_SYSTEM_PROMPT: &str = include_str!("prompt/system_prompt.md");

/// Load the base system prompt, allowing the user to fully replace the built-in
/// [`DEFAULT_SYSTEM_PROMPT`]. Precedence: project `./.jcode/system-prompt.md`,
/// then global `~/.jcode/system-prompt.md`, then the built-in default.
///
/// This is a *replacement* hook. To merely add guidance on top of the default,
/// use `.jcode/prompt-overlay.md` instead.
pub fn load_base_system_prompt(working_dir: Option<&Path>) -> String {
    load_base_system_prompt_override(working_dir).unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.into())
}

fn load_base_system_prompt_override(working_dir: Option<&Path>) -> Option<String> {
    let project_dir = working_dir.unwrap_or(Path::new("."));
    let candidates = [
        Some(project_dir.join(".jcode").join("system-prompt.md")),
        crate::storage::jcode_dir()
            .ok()
            .map(|dir| dir.join("system-prompt.md")),
    ];
    for path in candidates.into_iter().flatten() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Prompt guidance for the optional Mermaid rendering capability.
pub const MERMAID_PROMPT: &str = "# Mermaid\n\nRender fenced `mermaid` blocks inline.";

/// Prompt guidance for the Codemode capability.
///
/// This is the only place the model is told *when* one script beats many
/// single tool calls. Without it the model only ever sees the tool's own
/// description, and measured history shows it then reaches for `bash` and
/// `batch` instead (codemode was 0.14% of 7,884 tool calls).
pub const CODEMODE_PROMPT: &str = "# Tool calls

`codemode` runs one JavaScript or TypeScript program that calls tools itself, MCP tools included (`listTools('mcp__')`, `types(name)`). Use it when:
- the task needs 5 or more tool calls,
- each call needs the previous call's result,
- you would loop over a list another tool returned,
- a tool returns far more output than you need.

Only the result, logs, and call receipts reach the conversation. Use `Promise.allSettled` for independent calls when partial results are useful.
For example, pass `agentgrep` hits to `read` and return only needed `path:line` entries.

Call `swarm` directly for short coordination. Use `codemode` for dependent swarm reads or to return only needed fields from large results.
Use `batch` for a handful (1-10) independent calls whose results you want verbatim. Use `bash` for shell work, not to fan out tool calls. Call a tool directly when one call is enough.";

/// Harness capabilities that conditionally contribute prompt modules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptCapabilities {
    pub mermaid: bool,
    pub codemode: bool,
}

impl Default for PromptCapabilities {
    fn default() -> Self {
        Self {
            mermaid: true,
            codemode: false,
        }
    }
}

impl PromptCapabilities {
    fn current() -> Self {
        let config = crate::config::config();
        Self {
            mermaid: config.features.mermaid,
            codemode: config.features.code_mode && config.tools.codemode,
        }
    }
}

fn base_system_prompt_parts(capabilities: PromptCapabilities, base: String) -> Vec<String> {
    base_system_prompt_parts_composed(capabilities, base, &PromptComposition::default())
}

fn base_system_prompt_parts_composed(
    capabilities: PromptCapabilities,
    base: String,
    composition: &PromptComposition<'_>,
) -> Vec<String> {
    // Without a session policy every gated line stays, but the markers never
    // reach the model.
    let base = render_tool_gated(&base, composition.tool_gate.unwrap_or(&|_| true));
    let mut parts = vec![base];
    if let Some(model_prompt) = composition.model_prompt {
        for text in &model_prompt.append {
            parts.push(render_tool_gated(
                text,
                composition.tool_gate.unwrap_or(&|_| true),
            ));
        }
    }
    if capabilities.mermaid {
        parts.push(MERMAID_PROMPT.to_string());
    }
    if capabilities.codemode && composition.tool_allowed("codemode") {
        parts.push(CODEMODE_PROMPT.to_string());
    }
    if let Some(profile) = composition.profile
        && let Some(section) = profile.section()
    {
        parts.push(section);
    }
    parts
}

/// The prompt-visible part of an agent profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilePrompt {
    pub name: String,
    pub instructions: Option<String>,
    /// Whether the available-skills list is part of this profile's prompt.
    pub include_skills: bool,
}

impl ProfilePrompt {
    fn section(&self) -> Option<String> {
        let instructions = self.instructions.as_deref()?.trim();
        (!instructions.is_empty())
            .then(|| format!("# Agent profile: {}\n\n{instructions}", self.name))
    }
}

/// Session-specific inputs to static prompt composition.
///
/// Both are known before the first request and are frozen with the prefix.
#[derive(Clone, Copy, Default)]
pub struct PromptComposition<'a> {
    /// Whether this session may call a tool. `None` keeps every tool-gated line.
    pub tool_gate: Option<&'a dyn Fn(&str) -> bool>,
    pub profile: Option<&'a ProfilePrompt>,
    /// `[[agents.model_prompts]]` resolved for the session's model.
    pub model_prompt: Option<&'a crate::model_prompt::ModelPrompt>,
}

impl PromptComposition<'_> {
    fn tool_allowed(&self, name: &str) -> bool {
        self.tool_gate.is_none_or(|gate| gate(name))
    }

    fn include_skills(&self) -> bool {
        self.profile.is_none_or(|profile| profile.include_skills)
    }
}

const TOOL_GATE_PREFIX: &str = "<!-- tools:";

/// Parse a `<!-- tools: a, b -->` line prefix into its tool names and the text
/// after the marker.
fn split_tool_gate(line: &str) -> Option<(Vec<&str>, &str)> {
    let rest = line.trim_start().strip_prefix(TOOL_GATE_PREFIX)?;
    let (names, text) = rest.split_once("-->")?;
    let names = names
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    Some((names, text.trim_start()))
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    (hashes > 0 && line[hashes..].starts_with(' ')).then_some(hashes)
}

/// Keep a `<!-- tools: a, b -->` line only when the session may call one of
/// the listed tools, then drop headings left without content. Text without
/// markers is returned unchanged, so custom prompts opt in explicitly.
pub fn render_tool_gated(text: &str, gate: &dyn Fn(&str) -> bool) -> String {
    if !text.contains(TOOL_GATE_PREFIX) {
        return text.to_string();
    }
    let kept: Vec<&str> = text
        .lines()
        .filter_map(|line| match split_tool_gate(line) {
            Some((names, rest)) => names.iter().any(|name| gate(name)).then_some(rest),
            None => Some(line),
        })
        .collect();

    let mut out: Vec<&str> = Vec::with_capacity(kept.len());
    for (index, line) in kept.iter().enumerate() {
        if let Some(level) = heading_level(line) {
            let has_content = kept[index + 1..]
                .iter()
                .find(|next| !next.trim().is_empty())
                .is_some_and(|next| heading_level(next).is_none_or(|next| next > level));
            if !has_content {
                continue;
            }
        }
        // Removed lines must not leave stacked blank lines behind.
        if line.trim().is_empty() && out.last().is_none_or(|last| last.trim().is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|line| line.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// Fingerprints of the effective overrides, independent of Jcode's embedded
/// prompt version. `None` means the corresponding file section is absent.
pub type PromptFileVersions = [Option<String>; 3];

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StaticPromptSnapshot {
    pub text: String,
    /// Latest versions reflected either in the frozen text or in the transcript.
    /// `None` when a custom system prompt replaces the normal instructions.
    pub file_versions: Option<PromptFileVersions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills_chars: Option<usize>,
    /// Matched `[[agents.model_prompts]]` rules when the text was rendered.
    /// A model switch that changes this re-renders the prefix.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model_prompt_key: String,
    /// Model the text was rendered for. Empty in older snapshots, which are
    /// never re-rendered.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rendered_for_model: String,
}

impl StaticPromptSnapshot {
    pub fn skills_chars(&self, skills: &[SkillInfo]) -> Option<usize> {
        self.skills_chars.or_else(|| {
            if self.file_versions.is_none() {
                return Some(0);
            }
            let Some(section) = build_available_skills_section(skills) else {
                return Some(0);
            };
            // Legacy snapshots did not record the boundary. Attribute it only
            // if the frozen prompt contains the captured list as a section.
            self.text
                .contains(&format!("\n\n{section}"))
                .then_some(section.len())
        })
    }
}

pub struct PromptFiles {
    base_override: Option<String>,
    overlay: (Option<String>, usize),
    preferred_tools: (Option<String>, usize),
}

impl PromptFiles {
    pub fn load(working_dir: Option<&Path>) -> Self {
        Self {
            base_override: load_base_system_prompt_override(working_dir),
            overlay: load_prompt_overlay_files_from_dir(working_dir),
            preferred_tools: load_preferred_tools_files_from_dir(working_dir),
        }
    }

    fn sections(&self) -> [(&'static str, Option<&str>); 3] {
        [
            ("system-prompt.md", self.base_override.as_deref()),
            ("prompt-overlay.md", self.overlay.0.as_deref()),
            ("preferred-tools.md", self.preferred_tools.0.as_deref()),
        ]
    }

    pub fn versions(&self) -> PromptFileVersions {
        use sha2::{Digest, Sha256};
        self.sections()
            .map(|(_, text)| text.map(|text| format!("{:x}", Sha256::digest(text))))
    }

    pub fn change_reminder(
        &self,
        previous: &PromptFileVersions,
    ) -> Option<(String, PromptFileVersions)> {
        let versions = self.versions();
        if &versions == previous {
            return None;
        }
        let mut reminder = String::from(
            "<system-reminder>\nLocal prompt files changed. The sections below supersede their frozen counterparts and any earlier updates for the same files.\n",
        );
        for (index, (name, text)) in self.sections().into_iter().enumerate() {
            if versions[index] == previous[index] {
                continue;
            }
            reminder.push_str(&format!("\n## {name}\n\n"));
            reminder.push_str(match (name, text) {
                (_, Some(text)) => text,
                ("system-prompt.md", None) => DEFAULT_SYSTEM_PROMPT,
                _ => "This file section is no longer configured.",
            });
            reminder.push('\n');
        }
        reminder.push_str("</system-reminder>");
        Some((reminder, versions))
    }
}

/// Bootstrap inputs frozen along with AGENTS.md and the skill list.
pub struct StaticPromptSources {
    capabilities: PromptCapabilities,
    pub files: PromptFiles,
}

impl StaticPromptSources {
    pub fn load(working_dir: Option<&Path>) -> Self {
        Self::with_capabilities(working_dir, PromptCapabilities::current())
    }

    fn with_capabilities(working_dir: Option<&Path>, capabilities: PromptCapabilities) -> Self {
        Self {
            capabilities,
            files: PromptFiles::load(working_dir),
        }
    }
}

/// Built-in default swarm prompt: model-routing guidance for spawned swarm
/// agents (which model/effort to pick per task kind). Users can override it by
/// creating `~/.jcode/swarm-prompt.md` (global) or `./.jcode/swarm-prompt.md`
/// (project). See [`load_swarm_prompt`].
pub const DEFAULT_SWARM_PROMPT: &str = include_str!("prompt/swarm_prompt.md");

/// Load the swarm prompt used to steer swarm model routing. Precedence:
/// project `./.jcode/swarm-prompt.md`, then global `~/.jcode/swarm-prompt.md`,
/// then the built-in [`DEFAULT_SWARM_PROMPT`].
pub fn load_swarm_prompt(working_dir: Option<&Path>) -> String {
    let project_dir = working_dir.unwrap_or(Path::new("."));
    let candidates = [
        Some(project_dir.join(".jcode").join("swarm-prompt.md")),
        crate::storage::jcode_dir()
            .ok()
            .map(|dir| dir.join("swarm-prompt.md")),
    ];
    for path in candidates.into_iter().flatten() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    DEFAULT_SWARM_PROMPT.trim().to_string()
}

/// Reasoning-effort sentinel that enables swarm mode. Providers translate this
/// to the configured root effort (low by default) when building API requests,
/// while the UI/session keep the literal `swarm` marker so the agent knows to
/// append [`SWARM_USER_MESSAGE_REMINDER`] to the user's message.
pub const SWARM_EFFORT: &str = "swarm";

/// Appended to each user message while swarm mode is active.
pub const SWARM_USER_MESSAGE_REMINDER: &str = "Use swarms to get things done in parallel.";

/// Returns true when `effort` is the swarm sentinel, case-insensitive.
pub fn is_swarm_effort(effort: &str) -> bool {
    effort.trim().eq_ignore_ascii_case(SWARM_EFFORT)
}

/// Configured root reasoning level for the swarm sentinel. Providers translate
/// this real level to their supported range while retaining the sentinel in
/// session state. Ordinary reasoning efforts are left untouched.
pub fn swarm_root_reasoning_effort(effort: &str) -> Option<&'static str> {
    is_swarm_effort(effort).then(|| crate::config::config().agents.root_effort_for_swarm())
}

/// True when an effort rung is swarm mode rather than a plain reasoning level.
/// Swarm mode must not be treated as a per-model effort variant.
pub fn is_swarm_mode_effort(effort: &str) -> bool {
    is_swarm_effort(effort)
}

/// Append the swarm reminder to a user message when swarm mode is active.
/// No-op otherwise, and idempotent.
pub fn with_swarm_user_message_reminder(message: &str, effort: Option<&str>) -> String {
    if !effort.is_some_and(is_swarm_effort) || message.contains(SWARM_USER_MESSAGE_REMINDER) {
        return message.to_string();
    }
    if message.trim().is_empty() {
        return SWARM_USER_MESSAGE_REMINDER.to_string();
    }
    format!("{}\n\n{}", message.trim_end(), SWARM_USER_MESSAGE_REMINDER)
}

/// Mission-continuation template (embedded at compile time). Consumed by the
/// `mission` module in the upper `jcode-app-core` layer; the asset lives here
/// alongside the other prompt templates.
pub const MISSION_CONTINUATION_TEMPLATE: &str = include_str!("prompt/mission_continuation.md");
const SELFDEV_MODE_PROMPT: &str = include_str!("prompt/selfdev_mode.txt");
const DESKTOP_SELFDEV_MODE_PROMPT: &str = include_str!("prompt/desktop_selfdev_mode.txt");
const SELFDEV_FOCUS_TUI_PROMPT: &str = include_str!("prompt/selfdev_focus_tui.txt");
/// Split system prompt for efficient caching
/// Static content is cached, dynamic content is not
#[derive(Debug, Clone, Default)]
pub struct SplitSystemPrompt {
    /// Static content that should be cached (instruction files, base prompt, skills)
    pub static_part: String,
    /// Dynamic turn context that changes per request (memory, active skill, reminders)
    pub dynamic_part: String,
}

impl SplitSystemPrompt {
    pub fn chars(&self) -> usize {
        match (self.static_part.is_empty(), self.dynamic_part.is_empty()) {
            (true, true) => 0,
            (false, true) => self.static_part.len(),
            (true, false) => self.dynamic_part.len(),
            (false, false) => self.static_part.len() + 2 + self.dynamic_part.len(),
        }
    }

    pub fn estimated_tokens(&self) -> usize {
        crate::util::estimate_tokens(&if self.static_part.is_empty() {
            self.dynamic_part.clone()
        } else if self.dynamic_part.is_empty() {
            self.static_part.clone()
        } else {
            format!("{}\n\n{}", self.static_part, self.dynamic_part)
        })
    }
}

/// Skill info for system prompt
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
}

const SKILL_DESC_MAX_CHARS: usize = 120;

fn clip_skill_description(description: &str) -> String {
    let one_line = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= SKILL_DESC_MAX_CHARS {
        return one_line;
    }

    let mut clipped: String = one_line
        .chars()
        .take(SKILL_DESC_MAX_CHARS.saturating_sub(1))
        .collect();
    clipped.push('…');
    clipped
}

fn build_available_skills_section(available_skills: &[SkillInfo]) -> Option<String> {
    if available_skills.is_empty() {
        return None;
    }

    let mut section = "# Available Skills\n\nYou have access to the following skills that the user can invoke with `/skillname`:\n".to_string();
    for skill in available_skills {
        section.push_str(&format!(
            "\n- `/{} ` - {}",
            skill.name,
            clip_skill_description(&skill.description)
        ));
    }
    section.push_str(
        "\n\nWhen a user asks about available skills or capabilities, mention these skills.",
    );
    Some(section)
}

/// Information about what's loaded in the context window
#[derive(Debug, Clone, Default)]
pub struct ContextInfo {
    // === Static (System Prompt) ===
    /// Base system prompt size (chars)
    pub system_prompt_chars: usize,
    /// Immutable session context size (chars), when persisted in transcript history.
    pub session_context_chars: usize,
    /// Whether project AGENTS.md was loaded
    pub has_project_agents_md: bool,
    /// Project AGENTS.md size (chars)
    pub project_agents_md_chars: usize,
    /// Whether global ~/AGENTS.md was loaded
    pub has_global_agents_md: bool,
    /// Global AGENTS.md size (chars)
    pub global_agents_md_chars: usize,
    /// Skills section size (chars)
    pub skills_chars: usize,
    /// Self-dev section size (chars)
    pub selfdev_chars: usize,
    /// Memory section size (chars)
    pub memory_chars: usize,
    /// Prompt overlay section size (chars)
    pub prompt_overlay_chars: usize,
    /// Preferred tools section size (chars)
    pub preferred_tools_chars: usize,
    // === Dynamic (Conversation) ===
    /// Tool definitions sent to API (chars)
    pub tool_defs_chars: usize,
    /// Number of tool definitions
    pub tool_defs_count: usize,
    /// User messages total size (chars)
    pub user_messages_chars: usize,
    /// Number of user messages
    pub user_messages_count: usize,
    /// Assistant messages total size (chars)
    pub assistant_messages_chars: usize,
    /// Number of assistant messages
    pub assistant_messages_count: usize,
    /// Tool calls size (chars)
    pub tool_calls_chars: usize,
    /// Number of tool calls
    pub tool_calls_count: usize,
    /// Tool results size (chars)
    pub tool_results_chars: usize,
    /// Number of tool results
    pub tool_results_count: usize,

    /// Total system prompt size (chars)
    pub total_chars: usize,
}

impl ContextInfo {
    /// Rough estimate of tokens (chars / 4 is a common approximation)
    pub fn estimated_tokens(&self) -> usize {
        self.total_chars / 4
    }

    pub fn prompt_prefix_chars(&self) -> usize {
        self.system_prompt_chars
            + self.session_context_chars
            + self.project_agents_md_chars
            + self.global_agents_md_chars
            + self.skills_chars
            + self.selfdev_chars
            + self.memory_chars
            + self.prompt_overlay_chars
            + self.preferred_tools_chars
            + self.tool_defs_chars
    }

    pub fn prompt_prefix_tokens(&self) -> usize {
        self.prompt_prefix_chars() / 4
    }

    pub fn tool_definition_tokens(&self) -> usize {
        self.tool_defs_chars / 4
    }

    /// Get breakdown as (label, chars, icon) tuples for display
    pub fn breakdown(&self) -> Vec<(&'static str, usize, &'static str)> {
        let mut parts = vec![
            ("sys", self.system_prompt_chars, "⚙"),
            ("session", self.session_context_chars, "🌍"),
        ];
        if self.has_project_agents_md {
            parts.push(("agents", self.project_agents_md_chars, "📋"));
        }
        if self.has_global_agents_md {
            parts.push(("~agents", self.global_agents_md_chars, "📋"));
        }
        if self.skills_chars > 0 {
            parts.push(("skills", self.skills_chars, "🔧"));
        }
        if self.selfdev_chars > 0 {
            parts.push(("dev", self.selfdev_chars, "🛠"));
        }
        if self.memory_chars > 0 {
            parts.push(("mem", self.memory_chars, "🧠"));
        }
        if self.prompt_overlay_chars > 0 {
            parts.push(("overlay", self.prompt_overlay_chars, "🧩"));
        }
        if self.preferred_tools_chars > 0 {
            parts.push(("tools", self.preferred_tools_chars, "🧰"));
        }
        parts
    }
}

/// Build the full system prompt with static context.
pub fn build_system_prompt(skill_prompt: Option<&str>, available_skills: &[SkillInfo]) -> String {
    build_system_prompt_with_selfdev(skill_prompt, available_skills, false)
}

/// Build the full system prompt with optional self-dev tools
pub fn build_system_prompt_with_selfdev(
    skill_prompt: Option<&str>,
    available_skills: &[SkillInfo],
    is_selfdev: bool,
) -> String {
    let (prompt, _) = build_system_prompt_with_context(skill_prompt, available_skills, is_selfdev);
    prompt
}

/// Build the full system prompt and return context info about what was loaded
pub fn build_system_prompt_with_context(
    skill_prompt: Option<&str>,
    available_skills: &[SkillInfo],
    is_selfdev: bool,
) -> (String, ContextInfo) {
    build_system_prompt_with_context_and_memory(skill_prompt, available_skills, is_selfdev, None)
}

/// Build the full system prompt with optional memory section and return context info
pub fn build_system_prompt_with_context_and_memory(
    skill_prompt: Option<&str>,
    available_skills: &[SkillInfo],
    is_selfdev: bool,
    memory_prompt: Option<&str>,
) -> (String, ContextInfo) {
    build_system_prompt_full(
        skill_prompt,
        available_skills,
        is_selfdev,
        memory_prompt,
        None,
    )
}

/// Build the full system prompt with working directory support for loading context files
pub fn build_system_prompt_full(
    skill_prompt: Option<&str>,
    available_skills: &[SkillInfo],
    is_selfdev: bool,
    memory_prompt: Option<&str>,
    working_dir: Option<&Path>,
) -> (String, ContextInfo) {
    build_system_prompt_full_with_capabilities(
        skill_prompt,
        available_skills,
        is_selfdev,
        memory_prompt,
        working_dir,
        PromptCapabilities::current(),
    )
}

pub fn build_system_prompt_full_with_capabilities(
    skill_prompt: Option<&str>,
    available_skills: &[SkillInfo],
    is_selfdev: bool,
    memory_prompt: Option<&str>,
    working_dir: Option<&Path>,
    capabilities: PromptCapabilities,
) -> (String, ContextInfo) {
    let mut parts = base_system_prompt_parts(capabilities, load_base_system_prompt(working_dir));
    let mut info = ContextInfo {
        system_prompt_chars: parts.join("\n\n").len(),
        ..Default::default()
    };

    // Desktop checkout identity takes precedence over the CLI self-dev flag.
    if is_desktop_working_dir(working_dir) {
        info.selfdev_chars = DESKTOP_SELFDEV_MODE_PROMPT.len();
        parts.push(DESKTOP_SELFDEV_MODE_PROMPT.to_string());
    } else if is_selfdev {
        let selfdev_prompt = build_selfdev_prompt_for_working_dir(working_dir);
        info.selfdev_chars = selfdev_prompt.len();
        parts.push(selfdev_prompt);
    }

    // Add AGENTS.md instructions with tracking (from working_dir or cwd)
    let (md_content, md_info) = load_agents_md_files_from_dir(working_dir);
    if let Some(content) = md_content {
        parts.push(content);
    }
    // Merge file info
    info.has_project_agents_md = md_info.has_project_agents_md;
    info.project_agents_md_chars = md_info.project_agents_md_chars;
    info.has_global_agents_md = md_info.has_global_agents_md;
    info.global_agents_md_chars = md_info.global_agents_md_chars;

    // Add optional prompt overlays from ~/.jcode/ and ./.jcode/
    let (overlay_content, overlay_chars) = load_prompt_overlay_files_from_dir(working_dir);
    if let Some(content) = overlay_content {
        info.prompt_overlay_chars = overlay_chars;
        parts.push(content);
    }

    // Add optional preferred-tool guidance from ~/.jcode/ and ./.jcode/
    let (preferred_tools_content, preferred_tools_chars) =
        load_preferred_tools_files_from_dir(working_dir);
    if let Some(content) = preferred_tools_content {
        info.preferred_tools_chars = preferred_tools_chars;
        parts.push(content);
    }

    if let Some(memory) = memory_prompt {
        info.memory_chars = memory.len();
        parts.push(memory.to_string());
    }

    // Add available skills list
    if let Some(skills_section) = build_available_skills_section(available_skills) {
        info.skills_chars = skills_section.len();
        parts.push(skills_section);
    }

    // Add active skill prompt
    if let Some(skill) = skill_prompt {
        parts.push(format!("# Active Skill\n\n{}", skill));
    }

    let prompt = parts.join("\n\n");
    info.total_chars = prompt.len();

    (prompt, info)
}

/// Build system prompt split into static (cacheable) and dynamic parts
/// This improves cache hit rate by keeping frequently-changing content separate
pub fn build_system_prompt_split(
    skill_prompt: Option<&str>,
    available_skills: &[SkillInfo],
    is_selfdev: bool,
    memory_prompt: Option<&str>,
    working_dir: Option<&Path>,
) -> (SplitSystemPrompt, ContextInfo) {
    build_system_prompt_split_with_capabilities(
        skill_prompt,
        available_skills,
        is_selfdev,
        memory_prompt,
        working_dir,
        PromptCapabilities::current(),
    )
}

pub fn build_system_prompt_split_with_capabilities(
    skill_prompt: Option<&str>,
    available_skills: &[SkillInfo],
    is_selfdev: bool,
    memory_prompt: Option<&str>,
    working_dir: Option<&Path>,
    capabilities: PromptCapabilities,
) -> (SplitSystemPrompt, ContextInfo) {
    let agents_md = load_agents_md_files_from_dir(working_dir);
    build_system_prompt_split_with_sources(
        skill_prompt,
        available_skills,
        is_selfdev,
        memory_prompt,
        working_dir,
        &StaticPromptSources::with_capabilities(working_dir, capabilities),
        agents_md,
    )
}

/// Build a split prompt using an already captured AGENTS.md snapshot.
///
/// Long-lived agents use this to keep their provider-cache prefix stable when a
/// tool edits AGENTS.md during the session. New sessions still capture the
/// latest instructions.
pub fn build_system_prompt_split_with_agents_md(
    skill_prompt: Option<&str>,
    available_skills: &[SkillInfo],
    is_selfdev: bool,
    memory_prompt: Option<&str>,
    working_dir: Option<&Path>,
    agents_md: (Option<String>, ContextInfo),
) -> (SplitSystemPrompt, ContextInfo) {
    build_system_prompt_split_with_sources(
        skill_prompt,
        available_skills,
        is_selfdev,
        memory_prompt,
        working_dir,
        &StaticPromptSources::load(working_dir),
        agents_md,
    )
}

pub fn build_system_prompt_split_with_sources(
    skill_prompt: Option<&str>,
    available_skills: &[SkillInfo],
    is_selfdev: bool,
    memory_prompt: Option<&str>,
    working_dir: Option<&Path>,
    sources: &StaticPromptSources,
    agents_md: (Option<String>, ContextInfo),
) -> (SplitSystemPrompt, ContextInfo) {
    build_system_prompt_split_composed(
        skill_prompt,
        available_skills,
        is_selfdev,
        memory_prompt,
        working_dir,
        sources,
        agents_md,
        &PromptComposition::default(),
    )
}

/// Build a split prompt for one session's tool policy and agent profile.
///
/// Order: base or model replacement (tool-gated), model appends, capability
/// modules, agent profile, self-dev,
/// AGENTS.md, overlay, preferred tools, skills. Everything here is static for
/// the session and freezes with the provider-cache prefix.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors build_system_prompt_split_with_sources plus the session composition"
)]
pub fn build_system_prompt_split_composed(
    skill_prompt: Option<&str>,
    available_skills: &[SkillInfo],
    is_selfdev: bool,
    memory_prompt: Option<&str>,
    working_dir: Option<&Path>,
    sources: &StaticPromptSources,
    agents_md: (Option<String>, ContextInfo),
    composition: &PromptComposition<'_>,
) -> (SplitSystemPrompt, ContextInfo) {
    let base = composition
        .model_prompt
        .and_then(|model_prompt| model_prompt.replace.as_deref())
        .or(sources.files.base_override.as_deref())
        .unwrap_or(DEFAULT_SYSTEM_PROMPT);
    let mut static_parts =
        base_system_prompt_parts_composed(sources.capabilities, base.into(), composition);
    let mut dynamic_parts = Vec::new();
    let mut info = ContextInfo {
        system_prompt_chars: static_parts.join("\n\n").len(),
        ..Default::default()
    };

    // === STATIC CONTENT (cacheable) ===

    // Keep Desktop guidance cacheable and separate from CLI self-development.
    if is_desktop_working_dir(working_dir) {
        info.selfdev_chars = DESKTOP_SELFDEV_MODE_PROMPT.len();
        static_parts.push(DESKTOP_SELFDEV_MODE_PROMPT.to_string());
    } else if is_selfdev {
        let selfdev_prompt = build_selfdev_prompt_static_for_working_dir(working_dir);
        info.selfdev_chars = selfdev_prompt.len();
        static_parts.push(selfdev_prompt);
    }

    // Add AGENTS.md instructions (static per project)
    let (md_content, md_info) = agents_md;
    if let Some(content) = md_content {
        static_parts.push(content);
    }
    info.has_project_agents_md = md_info.has_project_agents_md;
    info.project_agents_md_chars = md_info.project_agents_md_chars;
    info.has_global_agents_md = md_info.has_global_agents_md;
    info.global_agents_md_chars = md_info.global_agents_md_chars;

    // Add optional prompt overlays from ~/.jcode/ and ./.jcode/
    let (overlay_content, overlay_chars) = &sources.files.overlay;
    if let Some(content) = overlay_content {
        info.prompt_overlay_chars = *overlay_chars;
        static_parts.push(content.clone());
    }

    // Add optional preferred-tool guidance (static per project/user)
    let (preferred_tools_content, preferred_tools_chars) = &sources.files.preferred_tools;
    if let Some(content) = preferred_tools_content {
        info.preferred_tools_chars = *preferred_tools_chars;
        static_parts.push(content.clone());
    }

    // Add available skills list (fairly static)
    if composition.include_skills()
        && let Some(skills_section) = build_available_skills_section(available_skills)
    {
        info.skills_chars = skills_section.len();
        static_parts.push(skills_section);
    }

    // === TURN CONTEXT (not cached) ===

    // Memory prompt (changes per conversation)
    if let Some(memory) = memory_prompt {
        info.memory_chars = memory.len();
        dynamic_parts.push(memory.to_string());
    }

    // Active skill prompt (changes per skill invocation)
    if let Some(skill) = skill_prompt {
        dynamic_parts.push(format!("# Active Skill\n\n{}", skill));
    }

    let static_part = static_parts.join("\n\n");
    let dynamic_part = dynamic_parts.join("\n\n");
    info.total_chars = static_part.len() + dynamic_part.len();

    (
        SplitSystemPrompt {
            static_part,
            dynamic_part,
        },
        info,
    )
}

/// Detect Desktop independently of the CLI self-development flag.
fn is_desktop_working_dir(working_dir: Option<&Path>) -> bool {
    working_dir
        .and_then(jcode_selfdev_types::desktop_repo_root)
        .is_some()
}

/// Build self-dev tools prompt section (static version without dynamic socket path)
#[cfg(test)]
fn build_selfdev_prompt_static() -> String {
    build_selfdev_prompt_static_for_context(SelfDevProductContext::Tui)
}

/// Build self-dev tools prompt section
#[cfg(test)]
fn build_selfdev_prompt() -> String {
    build_selfdev_prompt_for_context(SelfDevProductContext::Tui)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelfDevProductContext {
    Tui,
}

impl SelfDevProductContext {
    fn from_working_dir(working_dir: Option<&Path>) -> Self {
        let _ = working_dir;
        Self::Tui
    }

    fn prompt_block(self) -> &'static str {
        match self {
            Self::Tui => SELFDEV_FOCUS_TUI_PROMPT,
        }
    }
}

fn build_selfdev_prompt_static_for_working_dir(working_dir: Option<&Path>) -> String {
    build_selfdev_prompt_static_for_context(SelfDevProductContext::from_working_dir(working_dir))
}

fn build_selfdev_prompt_for_working_dir(working_dir: Option<&Path>) -> String {
    build_selfdev_prompt_for_context(SelfDevProductContext::from_working_dir(working_dir))
}

fn build_selfdev_prompt_static_for_context(context: SelfDevProductContext) -> String {
    build_selfdev_prompt_for_context(context).replace("__DEBUG_SOCKET_BLOCK__\n\n", "")
}

fn build_selfdev_prompt_for_context(context: SelfDevProductContext) -> String {
    SELFDEV_MODE_PROMPT.replace("__SELFDEV_PRODUCT_FOCUS__", context.prompt_block())
}

/// Build immutable session context captured once per session.
pub fn build_session_context(working_dir: Option<&Path>) -> String {
    let mut lines = vec!["# Session Context".to_string()];

    if is_desktop_working_dir(working_dir) {
        lines.push("Self-development mode: desktop".to_string());
    }

    lines.extend(session_datetime_lines());
    lines.push(format!("OS: {}", std::env::consts::OS));
    lines.push(format!("Architecture: {}", std::env::consts::ARCH));
    lines.push(format!(
        "Jcode version: {} ({})",
        jcode_build_meta::version(),
        jcode_build_meta::git_hash()
    ));

    if let Some(hardware) = hardware_context() {
        lines.push(hardware);
    }

    let cwd = working_dir.map(Path::to_path_buf);
    if let Some(cwd) = cwd.as_deref() {
        lines.push(format!("Working directory: {}", cwd.display()));
        if let Some(git_info) = get_git_info(Some(cwd)) {
            lines.push(git_info);
        }
    }

    lines.join("\n")
}

fn session_datetime_lines() -> [String; 3] {
    std::panic::catch_unwind(|| format_session_datetime(chrono::Local::now()))
        .unwrap_or_else(|_| format_session_datetime(chrono::Utc::now()))
}

fn format_session_datetime<Tz>(now: chrono::DateTime<Tz>) -> [String; 3]
where
    Tz: chrono::TimeZone,
    Tz::Offset: std::fmt::Display,
{
    [
        format!("Date: {}", now.format("%Y-%m-%d")),
        format!("Time: {}", now.format("%H:%M:%S")),
        format!("Timezone: {}", now.format("%Z")),
    ]
}

/// Get git branch and status summary
///
/// Every connecting client builds a session context, and a reload reconnects
/// dozens at once, each forking three `git` processes for usually the same
/// repo (avg 0.8s, up to 2.4s under that storm). Share one probe per
/// directory across concurrent callers and reuse it briefly. The TTL is short
/// enough that a genuinely new session still sees current status.
fn get_git_info(working_dir: Option<&Path>) -> Option<String> {
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, Mutex};
    use std::time::{Duration, Instant};

    const TTL: Duration = Duration::from_secs(3);
    type Slot = Arc<Mutex<Option<(Instant, Option<String>)>>>;
    static CACHE: LazyLock<Mutex<HashMap<Option<std::path::PathBuf>, Slot>>> =
        LazyLock::new(Default::default);

    let key = working_dir.map(Path::to_path_buf);
    let slot = CACHE
        .lock()
        .map(|mut cache| cache.entry(key).or_default().clone())
        .ok();
    let Some(slot) = slot else {
        return get_git_info_uncached(working_dir);
    };
    // Holding the per-directory lock while probing makes concurrent callers
    // for the same repo wait for one result instead of forking their own.
    let mut entry = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, info)) = entry.as_ref()
        && at.elapsed() < TTL
    {
        return info.clone();
    }
    let info = get_git_info_uncached(working_dir);
    *entry = Some((Instant::now(), info.clone()));
    info
}

fn get_git_info_uncached(working_dir: Option<&Path>) -> Option<String> {
    let mut command = Command::new("git");
    if let Some(dir) = working_dir {
        command.current_dir(dir);
    }
    // Check if we're in a git repo
    let in_repo = command
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !in_repo {
        return None;
    }

    let mut info = vec!["Git:".to_string()];

    // Current branch
    let mut branch_command = Command::new("git");
    if let Some(dir) = working_dir {
        branch_command.current_dir(dir);
    }
    if let Ok(output) = branch_command.args(["branch", "--show-current"]).output()
        && output.status.success()
    {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !branch.is_empty() {
            info.push(format!("  Branch: {}", branch));
        }
    }

    // Short status (modified files count)
    let mut status_command = Command::new("git");
    if let Some(dir) = working_dir {
        status_command.current_dir(dir);
    }
    if let Ok(output) = status_command.args(["status", "--porcelain"]).output()
        && output.status.success()
    {
        let status = String::from_utf8_lossy(&output.stdout);
        let modified: Vec<&str> = status.lines().take(5).collect();
        if !modified.is_empty() {
            info.push(format!("  Modified: {} files", status.lines().count()));
            for file in modified {
                info.push(format!("    {}", file));
            }
            if status.lines().count() > 5 {
                info.push("    ...".to_string());
            }
        }
    }

    if info.len() > 1 {
        Some(info.join("\n"))
    } else {
        None
    }
}

fn hardware_context() -> Option<String> {
    // Hardware never changes for the life of the process, but this used to be
    // rebuilt for every session create/attach, forking `lspci` each time. On a
    // busy shared server that meant one subprocess per client connection.
    static HARDWARE_CONTEXT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    HARDWARE_CONTEXT
        .get_or_init(hardware_context_uncached)
        .clone()
}

fn hardware_context_uncached() -> Option<String> {
    let mut lines = Vec::new();

    if let Some(machine) = machine_model() {
        lines.push(format!("  Machine: {}", machine));
    }
    if let Some(cpu) = cpu_model() {
        lines.push(format!("  CPU: {}", cpu));
    }
    if let Some(gpu) = gpu_summary() {
        lines.push(format!("  GPU: {}", gpu));
    }
    if let Some(memory) = memory_summary() {
        lines.push(format!("  Memory: {}", memory));
    }

    if lines.is_empty() {
        None
    } else {
        let mut out = vec!["Hardware:".to_string()];
        out.extend(lines);
        Some(out.join("\n"))
    }
}

fn read_trimmed_file(path: impl Into<PathBuf>) -> Option<String> {
    std::fs::read_to_string(path.into())
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn machine_model() -> Option<String> {
    let vendor = read_trimmed_file("/sys/devices/virtual/dmi/id/sys_vendor");
    let product = read_trimmed_file("/sys/devices/virtual/dmi/id/product_name");
    match (vendor, product) {
        (Some(vendor), Some(product)) if product.contains(&vendor) => Some(product),
        (Some(vendor), Some(product)) => Some(format!("{} {}", vendor, product)),
        (None, Some(product)) => Some(product),
        (Some(vendor), None) => Some(vendor),
        (None, None) => None,
    }
}

fn cpu_model() -> Option<String> {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    cpuinfo.lines().find_map(|line| {
        let (_, value) = line.split_once(':')?;
        if line.trim_start().starts_with("model name") {
            let value = value.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        } else {
            None
        }
    })
}

fn memory_summary() -> Option<String> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb = meminfo.lines().find_map(|line| {
        let rest = line.strip_prefix("MemTotal:")?.trim();
        rest.split_whitespace().next()?.parse::<u64>().ok()
    })?;
    let gib = kb as f64 / 1024.0 / 1024.0;
    Some(format!("{:.1} GiB", gib))
}

fn gpu_summary() -> Option<String> {
    let output = Command::new("lspci").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut gpus: Vec<String> = text
        .lines()
        .filter(|line| {
            line.contains(" VGA compatible controller")
                || line.contains(" 3D controller")
                || line.contains(" Display controller")
        })
        .filter_map(|line| {
            line.split_once(':')
                .map(|(_, rest)| rest.trim().to_string())
        })
        .collect();
    gpus.dedup();
    if gpus.is_empty() {
        None
    } else {
        Some(gpus.join("; "))
    }
}

fn same_canonical_path(first: &Path, second: &Path) -> bool {
    match (std::fs::canonicalize(first), std::fs::canonicalize(second)) {
        (Ok(first), Ok(second)) => first == second,
        _ => false,
    }
}

fn load_agents_md_files_from_dirs(
    project_dir: &Path,
    global_agents_md: Option<&Path>,
) -> (Option<String>, ContextInfo) {
    let mut contents = vec![];
    let mut info = ContextInfo::default();

    // Helper to load a file if it exists, returns (formatted_content, raw_size)
    let load_file = |path: &Path, label: &str| -> Option<(String, usize)> {
        if path.exists() {
            std::fs::read_to_string(path).ok().map(|content| {
                let raw_size = content.len();
                let formatted = format!("# {}\n\n{}", label, content.trim());
                (formatted, raw_size)
            })
        } else {
            None
        }
    };

    let project_agents_md = project_dir.join("AGENTS.md");
    if let Some((content, size)) = load_file(&project_agents_md, "Project Instructions (AGENTS.md)")
    {
        info.has_project_agents_md = true;
        info.project_agents_md_chars = size;
        contents.push(content);
    }

    // Canonical file identity handles cwd=$HOME as well as symlinked aliases.
    // If either file is absent or cannot be resolved, loading below remains the
    // source of truth and simply skips unreadable files.
    let global_duplicates_project = global_agents_md
        .is_some_and(|global_agents_md| same_canonical_path(&project_agents_md, global_agents_md));

    if !global_duplicates_project
        && let Some(global_agents_md) = global_agents_md
        && let Some((content, size)) =
            load_file(global_agents_md, "Global Instructions (~/AGENTS.md)")
    {
        info.has_global_agents_md = true;
        info.global_agents_md_chars = size;
        contents.push(content);
    }

    if contents.is_empty() {
        (None, info)
    } else {
        (Some(contents.join("\n\n")), info)
    }
}

/// Load AGENTS.md files from a specific working directory.
pub fn load_agents_md_files_from_dir(working_dir: Option<&Path>) -> (Option<String>, ContextInfo) {
    let project_dir = working_dir.unwrap_or(Path::new("."));
    let global_agents_md = crate::storage::user_home_path("AGENTS.md").ok();
    load_agents_md_files_from_dirs(project_dir, global_agents_md.as_deref())
}

/// Load optional prompt overlay markdown from ~/.jcode/ and ./.jcode/
fn load_prompt_overlay_files_from_dir(working_dir: Option<&Path>) -> (Option<String>, usize) {
    let mut contents = vec![];
    let mut total_chars = 0usize;

    let load_file = |path: &Path, label: &str| -> Option<(String, usize)> {
        if path.exists() {
            std::fs::read_to_string(path).ok().map(|content| {
                let raw_size = content.len();
                let formatted = format!("# {}\n\n{}", label, content.trim());
                (formatted, raw_size)
            })
        } else {
            None
        }
    };

    let project_dir = working_dir.unwrap_or(Path::new("."));
    let project_overlay = project_dir.join(".jcode").join("prompt-overlay.md");
    if let Some((content, size)) = load_file(
        &project_overlay,
        "Project Prompt Overlay (.jcode/prompt-overlay.md)",
    ) {
        total_chars += size;
        contents.push(content);
    }

    if let Ok(global_overlay) = crate::storage::jcode_dir().map(|dir| dir.join("prompt-overlay.md"))
        && !same_canonical_path(&project_overlay, &global_overlay)
        && let Some((content, size)) = load_file(
            &global_overlay,
            "Global Prompt Overlay (~/.jcode/prompt-overlay.md)",
        )
    {
        total_chars += size;
        contents.push(content);
    }

    if contents.is_empty() {
        (None, 0)
    } else {
        (Some(contents.join("\n\n")), total_chars)
    }
}

/// Load optional preferred-tool guidance from ~/.jcode/ and ./.jcode/
fn load_preferred_tools_files_from_dir(working_dir: Option<&Path>) -> (Option<String>, usize) {
    let mut contents = vec![];
    let mut total_chars = 0usize;

    let load_file = |path: &Path, label: &str| -> Option<(String, usize)> {
        if path.exists() {
            std::fs::read_to_string(path).ok().map(|content| {
                let raw_size = content.len();
                let formatted = format!("# {}\n\n{}", label, content.trim());
                (formatted, raw_size)
            })
        } else {
            None
        }
    };

    let project_dir = working_dir.unwrap_or(Path::new("."));
    let project_preferred_tools = project_dir.join(".jcode").join("preferred-tools.md");
    if let Some((content, size)) = load_file(
        &project_preferred_tools,
        "Project Preferred Tools (.jcode/preferred-tools.md)",
    ) {
        total_chars += size;
        contents.push(content);
    }

    if let Ok(global_preferred_tools) =
        crate::storage::jcode_dir().map(|dir| dir.join("preferred-tools.md"))
        && !same_canonical_path(&project_preferred_tools, &global_preferred_tools)
        && let Some((content, size)) = load_file(
            &global_preferred_tools,
            "Global Preferred Tools (~/.jcode/preferred-tools.md)",
        )
    {
        total_chars += size;
        contents.push(content);
    }

    if contents.is_empty() {
        (None, 0)
    } else {
        (Some(contents.join("\n\n")), total_chars)
    }
}

#[cfg(test)]
#[path = "prompt_tests.rs"]
mod prompt_tests;
