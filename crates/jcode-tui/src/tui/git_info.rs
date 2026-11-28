//! Git repository state gathered for the info widget and cloud status probes.

/// Git repository status for the info widget
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitInfo {
    pub branch: String,
    pub modified: usize,
    pub staged: usize,
    pub untracked: usize,
    pub ahead: usize,
    pub behind: usize,
    /// First few dirty paths with their porcelain status (capped).
    pub dirty_files: Vec<DirtyFile>,
    /// Total number of dirty paths, including those beyond the cap.
    pub dirty_total: usize,
    /// Lines added across all dirty files (text files only).
    pub added_total: usize,
    /// Lines removed across all dirty files (text files only).
    pub removed_total: usize,
    /// Absolute repository root, used to match agent-edited paths.
    pub repo_root: Option<std::path::PathBuf>,
    /// Most recent commits on HEAD, newest first.
    pub recent_commits: Vec<RecentCommit>,
}

/// One commit for the Commits widget.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecentCommit {
    /// Abbreviated hash.
    pub hash: String,
    pub subject: String,
    /// Committer time, seconds since the Unix epoch.
    pub timestamp: i64,
    /// Not yet on the upstream branch.
    pub unpushed: bool,
    pub added: Option<usize>,
    pub removed: Option<usize>,
}

/// One dirty path from `git status --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DirtyFile {
    /// Single-letter status shown in the Changes widget: `M`, `A`, `D`, `R`,
    /// `U` (conflict), or `?` (untracked).
    pub status: char,
    pub path: String,
    /// Lines added, `None` for binary or unknown.
    pub added: Option<usize>,
    /// Lines removed, `None` for binary or unknown.
    pub removed: Option<usize>,
    /// Last modification time, used for newest-first ordering.
    pub modified_at: Option<std::time::SystemTime>,
}

impl DirtyFile {
    pub fn new(status: char, path: impl Into<String>) -> Self {
        Self {
            status,
            path: path.into(),
            ..Default::default()
        }
    }

    pub fn with_lines(mut self, added: usize, removed: usize) -> Self {
        self.added = Some(added);
        self.removed = Some(removed);
        self
    }
}

impl GitInfo {
    pub fn is_interesting(&self) -> bool {
        self.modified > 0
            || self.staged > 0
            || self.untracked > 0
            || self.ahead > 0
            || self.behind > 0
    }
}
