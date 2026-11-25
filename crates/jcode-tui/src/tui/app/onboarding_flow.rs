//! First-run onboarding flow state machine.
//!
//! After the user logs in / imports credentials on a fresh install, we walk
//! them through a short guided flow:
//!
//!   1. `Login` - if we boot without working credentials, ask the
//!      user to log in right inside the TUI (the fresh
//!      install no longer runs a blocking CLI login).
//!      Skipped entirely when credentials already exist.
//!   2. `StartChoice` - show two stacked actions: run a suggested Git-based bug
//!      and architecture review, or start a blank new session.
//!   3. `Suggestions` - the existing prompt-suggestion cards. Reached when
//!      they choose "Start a new session" or as the terminal resting state.
//!
//!   (`ContinuePrompt` is retained as a legacy phase for replay/test fixtures
//!   but is no longer entered by the live flow.)
//!
//! Session history is intentionally excluded from onboarding and remains
//! available later through `/resume`.

use crossterm::event::KeyCode;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long we wait on a yes/no decision phase (login import, telemetry
/// consent) before auto-selecting the highlighted default. We keep this short
/// enough that the user doesn't get stuck deliberating, but long enough to
/// read the prompt.
pub(crate) const DECISION_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) use crate::tui::session_picker::ExternalCli;

/// Result of applying a key to the import walkthrough.
pub(crate) enum ReviewKey {
    /// The screen does not use this key.
    Ignored,
    /// The key changed the walkthrough.
    Handled,
    /// The user committed the import.
    Commit,
}

/// State of the informational "Telemetry" page (telemetry is removed in this
/// build): just the focused "Continue" button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TelemetrySettings {
    pub(crate) cursor: usize,
    pub(crate) checked: [bool; 6],
}

impl TelemetrySettings {
    pub(crate) fn current() -> Self {
        Self {
            cursor: 0,
            checked: [false; 6],
        }
    }
}

/// Single-screen review for importing detected external logins.
///
/// On a fresh install we may detect logins left behind by other tools (Codex,
/// Claude Code, Copilot, OpenClaw, Hermes, ...). The default screen is a
/// read-only SUMMARY: it lists everything we detected and lands the user on a
/// preselected "Continue" pill that imports all of it with one Enter. A second
/// "Choose what to import" pill switches to the per-login checkbox list
/// (`choosing = true`) for users who want to opt logins out individually.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImportReview {
    /// All detected importable logins, in display order.
    pub(crate) candidates: Vec<crate::external_auth::ExternalAuthReviewCandidate>,
    /// Per-candidate checked state (parallel to `candidates`). `true` = import.
    /// All start checked so the default action imports everything.
    pub(crate) checked: Vec<bool>,
    /// Index of the row the cursor is currently on (for toggling/highlight).
    pub(crate) cursor: usize,
    /// Focus flag for the "Continue" pill.
    ///
    /// On the summary screen (`choosing == false`) this is `true` when the
    /// "Continue" pill is focused (the default landing spot) and `false` when
    /// the "Choose what to import" pill is focused.
    ///
    /// On the checkbox list (`choosing == true`) it is `true` while the
    /// "Continue" pill above/below the rows is focused rather than a login row.
    pub(crate) continue_focused: bool,
    /// `false` = the default summary screen (detected logins listed read-only,
    /// Continue preselected). `true` = the per-login checkbox list.
    pub(crate) choosing: bool,
    /// Which summary pill is focused when `choosing == false`.
    pub(crate) summary_pill: SummaryPill,
    /// `Some` while the "Telemetry settings" sub-page is open, holding the
    /// highlighted level. Committing or pressing Esc returns to the summary.
    pub(crate) telemetry: Option<TelemetrySettings>,
    /// When the screen was first shown, for the single decision countdown.
    pub(crate) shown_at: Instant,
}

/// The actions on the import summary screen, left to right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SummaryPill {
    /// Import every detected login and move on (default).
    Continue,
    /// Open the per-login checkbox list to import fewer logins.
    ImportLess,
    /// Open the telemetry settings sub-page.
    Telemetry,
}

impl SummaryPill {
    const ORDER: [SummaryPill; 3] = [
        SummaryPill::Continue,
        SummaryPill::ImportLess,
        SummaryPill::Telemetry,
    ];

    fn index(self) -> usize {
        Self::ORDER.iter().position(|&p| p == self).unwrap_or(0)
    }

    fn step(self, forward: bool) -> Self {
        let len = Self::ORDER.len();
        let i = self.index();
        let next = if forward {
            (i + 1) % len
        } else {
            (i + len - 1) % len
        };
        Self::ORDER[next]
    }
}

impl ImportReview {
    /// Create a review for the given candidates with every login pre-checked,
    /// starting on the summary screen with Import preselected.
    /// Returns `None` if there are no candidates.
    pub(crate) fn new(
        candidates: Vec<crate::external_auth::ExternalAuthReviewCandidate>,
    ) -> Option<Self> {
        if candidates.is_empty() {
            return None;
        }
        let checked = vec![true; candidates.len()];
        Some(Self {
            candidates,
            checked,
            cursor: 0,
            continue_focused: true,
            choosing: false,
            summary_pill: SummaryPill::Continue,
            telemetry: None,
            shown_at: Instant::now(),
        })
    }

    /// Switch from the summary screen to the per-login checkbox list, with the
    /// cursor on the first login row.
    pub(crate) fn enter_choose_mode(&mut self) {
        self.choosing = true;
        self.continue_focused = false;
        self.cursor = 0;
        self.telemetry = None;
    }

    /// Open the telemetry settings sub-page with the persisted choices and
    /// the Continue button focused.
    pub(crate) fn open_telemetry(&mut self) {
        self.telemetry = Some(TelemetrySettings::current());
    }

    /// Close the telemetry sub-page and return to the summary screen with the
    /// "Telemetry settings" pill still focused.
    pub(crate) fn close_telemetry(&mut self) {
        self.telemetry = None;
    }

    /// Move the summary pill focus. Keeps `continue_focused` in sync so the
    /// existing rendering/commit paths stay correct.
    pub(crate) fn summary_step(&mut self, forward: bool) {
        self.summary_pill = self.summary_pill.step(forward);
        self.continue_focused = self.summary_pill == SummaryPill::Continue;
    }

    /// Focus a specific summary pill (used by tests to land directly on a
    /// given screen state).
    #[cfg(test)]
    pub(crate) fn focus_summary_pill(&mut self, pill: SummaryPill) {
        self.summary_pill = pill;
        self.continue_focused = pill == SummaryPill::Continue;
    }

    /// The candidate the cursor is currently on, if any. Returns `None` while
    /// the "Continue" pill is focused.
    #[allow(dead_code)] // Accessor kept for the import-review UI; not wired to a caller yet.
    pub(crate) fn current(&self) -> Option<&crate::external_auth::ExternalAuthReviewCandidate> {
        if self.continue_focused {
            return None;
        }
        self.candidates.get(self.cursor)
    }

    /// 1-based position of the cursor row (for "1 of 3" display).
    #[allow(dead_code)] // Accessor kept for the import-review UI; not wired to a caller yet.
    pub(crate) fn position(&self) -> usize {
        self.cursor + 1
    }

    /// Total number of candidates being reviewed.
    pub(crate) fn total(&self) -> usize {
        self.candidates.len()
    }

    /// Move focus to the previous item, treating the "Continue" pill as a single
    /// element that sits both above and below the list. The cycle is:
    /// Continue -> last row -> ... -> first row -> Continue.
    pub(crate) fn cursor_up(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        if self.continue_focused {
            // From Continue, step up onto the last row.
            self.continue_focused = false;
            self.cursor = self.candidates.len() - 1;
        } else if self.cursor == 0 {
            // Above the first row sits the Continue pill.
            self.continue_focused = true;
        } else {
            self.cursor -= 1;
        }
    }

    /// Move focus to the next item. The cycle is:
    /// first row -> ... -> last row -> Continue -> first row.
    pub(crate) fn cursor_down(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        if self.continue_focused {
            // From Continue, step down onto the first row.
            self.continue_focused = false;
            self.cursor = 0;
        } else if self.cursor + 1 >= self.candidates.len() {
            // Below the last row sits the Continue pill.
            self.continue_focused = true;
        } else {
            self.cursor += 1;
        }
    }

    /// Toggle the checked state of the row under the cursor. No-op while the
    /// "Continue" pill is focused.
    pub(crate) fn toggle_current(&mut self) {
        if self.continue_focused {
            return;
        }
        if let Some(slot) = self.checked.get_mut(self.cursor) {
            *slot = !*slot;
        }
    }

    /// Set the checked state of the row under the cursor. No-op while the
    /// "Continue" pill is focused.
    pub(crate) fn set_current(&mut self, checked: bool) {
        if self.continue_focused {
            return;
        }
        if let Some(slot) = self.checked.get_mut(self.cursor) {
            *slot = checked;
        }
    }

    /// Whether the row under the cursor is currently checked. False while the
    /// "Continue" pill is focused.
    #[allow(dead_code)] // Accessor kept for the import-review UI; not wired to a caller yet.
    pub(crate) fn current_checked(&self) -> bool {
        if self.continue_focused {
            return false;
        }
        self.checked.get(self.cursor).copied().unwrap_or(false)
    }

    /// The zero-based indices of all checked (to-be-imported) candidates.
    pub(crate) fn approved_indices(&self) -> Vec<usize> {
        self.checked
            .iter()
            .enumerate()
            .filter_map(|(i, &c)| c.then_some(i))
            .collect()
    }

    /// How many logins are currently checked for import.
    pub(crate) fn checked_count(&self) -> usize {
        self.checked.iter().filter(|&&c| c).count()
    }

    /// Seconds left before the screen auto-commits its default (import all
    /// currently-checked logins).
    pub(crate) fn seconds_remaining(&self) -> u64 {
        DECISION_TIMEOUT
            .saturating_sub(self.shown_at.elapsed())
            .as_secs()
    }

    /// Apply one key to the walkthrough.
    pub(crate) fn handle_key(&mut self, code: KeyCode) -> ReviewKey {
    if self.telemetry.is_some() {
        // Telemetry sub-page: informational only (telemetry is removed
        // in this build). Esc/Enter/Space return to the summary.
        match code {
            KeyCode::Esc => self.close_telemetry(),
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.close_telemetry();
                self.summary_pill = SummaryPill::Continue;
                self.continue_focused = true;
            }
            _ => return ReviewKey::Ignored,
        }
    } else if !self.choosing {
        // Summary: Continue, Import less, and Telemetry pills.
        // Arrows/Tab move; Enter/Space commits the focused one.
        match code {
            KeyCode::Left
            | KeyCode::Up
            | KeyCode::BackTab
            | KeyCode::Char('h')
            | KeyCode::Char('k') => self.summary_step(false),
            KeyCode::Right
            | KeyCode::Down
            | KeyCode::Tab
            | KeyCode::Char('j')
            | KeyCode::Char('l') => self.summary_step(true),
            // c is a direct shortcut into choose mode.
            KeyCode::Char('c') | KeyCode::Char('C') => self.enter_choose_mode(),
            // t jumps straight to the telemetry page.
            KeyCode::Char('t') | KeyCode::Char('T') => self.open_telemetry(),
            // y = "yes, import everything" regardless of pill focus, so
            // the timeout is never a forced wait.
            KeyCode::Char('y') | KeyCode::Char('Y') => return ReviewKey::Commit,
            KeyCode::Enter | KeyCode::Char(' ') => match self.summary_pill {
                SummaryPill::Continue => return ReviewKey::Commit,
                SummaryPill::ImportLess => self.enter_choose_mode(),
                SummaryPill::Telemetry => self.open_telemetry(),
            },
            _ => return ReviewKey::Ignored,
        }
    } else {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor_up(),
            KeyCode::Down | KeyCode::Char('j') => self.cursor_down(),
            // Left = Yes (import), Right = No (skip), for the highlighted row.
            KeyCode::Left
            | KeyCode::Char('h')
            | KeyCode::Char('y')
            | KeyCode::Char('Y') => self.set_current(true),
            KeyCode::Right
            | KeyCode::Char('l')
            | KeyCode::Char('n')
            | KeyCode::Char('N') => self.set_current(false),
            // Space toggles the highlighted row between Yes and No.
            KeyCode::Char(' ') => self.toggle_current(),
            // Enter on a login row selects/deselects that row, so
            // "picking an option" never commits the whole import out
            // from under the user. Only Enter on the Continue pill
            // (or `c`) commits.
            KeyCode::Enter if !self.continue_focused => self.toggle_current(),
            KeyCode::Enter | KeyCode::Char('c') | KeyCode::Char('C') => return ReviewKey::Commit,
            _ => return ReviewKey::Ignored,
        }
    }
        ReviewKey::Handled
    }

    /// Status-line text for the current screen of the walkthrough.
    pub(crate) fn status_notice(&self) -> String {
        let checked = self.checked_count();
        let total = self.total();
        let secs = self.seconds_remaining();
        if !self.choosing {
            if self.telemetry.is_some() {
                "Telemetry is disabled in this build - Enter or Esc goes back".to_string()
            } else {
                format!(
                    "Found {total} login{} - Enter imports all (auto in {secs}s), or pick \"Import less\"",
                    if total == 1 { "" } else { "s" },
                )
            }
        } else {
            format!(
                "Import {checked} of {total} login{} - Enter/Space toggles, arrows move, Continue imports",
                if total == 1 { "" } else { "s" },
            )
        }
    }

    /// Whether the telemetry sub-page is showing.
    pub(crate) fn telemetry_page_open(&self) -> bool {
        self.telemetry.is_some()
    }

    /// The candidates and the indices the user approved, for running the import.
    pub(crate) fn approved_candidates(
        &self,
    ) -> (
        Vec<crate::external_auth::ExternalAuthReviewCandidate>,
        Vec<usize>,
    ) {
        (self.candidates.clone(), self.approved_indices())
    }

    /// What the welcome card renders for this walkthrough.
    pub(crate) fn welcome_prompt(&self) -> crate::tui::LoginImportPrompt {
        let rows = self
            .candidates
            .iter()
            .enumerate()
            .map(|(i, candidate)| crate::tui::LoginImportRow {
                provider_summary: candidate.provider_summary().to_string(),
                source_name: candidate.source_name().to_string(),
                checked: self.checked.get(i).copied().unwrap_or(false),
            })
            .collect();
        crate::tui::LoginImportPrompt {
            rows,
            cursor: self.cursor,
            continue_focused: self.continue_focused,
            choosing: self.choosing,
            summary_pill: match self.summary_pill {
                SummaryPill::Continue => crate::tui::ImportSummaryPill::Continue,
                SummaryPill::ImportLess => crate::tui::ImportSummaryPill::ImportLess,
                SummaryPill::Telemetry => crate::tui::ImportSummaryPill::Telemetry,
            },
            telemetry: self.telemetry.map(|t| crate::tui::TelemetryPage {
                cursor: t.cursor,
                checked: t.checked,
            }),
            telemetry_env_forced_off: crate::telemetry::opt_out_forced_by_env(),
            checked_count: self.checked_count(),
            seconds_left: self.seconds_remaining(),
        }
    }

    /// Whether the decision countdown has elapsed. Paused while the telemetry
    /// settings sub-page or the per-login "Import less" list is open, so the
    /// screen never commits the import out from under a user who is choosing.
    pub(crate) fn timed_out(&self) -> bool {
        self.telemetry.is_none() && !self.choosing && self.shown_at.elapsed() >= DECISION_TIMEOUT
    }
}

/// The current phase of the onboarding flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OnboardingPhase {
    /// Log in. Entered on a fresh install when no working credentials exist.
    /// The TUI now owns the entire first-run login experience instead of the
    /// old blocking CLI provider prompt.
    ///
    /// When we detect importable external logins, `import` holds a per-candidate
    /// yes/no walkthrough so the user can step through and choose what to import.
    /// When `None`, there was nothing to import and we prompt the user to pick a
    /// provider manually (Enter opens the login picker).
    Login { import: Option<ImportReview> },
    /// Ask the user how to log in on a fresh install when no importable
    /// external logins were detected: OpenAI, Claude, API key, or Skip.
    /// Defaults to OpenAI. Unlike the import/telemetry prompts this one has no
    /// auto-timeout: logging in is a meaningful first step, so we wait for the
    /// user rather than opening a browser on a countdown.
    LoginOpenAi {
        /// Which sign-in option is highlighted (default `OpenAi`).
        choice: crate::tui::OnboardingLoginChoice,
    },
    /// Legacy phase kept for compatibility with older replay/test fixtures.
    /// New onboarding skips explicit model selection and uses the default route;
    /// users can still run `/model` later.
    ModelSelect,
    /// "Continue where you left off in <cli>?" Yes/No with a
    /// [`DECISION_TIMEOUT`] countdown. Highlightable Yes/No selector to match
    /// the import prompt; the default (and timeout choice) is "Yes" so the
    /// resume menu opens unless the user declines.
    ContinuePrompt {
        cli: ExternalCli,
        /// Which option is highlighted (true = "Yes, continue").
        yes_highlighted: bool,
        /// When the prompt was shown, for the countdown.
        shown_at: Instant,
    },
    /// Action-only picker offering the suggested review or a blank new session.
    StartChoice { shown_at: Instant },
    /// Legacy prompt-suggestion cards state. The live flow no longer enters it
    /// (it lands on the regular new-session screen instead), but replay, golden
    /// and onboarding-graph fixtures still exercise it.
    #[cfg_attr(not(test), allow(dead_code))]
    Suggestions,
    /// Flow finished; nothing onboarding-specific to render.
    Done,
}

/// A first-run new-session model-validation request that is waiting for a
/// concrete default-model id to be known before it fires. In remote/client
/// mode the live model is reported by the server asynchronously, so the
/// onboarding tick polls until a real id (not "unknown") is available, then
/// runs the lightweight validation ping.
///
/// When the validation is requested right after a login (remote mode), the
/// server also pushes a fresh model catalog a moment later (e.g. switching the
/// route to gpt-5.5 after an OpenAI login). We capture the catalog "generation"
/// at request time and wait for it to advance so the readiness line reports the
/// freshly-selected model rather than the stale pre-login default.
#[derive(Clone, Debug)]
pub(crate) struct OnboardingPendingValidation {
    /// Session the validation belongs to; stale requests are ignored.
    pub(crate) session_id: String,
    /// When the request was created, so we can give up after a short wait
    /// (and validate whatever default we have) rather than spinning forever.
    pub(crate) requested_at: Instant,
    /// Whether to wait for the server's post-login catalog refresh to land
    /// before firing (remote mode after a login).
    pub(crate) await_catalog_refresh: bool,
    /// Remote catalog generation observed when the request was created. The
    /// post-login refresh has landed once the live generation moves past this.
    pub(crate) catalog_generation_at_request: u64,
}

impl OnboardingPendingValidation {
    /// How long we will wait for the server to report a concrete model id
    /// before validating with the best default we currently have.
    const RESOLVE_TIMEOUT: Duration = Duration::from_secs(8);

    pub(crate) fn new(session_id: String) -> Self {
        Self {
            session_id,
            requested_at: Instant::now(),
            await_catalog_refresh: false,
            catalog_generation_at_request: 0,
        }
    }

    /// Variant that also waits for the remote catalog generation to advance
    /// past `catalog_generation` (the post-login refresh) before firing.
    pub(crate) fn awaiting_catalog_refresh(session_id: String, catalog_generation: u64) -> Self {
        Self {
            session_id,
            requested_at: Instant::now(),
            await_catalog_refresh: true,
            catalog_generation_at_request: catalog_generation,
        }
    }

    /// Whether we have waited long enough that we should validate now even if
    /// the model id has not been reported yet.
    pub(crate) fn resolve_timed_out(&self) -> bool {
        self.requested_at.elapsed() >= Self::RESOLVE_TIMEOUT
    }
}

/// Runtime state for the onboarding flow. `None`/`Done` means inactive.
#[derive(Clone, Debug)]
pub(crate) struct OnboardingFlow {
    pub(crate) phase: OnboardingPhase,
}

impl OnboardingFlow {
    /// Start the post-login flow. The app immediately advances this legacy
    /// phase to continue/suggestions so first-run onboarding no longer blocks on
    /// choosing a model.
    pub(crate) fn begin() -> Self {
        Self {
            phase: OnboardingPhase::ModelSelect,
        }
    }

    /// Start the flow at the login phase (no working credentials yet).
    /// `import` is the per-candidate import walkthrough when external logins
    /// were detected. When no logins were detected (`import` is `None`) we ask a
    /// simple "Log in to OpenAI?" Yes/No instead of dropping straight to the
    /// provider picker.
    pub(crate) fn begin_at_login(import: Option<ImportReview>) -> Self {
        let phase = match import {
            Some(review) => OnboardingPhase::Login {
                import: Some(review),
            },
            None => OnboardingPhase::LoginOpenAi {
                choice: crate::tui::OnboardingLoginChoice::OpenAi,
            },
        };
        Self { phase }
    }

    /// Whether the flow is actively driving the UI.
    pub(crate) fn is_active(&self) -> bool {
        !matches!(self.phase, OnboardingPhase::Done)
    }

    /// Seconds remaining on the longer [`DECISION_TIMEOUT`] yes/no phases
    /// (login import walkthrough, continue prompt), if one is active.
    pub(crate) fn decision_seconds_remaining(&self) -> Option<u64> {
        match &self.phase {
            OnboardingPhase::Login {
                import: Some(review),
            } => Some(review.seconds_remaining()),
            OnboardingPhase::ContinuePrompt { shown_at, .. } => Some(
                DECISION_TIMEOUT
                    .saturating_sub(shown_at.elapsed())
                    .as_secs(),
            ),
            _ => None,
        }
    }

    /// Whether a [`DECISION_TIMEOUT`] yes/no phase has elapsed and should
    /// auto-select its default.
    pub(crate) fn decision_timed_out(&self) -> bool {
        match &self.phase {
            OnboardingPhase::Login {
                import: Some(review),
            } => review.timed_out(),
            OnboardingPhase::ContinuePrompt { shown_at, .. } => {
                shown_at.elapsed() >= DECISION_TIMEOUT
            }
            _ => false,
        }
    }
}

/// Detect whether an external Codex, Claude Code, Pi, or OpenCode OAuth login
/// is present.
///
/// Returns every detected CLI (sandbox-aware), so the caller can choose which
/// one to offer (e.g. by most-recent activity). The order is Codex, Claude, Pi,
/// then OpenCode, but callers should not treat that as a preference.
pub(crate) fn detect_external_cli_oauths() -> Vec<ExternalCli> {
    let mut found = Vec::new();
    // Detection drives the first-run "continue where you left off" picker, whose
    // only requirement is that resumable transcripts exist. We therefore treat a
    // CLI as present when EITHER its OAuth login file exists OR it has written
    // transcripts. The transcript fallback matters because some tools store
    // credentials outside a plain JSON file (Claude Code and Cursor use the
    // macOS keychain / a vscdb), so an auth-file-only check would silently hide
    // sessions the user clearly has.
    if external_oauth_present(&external_home_path(".codex/auth.json"))
        || external_transcripts_present(&external_home_path(".codex/sessions"), "jsonl")
    {
        found.push(ExternalCli::Codex);
    }
    if external_oauth_present(&external_home_path(".claude/.credentials.json"))
        || external_transcripts_present(&external_home_path(".claude/projects"), "jsonl")
    {
        found.push(ExternalCli::ClaudeCode);
    }
    if external_oauth_present(&external_home_path(".pi/agent/auth.json"))
        || external_transcripts_present(&external_home_path(".pi/agent/sessions"), "jsonl")
    {
        found.push(ExternalCli::Pi);
    }
    if external_oauth_present(&external_home_path(".local/share/opencode/auth.json"))
        || external_transcripts_present(
            &external_home_path(".local/share/opencode/storage/session"),
            "json",
        )
    {
        found.push(ExternalCli::OpenCode);
    }
    // Cursor agent stores its credentials in a vscdb/keychain rather than a
    // plain JSON file, so the reliable "can we resume?" signal is the presence
    // of agent transcripts under ~/.cursor/projects. Fall back to the optional
    // auth.json when transcripts have not been written yet.
    if external_transcripts_present(&external_home_path(".cursor/projects"), "jsonl")
        || external_oauth_present(&external_home_path(".cursor/auth.json"))
        || external_oauth_present(&external_home_path(".config/cursor/auth.json"))
    {
        found.push(ExternalCli::Cursor);
    }
    found
}

/// Whether `root` contains at least one file with the given extension, searched
/// shallowly-recursively. Cheap directory walk used for resume detection.
fn external_transcripts_present(root: &std::path::Path, ext: &str) -> bool {
    fn walk(dir: &std::path::Path, ext: &str, budget: &mut u32) -> bool {
        if *budget == 0 {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            if *budget == 0 {
                return false;
            }
            *budget -= 1;
            let path = entry.path();
            if path.is_dir() {
                if walk(&path, ext, budget) {
                    return true;
                }
            } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
                return true;
            }
        }
        false
    }
    if !root.exists() {
        return false;
    }
    // Bound the walk so a pathological tree cannot stall onboarding.
    let mut budget = 20_000u32;
    walk(root, ext, &mut budget)
}

/// Resolve a path under the (sandbox-aware) external home so onboarding honors
/// `JCODE_HOME`/external isolation, matching the import detectors.
fn external_home_path(rel: &str) -> PathBuf {
    crate::storage::user_home_path(rel)
        .ok()
        .or_else(|| home_dir().map(|home| home.join(rel)))
        .unwrap_or_else(|| PathBuf::from(rel))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// A credentials file counts as an OAuth login when it exists and is non-empty.
fn external_oauth_present(path: &PathBuf) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() > 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_starts_at_model_select_and_is_active() {
        let flow = OnboardingFlow::begin();
        assert_eq!(flow.phase, OnboardingPhase::ModelSelect);
        assert!(flow.is_active());
    }

    #[test]
    fn done_phase_is_inactive() {
        let flow = OnboardingFlow {
            phase: OnboardingPhase::Done,
        };
        assert!(!flow.is_active());
    }

    #[test]
    fn continue_prompt_counts_down_and_times_out() {
        let past = Instant::now() - (DECISION_TIMEOUT + Duration::from_secs(1));
        let flow = OnboardingFlow {
            phase: OnboardingPhase::ContinuePrompt {
                cli: ExternalCli::Codex,
                yes_highlighted: true,
                shown_at: past,
            },
        };
        // The continue prompt now shares the longer DECISION_TIMEOUT with the
        // import and telemetry prompts (not the short AUTO_ADVANCE).
        assert_eq!(flow.decision_seconds_remaining(), Some(0));
        assert!(flow.decision_timed_out());
    }

    #[test]
    fn fresh_continue_prompt_has_remaining_time() {
        let flow = OnboardingFlow {
            phase: OnboardingPhase::ContinuePrompt {
                cli: ExternalCli::ClaudeCode,
                yes_highlighted: true,
                shown_at: Instant::now(),
            },
        };
        let remaining = flow.decision_seconds_remaining().unwrap();
        assert!(
            remaining >= DECISION_TIMEOUT.as_secs() - 2 && remaining <= DECISION_TIMEOUT.as_secs()
        );
        assert!(!flow.decision_timed_out());
    }

    #[test]
    fn external_oauth_present_requires_nonempty_file() {
        let dir = std::env::temp_dir().join(format!("jcode-onb-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let empty = dir.join("empty.json");
        let full = dir.join("full.json");
        std::fs::write(&empty, b"").unwrap();
        std::fs::write(&full, b"{\"token\":\"x\"}").unwrap();
        assert!(!external_oauth_present(&empty));
        assert!(external_oauth_present(&full));
        assert!(!external_oauth_present(&dir.join("missing.json")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_transcripts_present_finds_nested_files() {
        let dir = std::env::temp_dir().join(format!(
            "jcode-onb-transcripts-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let nested = dir.join("projects/demo/agent-transcripts/uuid");
        std::fs::create_dir_all(&nested).unwrap();
        // No matching files yet.
        assert!(!external_transcripts_present(&dir, "jsonl"));
        std::fs::write(nested.join("uuid.jsonl"), b"{}\n").unwrap();
        assert!(external_transcripts_present(&dir, "jsonl"));
        // A different extension should not match.
        assert!(!external_transcripts_present(&dir, "json"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
