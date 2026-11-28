//! Verifiable, bounded text excerpts from snapcompact history.
use crate::message::{ContentBlock, Message, Role};
use aho_corasick::{AhoCorasick, AhoCorasickKind};
use jcode_compaction_core::snapcompact::{FrameRange, normalize};
use jcode_message_types::SnapcompactArchive;
use regex::Regex;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

#[cfg(test)]
#[path = "snap_index_tests.rs"]
mod tests;

const INDEX_LIMIT: usize = 120_000;
const BUCKETS: usize = 20;

fn interesting(line: &str, user: bool) -> bool {
    if user {
        return true;
    }
    static FACTS: OnceLock<Result<[Regex; 5], regex::Error>> = OnceLock::new();
    let facts = FACTS.get_or_init(|| (|| {
        Ok([
            Regex::new(r"(?i)\b[0-9a-f]{7,40}\b")?,
            Regex::new(r"(?i)(?:[~/]|[\w.-]+/)[\w./~-]+")?,
            Regex::new(r"(?i)\b[\w.-]+\s*[:=]\s*[^\s,;]+")?,
            Regex::new(r"(?i)\b\d+(?:\.\d+)?%?\b")?,
            Regex::new(r"(?i)\b(worker|decision|decided|because|reason|must|never|avoid|prefer|should|do not|don't|rule|task|result)\b")?,
        ])
    })().inspect_err(|error| crate::logging::warn(&format!("Invalid snap index fact matcher: {error}"))));
    thread_local! {
        static LOCAL: std::cell::OnceCell<Option<[Regex; 5]>> = const { std::cell::OnceCell::new() };
    }
    LOCAL.with(|local| {
        let Some([hex, path, assignment, number, word]) =
            local.get_or_init(|| facts.as_ref().ok().cloned())
        else {
            return true;
        };
        // Clones share the compiled automata, but give each gather worker its
        // own search caches instead of contending on a shared cache pool.
        (memchr::memchr2(b'/', b'~', line.as_bytes()).is_some() && path.is_match(line))
            || (memchr::memchr2(b':', b'=', line.as_bytes()).is_some() && assignment.is_match(line))
            || number.is_match(line)
            || word.is_match(line)
            || hex.is_match(line)
    })
}

fn excerpt(line: &str) -> &str {
    if line.len() <= 400 {
        return line;
    }
    let end = if line.as_bytes()[..400].is_ascii() {
        400
    } else {
        line.char_indices().nth(400).map_or(line.len(), |(i, _)| i)
    };
    &line[..end]
}

fn normalized_excerpt(excerpt: &str) -> Cow<'_, str> {
    let bytes = excerpt.as_bytes();
    if bytes.is_ascii()
        && !excerpt.ends_with(' ')
        && memchr::memchr3(b'\t', 0x0b, 0x0c, bytes).is_none()
        && memchr::memchr3(b'\r', b'\n', 0x1b, bytes).is_none()
        && !excerpt.contains("  ")
    {
        Cow::Borrowed(excerpt)
    } else {
        Cow::Owned(normalize(excerpt))
    }
}

struct IndexEntry {
    len: usize,
    message: usize,
    page: Option<u32>,
    role: &'static str,
    excerpt: String,
}

impl IndexEntry {
    fn new(message: usize, page: Option<u32>, role: &'static str, excerpt: String) -> Self {
        let message_digits = message.checked_ilog10().map_or(1, |n| n as usize + 1);
        let page_digits = page.map_or(7, |p| p.checked_ilog10().map_or(1, |n| n as usize + 1));
        let len = 22 + message_digits + page_digits + role.len() + excerpt.len();
        Self {
            len,
            message,
            page,
            role,
            excerpt,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn render(&self) -> String {
        format!(
            "[message {}, page {} | {}] {}\n",
            self.message,
            self.page
                .map_or_else(|| "UNKNOWN".into(), |p| p.to_string()),
            self.role,
            self.excerpt
        )
    }
}

struct RawExcerpt {
    message: usize,
    bucket: usize,
    priority: u8,
    role: &'static str,
    text: String,
    normalized: String,
}

fn collect_excerpts(messages: &[Message], offset: usize, total: usize) -> Vec<RawExcerpt> {
    let mut excerpts = Vec::new();
    let mut facts = HashMap::<String, bool>::new();
    for (local_index, message) in messages.iter().enumerate() {
        let i = offset + local_index;
        let mut pieces: Vec<(Cow<'_, str>, &str, bool)> = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text { text, .. } => {
                    let user = message.role == Role::User;
                    pieces.push((
                        Cow::Borrowed(text),
                        if user { "user" } else { "assistant" },
                        user,
                    ));
                }
                ContentBlock::ToolResult { content, .. } => {
                    pieces.push((Cow::Borrowed(content), "tool result", false))
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    pieces.push((Cow::Borrowed(name), "tool call", false));
                    if let Some(args) = input.as_object() {
                        for value in args.values() {
                            let text = value
                                .as_str()
                                .map(Cow::Borrowed)
                                .unwrap_or_else(|| Cow::Owned(value.to_string()));
                            pieces.push((text, "tool call", false));
                        }
                    }
                }
                _ => {}
            }
        }
        for (piece, role, user) in pieces {
            for line in piece.lines() {
                let line = line.trim();
                let keep = line.len() >= 5
                    && (user
                        || if let Some(&keep) = facts.get(line) {
                            keep
                        } else {
                            let keep = interesting(line, false);
                            facts.insert(line.to_owned(), keep);
                            keep
                        });
                if !keep {
                    continue;
                }
                let excerpt = excerpt(line).to_owned();
                let normalized = normalized_excerpt(&excerpt).into_owned();
                if normalized.len() < 5 {
                    continue;
                }
                let lower = excerpt.to_ascii_lowercase();
                let priority = if user
                    || [
                        "worker", "because", "decision", "decided", "commit", "must", "never",
                    ]
                    .iter()
                    .any(|s| lower.contains(s))
                {
                    0
                } else {
                    1
                };
                let bucket = (i.saturating_mul(BUCKETS) / total.max(1)).min(BUCKETS - 1);
                excerpts.push(RawExcerpt {
                    message: i,
                    bucket,
                    priority,
                    role,
                    text: excerpt,
                    normalized,
                });
            }
        }
    }
    excerpts
}

fn gather_excerpts(messages: &[Message]) -> Vec<RawExcerpt> {
    let workers = messages
        .len()
        .div_ceil(256)
        .min(8)
        .min(std::thread::available_parallelism().map_or(1, usize::from));
    if workers <= 1 {
        return collect_excerpts(messages, 0, messages.len());
    }
    let weights: Vec<usize> = messages
        .iter()
        .map(|m| {
            m.content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text, .. } => text.len(),
                    ContentBlock::ToolResult { content, .. } => content.len(),
                    ContentBlock::ToolUse { input, .. } => input.as_object().map_or(1, |args| {
                        args.values().map(|v| v.as_str().map_or(32, str::len)).sum()
                    }),
                    _ => 0,
                })
                .sum::<usize>()
                .max(1)
        })
        .collect();
    let target = weights.iter().sum::<usize>().div_ceil(workers);
    let mut chunks = Vec::new();
    let (mut start, mut weight) = (0, 0);
    for (i, bytes) in weights.into_iter().enumerate() {
        weight += bytes;
        if weight >= target && chunks.len() + 1 < workers {
            chunks.push(start..i + 1);
            start = i + 1;
            weight = 0;
        }
    }
    if start < messages.len() {
        chunks.push(start..messages.len());
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|range| {
                scope.spawn(move || {
                    collect_excerpts(&messages[range.clone()], range.start, messages.len())
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    })
}

struct PageLookup {
    // Byte offsets, in the original order so malformed legacy ranges retain
    // their first-containing-range semantics as well.
    ranges: Vec<(usize, usize, u32)>,
    disjoint: bool,
}

impl PageLookup {
    fn new(source: &str, ranges: &[FrameRange]) -> Self {
        let boundaries: Vec<_> = source
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(source.len()))
            .collect();
        let ranges: Vec<_> = ranges
            .iter()
            .map(|r| {
                (
                    boundaries
                        .get(r.source_start)
                        .copied()
                        .unwrap_or(source.len()),
                    boundaries
                        .get(r.source_end)
                        .copied()
                        .unwrap_or(source.len()),
                    r.index,
                )
            })
            .collect();
        let disjoint = ranges.iter().all(|r| r.0 <= r.1)
            && ranges.windows(2).all(|pair| pair[0].1 <= pair[1].0);
        Self { ranges, disjoint }
    }

    fn page(&self, start: usize, end: usize) -> Option<u32> {
        if self.disjoint {
            let i = self
                .ranges
                .partition_point(|r| r.0 <= start)
                .checked_sub(1)?;
            let r = self.ranges[i];
            (end <= r.1).then_some(r.2 + 1)
        } else {
            self.ranges
                .iter()
                .find(|r| start >= r.0 && end <= r.1)
                .map(|r| r.2 + 1)
        }
    }
}

#[derive(Clone, Copy, Default)]
enum Occurrence {
    #[default]
    Missing,
    Unique {
        start: usize,
        end: usize,
    },
    Ambiguous,
}

impl Occurrence {
    fn record(&mut self, start: usize, end: usize) {
        match *self {
            Self::Missing => *self = Self::Unique { start, end },
            // str::match_indices ignores overlapping occurrences of the same
            // pattern. Cross-pattern overlaps must still be reported by AC.
            Self::Unique { end: first_end, .. } if start >= first_end => *self = Self::Ambiguous,
            _ => {}
        }
    }
}

fn scan_patterns(
    source: &[u8],
    offset: usize,
    after: usize,
    ids: &[usize],
    patterns: &[String],
    occurrences: &mut [Occurrence],
) {
    if ids.is_empty() {
        return;
    }
    // Neighbor LCPs in lexical order give a distinguishing prefix for every
    // pattern. This avoids building hundreds of trie states per long excerpt.
    // Hits are verified against the complete bytes before being recorded.
    #[cfg(test)]
    let scan_started = std::time::Instant::now();
    let mut order: Vec<_> = (0..ids.len()).collect();
    order.sort_unstable_by(|&a, &b| patterns[ids[a]].cmp(&patterns[ids[b]]));
    let mut lengths: Vec<_> = ids.iter().map(|&i| patterns[i].len().min(8)).collect();
    for pair in order.windows(2) {
        let a = patterns[ids[pair[0]]].as_bytes();
        let b = patterns[ids[pair[1]]].as_bytes();
        let common = a.iter().zip(b).take_while(|(a, b)| a == b).count();
        for &i in pair {
            lengths[i] = lengths[i].max((common + 1).min(patterns[ids[i]].len()));
        }
    }
    let matcher = AhoCorasick::builder()
        .kind(Some(AhoCorasickKind::NoncontiguousNFA))
        .build(
            ids.iter()
                .enumerate()
                .map(|(i, &id)| &patterns[id].as_bytes()[..lengths[i]]),
        );
    #[cfg(test)]
    if std::env::var_os("JCODE_SNAP_INDEX_PROFILE").is_some() {
        eprintln!(
            "scan build {:?}, patterns={}, anchor_bytes={}",
            scan_started.elapsed(),
            ids.len(),
            lengths.iter().sum::<usize>()
        );
    }
    #[cfg(test)]
    let search_started = std::time::Instant::now();
    match matcher {
        Ok(matcher) => {
            for found in matcher.find_overlapping_iter(source) {
                let id = ids[found.pattern().as_usize()];
                let pattern = patterns[id].as_bytes();
                let local_end = found.start() + pattern.len();
                let end = offset + local_end;
                if end > after && source.get(found.start()..local_end) == Some(pattern) {
                    occurrences[id].record(offset + found.start(), end);
                }
            }
        }
        Err(error) => {
            crate::logging::warn(&format!(
                "Snap index matcher unavailable, using literal search: {error}"
            ));
            // Construction limits must not turn a verifiable page into UNKNOWN.
            for &i in ids {
                let pattern = patterns[i].as_bytes();
                for start in source
                    .windows(pattern.len())
                    .enumerate()
                    .filter_map(|(start, window)| (window == pattern).then_some(start))
                {
                    let end = offset + start + pattern.len();
                    if end > after {
                        occurrences[i].record(offset + start, end);
                    }
                    if matches!(occurrences[i], Occurrence::Ambiguous) {
                        break;
                    }
                }
            }
        }
    }
    #[cfg(test)]
    if std::env::var_os("JCODE_SNAP_INDEX_PROFILE").is_some() {
        eprintln!("scan search {:?}", search_started.elapsed());
    }
}

fn prefix_occurrences(
    previous: &SnapcompactArchive,
    source: &str,
    ranges: &[FrameRange],
    pages: &PageLookup,
    pattern_ids: &HashMap<String, usize>,
    occurrences: &mut [Occurrence],
) -> usize {
    if !pages.disjoint || previous.source.is_empty() || !source.starts_with(&previous.source) {
        return 0;
    }
    for line in previous.index.lines() {
        let Some((header, excerpt)) = line.split_once("] ") else {
            continue;
        };
        let Some((_, page)) = header
            .strip_prefix("[message ")
            .and_then(|h| h.split_once(", page "))
        else {
            continue;
        };
        let Some(page) = page
            .split_once(" | ")
            .and_then(|(p, _)| p.parse::<usize>().ok())
            .and_then(|p| p.checked_sub(1))
        else {
            continue;
        };
        let Some(&(start, end)) = previous.page_ranges.get(page) else {
            continue;
        };
        let Some((i, _)) = ranges.iter().enumerate().find(|(_, r)| {
            r.index as usize == page && r.source_start == start && r.source_end == end
        }) else {
            continue;
        };
        let (start, end, _) = pages.ranges[i];
        let Some(text) = previous.source.get(start..end) else {
            continue;
        };
        let normalized = normalize(excerpt);
        let Some(&id) = pattern_ids.get(&normalized) else {
            continue;
        };
        if let Some(pos) = text.find(&normalized) {
            occurrences[id] = Occurrence::Unique {
                start: start + pos,
                end: start + pos + normalized.len(),
            };
        }
    }
    previous.source.len()
}

/// Excerpts come from raw messages. A page is cited only when its normalized
/// text has an unambiguous occurrence in the rendered archive. Older pages can
/// contain an earlier summary instead of the raw message, so missing text must
/// remain in the index with an UNKNOWN page rather than being discarded.
pub(super) fn build_snapcompact_index(
    messages: &[Message],
    message_offset: usize,
    source: &str,
    ranges: &[FrameRange],
) -> String {
    build_snapcompact_index_with_previous(messages, message_offset, source, ranges, None)
}

pub(super) fn build_snapcompact_index_with_previous(
    messages: &[Message],
    message_offset: usize,
    source: &str,
    ranges: &[FrameRange],
    previous: Option<&SnapcompactArchive>,
) -> String {
    #[cfg(test)]
    let gather_started = std::time::Instant::now();
    let pages = PageLookup::new(source, ranges);
    let mut patterns = Vec::new();
    let mut pattern_ids = HashMap::new();
    let mut excerpts = Vec::new();
    for RawExcerpt {
        message,
        bucket,
        priority,
        role,
        text,
        normalized,
    } in gather_excerpts(messages)
    {
        let id = if let Some(&id) = pattern_ids.get(normalized.as_str()) {
            id
        } else {
            let id = patterns.len();
            pattern_ids.insert(normalized.clone(), id);
            patterns.push(normalized);
            id
        };
        excerpts.push((message, bucket, priority, role, text, id));
    }
    #[cfg(test)]
    if std::env::var_os("JCODE_SNAP_INDEX_PROFILE").is_some() {
        eprintln!(
            "gather {:?}, excerpts={}, patterns={}",
            gather_started.elapsed(),
            excerpts.len(),
            patterns.len()
        );
    }
    let mut occurrences = vec![Occurrence::Missing; patterns.len()];
    let prefix = previous.map_or(0, |previous| {
        prefix_occurrences(
            previous,
            source,
            ranges,
            &pages,
            &pattern_ids,
            &mut occurrences,
        )
    });
    let (cached, fresh): (Vec<_>, Vec<_>) =
        (0..patterns.len()).partition(|&i| matches!(occurrences[i], Occurrence::Unique { .. }));
    scan_patterns(source.as_bytes(), 0, 0, &fresh, &patterns, &mut occurrences);
    if !cached.is_empty() {
        let overlap = cached.iter().map(|&i| patterns[i].len()).max().unwrap_or(1) - 1;
        let start = prefix.saturating_sub(overlap);
        scan_patterns(
            &source.as_bytes()[start..],
            start,
            prefix,
            &cached,
            &patterns,
            &mut occurrences,
        );
    }
    let candidates: Vec<_> = excerpts
        .into_iter()
        .map(|(i, bucket, priority, role, excerpt, id)| {
            let page = match occurrences[id] {
                Occurrence::Unique { start, end } => pages.page(start, end),
                _ => None,
            };
            let entry = IndexEntry::new(message_offset + i, page, role, excerpt);
            (bucket, priority, entry)
        })
        .collect();
    let mut chosen = vec![false; candidates.len()];
    let mut used = 0usize;
    for bucket in 0..BUCKETS {
        let mut bucket_used = 0;
        for priority in 0..=1 {
            for (i, (b, p, entry)) in candidates.iter().enumerate() {
                if *b == bucket
                    && *p == priority
                    && bucket_used + entry.len() <= INDEX_LIMIT / BUCKETS
                {
                    chosen[i] = true;
                    bucket_used += entry.len();
                    used += entry.len();
                }
            }
        }
    }
    // Reuse unspent bucket budgets for sparse archives, still prioritizing rules.
    for priority in 0..=1 {
        for (i, (_, p, entry)) in candidates.iter().enumerate() {
            if !chosen[i] && *p == priority && used + entry.len() <= INDEX_LIMIT {
                chosen[i] = true;
                used += entry.len();
            }
        }
    }
    let omitted = chosen.iter().any(|selected| !selected);
    let mut out = String::with_capacity(used + 64);
    for ((_, _, entry), selected) in candidates.into_iter().zip(chosen) {
        if selected {
            out.push_str(&entry.render());
        }
    }
    if omitted {
        out.push_str("[Index truncated: absence is not evidence.]\n");
    }
    out
}
