use super::ToolContext;
use sha2::{Digest, Sha256};
use std::{fmt::Write as _, io::Read as _, ops::Range, path::Path, sync::LazyLock};

const FILE_LIMIT: usize = 1024 * 1024;
const MAX_ANCHORS: usize = 8;
static INSTANCE: LazyLock<uuid::Uuid> = LazyLock::new(uuid::Uuid::new_v4);

pub(super) fn enabled() -> bool {
    crate::config::config().tools.line_anchors
}

pub(super) fn direct(ctx: &ToolContext) -> bool {
    ctx.root_invocation_id() == ctx.tool_call_id && enabled()
}

fn version(path: &Path, session: &str, bytes: &[u8]) -> Option<Sha256> {
    let canonical = path.canonicalize().ok()?;
    let mut key = Sha256::new();
    key.update(INSTANCE.as_bytes());
    for field in [
        session.as_bytes(),
        canonical.as_os_str().as_encoded_bytes(),
        bytes,
    ] {
        key.update((field.len() as u64).to_be_bytes());
        key.update(field);
    }
    Some(key)
}

fn token(key: &Sha256, line: usize) -> String {
    let mut state = key.clone();
    state.update((line as u64).to_be_bytes());
    let digest = state.finalize();
    let mut tag = format!("L{line}-");
    for b in &digest[..12] {
        write!(tag, "{b:02x}").unwrap();
    }
    tag
}

/// Print the complete source row beside every issued token. Never derive an edit
/// range from a shortened search excerpt, a path-only result or an outline.
pub(super) fn append<'a>(
    out: &mut String,
    path: &Path,
    ctx: &ToolContext,
    rows: impl Iterator<Item = (usize, &'a str)>,
) {
    if !direct(ctx) {
        return;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    if !file
        .metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= FILE_LIMIT as u64)
    {
        return;
    }
    let mut body = String::new();
    if file
        .take((FILE_LIMIT + 1) as u64)
        .read_to_string(&mut body)
        .is_err()
        || body.len() > FILE_LIMIT
    {
        return;
    }
    let Some(key) = version(path, &ctx.session_id, body.as_bytes()) else {
        return;
    };
    // Bound both scanning work and emitted metadata for large result sets.
    for (line, shown) in rows.take(MAX_ANCHORS) {
        if line > 0 && shown.len() <= 512 && body.lines().nth(line - 1) == Some(shown) {
            let _ = writeln!(out, "\n[edit anchor {}] {shown}", token(&key, line));
        }
    }
}

pub(super) fn resolve(
    path: &Path,
    session: &str,
    content: &str,
    tag: &str,
) -> anyhow::Result<(usize, Range<usize>)> {
    anyhow::ensure!(content.len() <= FILE_LIMIT, "Anchor stale: reread the file");
    let line = tag
        .strip_prefix('L')
        .and_then(|s| s.split_once('-'))
        .and_then(|(n, _)| n.parse::<usize>().ok())
        .filter(|n| *n > 0);
    let err = || {
        anyhow::anyhow!(
            "Invalid or stale edit anchor: read or search the file again. No changes written."
        )
    };
    let line = line.ok_or_else(err)?;
    let key = version(path, session, content.as_bytes()).ok_or_else(err)?;
    anyhow::ensure!(token(&key, line) == tag, "{}", err());
    let mut pos = 0;
    for (i, row) in content.split_inclusive('\n').enumerate() {
        if i + 1 == line {
            let text = row
                .strip_suffix("\r\n")
                .or_else(|| row.strip_suffix('\n'))
                .unwrap_or(row);
            return Ok((line, pos..pos + text.len()));
        }
        pos += row.len();
    }
    Err(err())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn anchor_binds_duplicate_line_to_file_session_and_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a");
        let other = dir.path().join("b");
        let s = "same\r\nsame\r\n";
        std::fs::write(&path, s).unwrap();
        std::fs::write(&other, s).unwrap();
        let tag = token(&version(&path, "s", s.as_bytes()).unwrap(), 2);
        assert_eq!(resolve(&path, "s", s, &tag).unwrap(), (2, 6..10));
        assert!(resolve(&other, "s", s, &tag).is_err());
        assert!(resolve(&path, "other", s, &tag).is_err());
        assert!(resolve(&path, "s", "same\r\nsame\r\nnew", &tag).is_err());
        assert!(resolve(&path, "s", s, &tag.replacen("L2", "L1", 1)).is_err());
    }
}
