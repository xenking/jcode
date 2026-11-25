//! Parsers for `git status --porcelain` and `git diff --numstat` output.

/// Parse `git diff --numstat` into path -> (added, removed). Binary files
/// report `-` and map to `None` counts.
pub(crate) fn parse_numstat(
    text: &str,
) -> std::collections::HashMap<String, (Option<usize>, Option<usize>)> {
    let mut out = std::collections::HashMap::new();
    for line in text.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(a), Some(r), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        // Renames: `old => new` or `dir/{old => new}/file`.
        let path = if let (Some(open), Some(close)) = (path.find('{'), path.find('}')) {
            let inner = &path[open + 1..close];
            let new = inner.rsplit(" => ").next().unwrap_or(inner);
            format!("{}{}{}", &path[..open], new, &path[close + 1..]).replace("//", "/")
        } else {
            path.rsplit(" => ").next().unwrap_or(path).to_string()
        };
        out.insert(path, (a.parse().ok(), r.parse().ok()));
    }
    out
}

/// Collapse a porcelain `XY` pair into the single letter the Changes widget
/// shows. Conflicts win, then the worktree side (what the user is editing),
/// then the index side.
pub(crate) fn porcelain_status_letter(index: u8, worktree: u8) -> char {
    if index == b'?' {
        return '?';
    }
    if index == b'U' || worktree == b'U' || (index == b'A' && worktree == b'A') {
        return 'U';
    }
    let pick = if worktree != b' ' { worktree } else { index };
    match pick {
        b'M' | b'T' => 'M',
        b'A' => 'A',
        b'D' => 'D',
        b'R' | b'C' => 'R',
        _ => 'M',
    }
}
