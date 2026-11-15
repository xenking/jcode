//! Same-command variable resolution.
//!
//! The path layer treats an unexpanded `$VAR` as unknowable, which is correct in
//! general but wrong for the shell idiom that names a scratch file once and uses
//! it later:
//!
//! ```text
//! R=$HOME/.jcode/scratch/cm-check; cargo test > $R/out.ndjson 2> $R/err.log
//! for w in gro sar; do rm -rf ~/.jcode/wt/$w/target; done
//! ```
//!
//! Nothing here guesses. A variable is only resolved when this same command line
//! assigns it a literal value (or a value built from variables that are already
//! known). Anything read from the environment, from a command substitution, or
//! from a glob stays unresolved and therefore stays at the Confirm tier.
//!
//! Values are kept as shell words, not as resolved paths, because the shell
//! splices them into a larger word: `for w in gro` used as `~/.jcode/wt/$w`
//! means `~/.jcode/wt/gro`, not `<cwd>/gro` glued onto a prefix.

use std::collections::BTreeMap;

use super::{RiskFinding, RiskLevel, Token, WRAPPER_COMMANDS};

/// Variables whose value the guard supplies itself. Reassigning one is handled
/// by a separate finding, and never by resolution, so `HOME=/etc` cannot make
/// `$HOME` expand to `/etc` in the trusted path layer.
pub(crate) const TRUSTED_VARS: &[&str] = &["HOME", "JCODE_SCRATCH_DIR"];

/// Words that may precede an assignment without making it an argument.
const ASSIGNMENT_PREFIXES: &[&str] = &["export", "declare", "local", "readonly", "typeset"];

/// Expansion caps. Exceeding either keeps the token unresolved, which escalates
/// instead of silently dropping a value: a dropped loop iteration could hide a
/// protected path.
const MAX_VARIANTS: usize = 64;
/// One segment cannot produce more shell words than this. Beyond it the whole
/// segment is assessed unresolved (Confirm) rather than risking a truncated set.
const MAX_SEGMENT_VARIANTS: usize = 256;
const MAX_RESOLUTION_DEPTH: usize = 8;

/// Values assigned on this command line, in source order.
#[derive(Debug, Default, Clone)]
pub(crate) struct VarEnv {
    values: BTreeMap<String, Vec<String>>,
}

impl VarEnv {
    /// Read every same-command assignment and `for NAME in ...` list. Only the
    /// tests need the whole-line view; the assessor folds segments in order.
    #[cfg(test)]
    pub(crate) fn collect(segments: &[Vec<Token>]) -> Self {
        let mut env = Self::default();
        for segment in segments {
            env.absorb(segment);
        }
        env
    }

    /// Fold one segment's assignments into the environment. Callers that assess
    /// segments in source order should do this just before expanding the
    /// segment, so a variable used *before* it is assigned stays unresolved.
    pub(crate) fn absorb(&mut self, segment: &[Token]) {
        self.collect_loops(segment);
        self.collect_assignments(segment);
    }

    fn collect_loops(&mut self, segment: &[Token]) {
        for (index, token) in segment.iter().enumerate() {
            if token.text != "for" {
                continue;
            }
            let Some(name) = segment.get(index + 1).map(|t| t.text.as_str()) else {
                continue;
            };
            if !is_identifier(name) || TRUSTED_VARS.contains(&name) {
                continue;
            }
            if segment.get(index + 2).map(|t| t.text.as_str()) != Some("in") {
                continue;
            }
            let values: Vec<&str> = segment[index + 3..]
                .iter()
                .take_while(|t| !t.is_operator && t.text != "do")
                .map(|t| t.text.as_str())
                .collect();
            // `for w in $(ls)`, `for w in ~/wt/*`: the iteration set is decided
            // at runtime, so the variable stays unknown.
            if values.is_empty()
                || values
                    .iter()
                    .any(|value| value.contains(['$', '`', '*', '?']))
            {
                continue;
            }
            let values = values.iter().map(|value| value.to_string()).collect();
            self.record(name, values);
        }
    }

    fn collect_assignments(&mut self, segment: &[Token]) {
        for (index, token) in segment.iter().enumerate() {
            if !assignment_position(segment, index) {
                continue;
            }
            let Some((name, value)) = split_assignment(&token.text) else {
                continue;
            };
            if TRUSTED_VARS.contains(&name.as_str()) {
                continue;
            }
            self.record(&name, vec![value.to_string()]);
        }
    }

    fn record(&mut self, name: &str, raw_values: Vec<String>) {
        for raw in raw_values {
            let mut value = raw;
            for _ in 0..MAX_RESOLUTION_DEPTH {
                match self.substitute(&value) {
                    Some(next) if next != value => value = next,
                    _ => break,
                }
            }
            let entry = self.values.entry(name.to_string()).or_default();
            if entry.len() < MAX_VARIANTS && !entry.contains(&value) {
                entry.push(value);
            }
        }
    }

    /// Replace every known `$NAME`/`${NAME}` in `text` with each possible value.
    /// Returns `None` when the shell word has too many combinations to trust.
    fn substitute(&self, text: &str) -> Option<String> {
        let mut expanded = self.expand_text(text)?;
        if expanded.len() != 1 {
            return None;
        }
        expanded.pop()
    }

    /// Every token variant this segment can produce once known variables are
    /// spliced in.
    pub(crate) fn expand_segment(&self, segment: &[Token]) -> Vec<Vec<Token>> {
        if self.values.is_empty() {
            return vec![segment.to_vec()];
        }
        let mut variants: Vec<Vec<Token>> = vec![Vec::with_capacity(segment.len())];
        for token in segment {
            // Too many combinations keep the word unresolved, so the path layer
            // escalates rather than guessing.
            let options = match self.expand_text(&token.text) {
                Some(options) => options,
                None => vec![token.text.clone()],
            };
            let mut next = Vec::new();
            for prefix in &variants {
                for option in &options {
                    let mut variant = prefix.clone();
                    variant.push(Token {
                        text: option.clone(),
                        ..token.clone()
                    });
                    next.push(variant);
                }
            }
            variants = next;
            if variants.len() > MAX_SEGMENT_VARIANTS {
                // Too many combinations to be worth enumerating; assess the raw
                // segment so unresolved variables stay guarded.
                return vec![segment.to_vec()];
            }
        }
        variants
    }

    fn expand_text(&self, text: &str) -> Option<Vec<String>> {
        if !text.contains('$') {
            return Some(vec![text.to_string()]);
        }
        let mut results = vec![String::new()];
        let mut rest = text;
        while let Some(dollar) = rest.find('$') {
            let head = &rest[..dollar];
            for result in &mut results {
                result.push_str(head);
            }
            let tail = &rest[dollar..];
            match parse_var(tail).and_then(|(name, end)| {
                let values = self.values.get(&name)?;
                Some((values.clone(), end))
            }) {
                Some((values, end)) => {
                    if results.len().saturating_mul(values.len()) > MAX_VARIANTS {
                        return None;
                    }
                    let mut next = Vec::with_capacity(results.len() * values.len());
                    for prefix in &results {
                        for value in &values {
                            next.push(format!("{prefix}{value}"));
                        }
                    }
                    results = next;
                    rest = &tail[end..];
                }
                None => {
                    // Unknown variable: copy the `$` and keep scanning, so
                    // `$KNOWN/$UNKNOWN` still resolves the known half.
                    for result in &mut results {
                        result.push('$');
                    }
                    rest = &tail[1..];
                }
            }
        }
        for result in &mut results {
            result.push_str(rest);
        }
        Some(results)
    }
}

/// Findings for assignments of the variables the guard trusts. Kept position
/// aware: `grep 'JCODE_HOME='` and `dd if=/dev/zero` mention a path variable but
/// assign nothing, and `ps eww -p $pid | grep HOME=` is read-only.
pub(crate) fn reassignment_findings(segment: &[Token]) -> Vec<RiskFinding> {
    let mut findings = Vec::new();
    for (index, token) in segment.iter().enumerate() {
        if !assignment_position(segment, index) {
            continue;
        }
        let Some((name, _)) = split_assignment(&token.text) else {
            continue;
        };
        if !TRUSTED_VARS.contains(&name.as_str()) {
            continue;
        }
        findings.push(RiskFinding {
            level: RiskLevel::Confirm,
            reason: format!(
                "`{}` reassigns `{name}`, the value this guard uses to expand paths, so the \
                 trusted expansion no longer describes the shell that will run; pass the path \
                 literally instead or delete a narrower target",
                token.text
            ),
            target: Some(token.text.clone()),
        });
    }
    findings
}

/// An assignment word only counts at the start of a command, optionally behind
/// `export` or a wrapper such as `env`/`sudo`. That is what separates
/// `HOME=/etc cmd` from `grep 'HOME='` and `dd of=/dev/sda`.
fn assignment_position(segment: &[Token], index: usize) -> bool {
    segment[..index].iter().all(|token| {
        is_assignment_word(&token.text)
            || WRAPPER_COMMANDS.contains(&token.basename().as_str())
            || ASSIGNMENT_PREFIXES.contains(&token.basename().as_str())
            || token.is_flag()
    })
}

fn is_assignment_word(text: &str) -> bool {
    split_assignment(text).is_some()
}

/// Exposed so the assessor can skip `X=1 rm -rf ...` prefixes and see `rm`.
pub(crate) fn starts_with_assignment(tokens: &[Token]) -> bool {
    tokens
        .first()
        .is_some_and(|token| is_assignment_word(&token.text))
}

fn split_assignment(text: &str) -> Option<(String, &str)> {
    let (name, value) = text.split_once('=')?;
    // `--opt=value` and `dd of=...` are options/operands, not assignments.
    if !is_identifier(name) {
        return None;
    }
    Some((name.to_string(), value))
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse `$NAME` or `${NAME}` at the start of `text`, returning the name and the
/// number of bytes consumed.
fn parse_var(text: &str) -> Option<(String, usize)> {
    let rest = text.strip_prefix('$')?;
    if let Some(body) = rest.strip_prefix('{') {
        let end = body.find('}')?;
        let name = &body[..end];
        return is_identifier(name).then(|| (name.to_string(), end + 3));
    }
    let end = rest
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map(|(index, _)| index)
        .unwrap_or(rest.len());
    let name = &rest[..end];
    is_identifier(name).then(|| (name.to_string(), end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(command: &str) -> Vec<Vec<Token>> {
        crate::tokenize::split_segments(command)
    }

    #[test]
    fn assignment_values_are_spliced_as_words_not_as_paths() {
        let segments = tokens("for w in gro sar; do rm -rf ~/.jcode/wt/$w/target; done");
        let env = VarEnv::collect(&segments);
        let expanded = env.expand_segment(&segments[1]);
        let texts: Vec<String> = expanded
            .iter()
            .flat_map(|variant| variant.iter().map(|t| t.text.clone()))
            .collect();
        assert!(
            texts.iter().any(|t| t == "~/.jcode/wt/gro/target"),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "~/.jcode/wt/sar/target"),
            "{texts:?}"
        );
    }

    #[test]
    fn unknown_values_stay_unresolved() {
        for command in [
            "for w in $(ls); do rm -rf $w; done",
            "X=$UNKNOWN; rm -rf $X",
            "for w in ~/wt/*; do rm -rf $w; done",
        ] {
            let segments = tokens(command);
            let env = VarEnv::collect(&segments);
            let unresolved = segments.iter().any(|segment| {
                env.expand_segment(segment)
                    .iter()
                    .flatten()
                    .any(|token| token.text.contains('$'))
            });
            assert!(unresolved, "{command}");
        }
    }

    #[test]
    fn trusted_variables_are_never_resolved_from_an_assignment() {
        let segments = tokens("HOME=/etc; rm -rf $HOME/cache");
        let env = VarEnv::collect(&segments);
        assert!(env.values.get("HOME").is_none());
        assert!(!reassignment_findings(&segments[0]).is_empty());
    }
}
