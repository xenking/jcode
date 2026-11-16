//! Boundary-aware slash-command parsing shared by submission and completion.
//!
//! A slash is only command syntax when it starts a known command token at the
//! beginning of input or after whitespace. This deliberately avoids treating
//! URLs, filesystem paths, prose, quoted text, and code as commands.

/// Byte offsets of a known command token in the original input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    /// Inclusive start of the slash token.
    pub start: usize,
    /// Exclusive end of the command name.
    pub name_end: usize,
}

/// Find known slash tokens, using names that include their leading slash.
pub fn find_known_commands(input: &str, names: &[&str]) -> Vec<Match> {
    let mut matches = Vec::new();
    scan_slash_tokens(input, |start, end| {
        let token = &input[start..end];
        if names.contains(&token) {
            matches.push(Match {
                start,
                name_end: end,
            });
        }
    });
    matches
}

/// Build the dispatch text for each match returned by `find_known_commands`.
/// Text preceding the first command becomes its first argument.
///
/// # Panics
///
/// May panic if matches are not ordered valid byte spans in this input.
pub fn split_command_sequence(input: &str, matches: &[Match]) -> Vec<String> {
    let Some(first) = matches.first() else {
        return Vec::new();
    };
    let prefix = input[..first.start].trim();
    matches
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let end = matches
                .get(index + 1)
                .map_or(input.len(), |next| next.start);
            let args = input[command.name_end..end].trim();
            if index == 0 && !prefix.is_empty() {
                if args.is_empty() {
                    format!("{} {}", &input[command.start..command.name_end], prefix)
                } else {
                    format!(
                        "{} {} {}",
                        &input[command.start..command.name_end],
                        prefix,
                        args
                    )
                }
            } else {
                input[command.start..end].trim().to_string()
            }
        })
        .collect()
}

/// Return the byte span of the slash token ending at the byte cursor, if any.
/// A cursor beyond the input is clamped to its end. A cursor inside a UTF-8
/// code point returns `None`.
pub fn active_token_before_cursor(input: &str, cursor: usize) -> Option<(usize, usize)> {
    let cursor = cursor.min(input.len());
    if !input.is_char_boundary(cursor) {
        return None;
    }

    let mut active = None;
    scan_slash_tokens(&input[..cursor], |start, end| {
        if end == cursor {
            active = Some((start, end));
        }
    });
    active
}

fn scan_slash_tokens(input: &str, mut on_token: impl FnMut(usize, usize)) {
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_backticks = false;
    let mut in_fenced_code = false;
    let mut escaped = false;
    let mut line_start = true;

    let mut iter = input.char_indices().peekable();
    while let Some((index, ch)) = iter.next() {
        if in_fenced_code {
            if line_start && ch == '`' && input[index..].starts_with("```") {
                iter.next();
                iter.next();
                in_fenced_code = false;
                line_start = false;
                continue;
            }
            line_start = ch == '\n';
            continue;
        }

        if escaped {
            escaped = false;
            line_start = ch == '\n';
            continue;
        }
        if ch == '\\' {
            escaped = true;
            line_start = false;
            continue;
        }

        if !in_single_quote && !in_double_quote && ch == '`' {
            if line_start && input[index..].starts_with("```") {
                iter.next();
                iter.next();
                in_fenced_code = true;
                line_start = false;
                continue;
            }
            in_backticks = !in_backticks;
            line_start = false;
            continue;
        }
        if in_backticks {
            line_start = ch == '\n';
            continue;
        }
        if !in_double_quote && ch == '\'' {
            in_single_quote = !in_single_quote;
            line_start = false;
            continue;
        }
        if !in_single_quote && ch == '"' {
            in_double_quote = !in_double_quote;
            line_start = false;
            continue;
        }

        if !in_single_quote
            && !in_double_quote
            && ch == '/'
            && (index == 0
                || input[..index]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace))
        {
            let end = input[index..]
                .char_indices()
                .find_map(|(offset, value)| value.is_whitespace().then_some(index + offset))
                .unwrap_or(input.len());
            on_token(index, end);
            line_start = false;
            continue;
        }

        line_start = ch == '\n';
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMANDS: &[&str] = &["/plan", "/review", "/help"];

    #[test]
    fn finds_known_commands_at_start_and_after_whitespace() {
        assert_eq!(
            find_known_commands("test /plan now /review", COMMANDS),
            vec![
                Match {
                    start: 5,
                    name_end: 10
                },
                Match {
                    start: 15,
                    name_end: 22
                },
            ]
        );
    }

    #[test]
    fn ignores_urls_paths_unknown_commands_and_code() {
        assert!(find_known_commands("https://example.com /tmp /unknown", COMMANDS).is_empty());
        assert!(find_known_commands("`/plan` ```\n/review\n```", COMMANDS).is_empty());
    }

    #[test]
    fn finds_active_partial_token_for_completion() {
        assert_eq!(active_token_before_cursor("text /pla", 9), Some((5, 9)));
        assert_eq!(active_token_before_cursor("https://pla", 11), None);
    }

    #[test]
    fn splits_prefix_and_multiple_commands_in_order() {
        let names = &["/poteto-mode", "/review"];
        let input = "fix this /poteto-mode please";
        assert_eq!(
            split_command_sequence(input, &find_known_commands(input, names)),
            ["/poteto-mode fix this please"]
        );
        let input = "fix this /poteto-mode please /review now";
        assert_eq!(
            split_command_sequence(input, &find_known_commands(input, names)),
            ["/poteto-mode fix this please", "/review now"]
        );
        let input = "fix this /poteto-mode /review";
        assert_eq!(
            split_command_sequence(input, &find_known_commands(input, names)),
            ["/poteto-mode fix this", "/review"]
        );
        assert!(split_command_sequence(input, &[]).is_empty());
    }

    #[test]
    fn ignores_non_commands_even_when_their_parts_are_known() {
        let names = &["/x", "/usr", "/b"];
        for input in [
            "https://a/b",
            "/usr/bin/env",
            "`code /x`",
            "quoted \"/x\"",
            "quoted '/x'",
            "```\n/x\n```",
            "/foo",
        ] {
            assert!(find_known_commands(input, names).is_empty(), "{input}");
        }
    }

    #[test]
    fn active_token_uses_cursor_position_in_mid_text() {
        let input = "fix /poteto-mode please";
        assert_eq!(active_token_before_cursor(input, 8), Some((4, 8)));
        assert_eq!(active_token_before_cursor(input, input.len()), None);
        assert_eq!(active_token_before_cursor("é /x", 1), None);
    }
}
