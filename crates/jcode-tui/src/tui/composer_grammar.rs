//! What the composer's text means before it is submitted: shell escape, slash command token, suggestion window.

use jcode_slash_command as slash_command_parser;

pub(crate) const COMMAND_SUGGESTION_VISIBLE_LIMIT: usize = 8;

pub(crate) fn extract_input_shell_command(input: &str) -> Option<&str> {
    input.trim().strip_prefix('!').map(str::trim)
}

pub(crate) fn has_safe_slash_command_token(input: &str) -> bool {
    slash_command_parser::active_token_before_cursor(input, input.len()).is_some()
}
