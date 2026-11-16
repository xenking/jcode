#!/usr/bin/env python3
"""Regression tests for cell-width chrome enforcement."""
from pathlib import Path
import tempfile
import unittest

from check_tui_chrome import violations
from guardrail_source import function_bodies


class ChromeGateTests(unittest.TestCase):
    def check(self, sources):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, content in sources.items():
                path = root / "crates/jcode-tui/src/tui" / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content)
            return violations(root)

    def test_scalar_width_in_new_widget_is_rejected_with_fix_and_line(self):
        errors = self.check({"info_widget_future.rs": "fn render() {\n let w = label.chars().count();\n}"})
        self.assertEqual(len(errors), 1)
        self.assertIn("info_widget_future.rs:2:", errors[0])
        self.assertIn("UnicodeWidthStr::width", errors[0])

    def test_multiline_scalar_slicing_is_rejected(self):
        errors = self.check({"ui_header.rs": "fn fit() { label\n . chars ( )\n . take (width).collect() }"})
        self.assertEqual(len(errors), 1)
        self.assertIn("truncate_line_with_ellipsis_to_width", errors[0])

    def test_real_overscroll_error_is_rejected_but_text_metrics_are_not(self):
        errors = self.check({"ui_input.rs": """
fn input_prompt_len() { input.chars().count() }
fn overscroll_truncate_label(label: &str, max_chars: usize) -> String {
    if label.chars().count() <= max_chars { return label.to_string(); }
    label.chars().take(max_chars).collect()
}
"""})
        self.assertEqual(len(errors), 2)

    def test_unicode_width_and_existing_shared_truncator_pass(self):
        self.assertEqual(self.check({"ui_header.rs": """
fn fit() {
 let n = UnicodeWidthStr::width(label);
 truncate_line_with_ellipsis_to_width(&line, n)
}
"""}), [])

    def test_literals_nested_comments_lifetimes_and_test_helpers_are_ignored(self):
        self.assertEqual(self.check({"ui_header.rs": '''
// label.chars().count()
/* nested /* x.chars().take(width) */ x.chars().count() */
const EXAMPLE: &str = r##"x.chars().count() {"##;
#[cfg(test)]
fn sample<'a>(s: &'a str) { s.chars().take(3); }
fn fit<'a>(s: &'a str) { s.width(); }
'''}), [])

    def test_differently_named_external_test_modules_are_not_linted(self):
        self.assertEqual(self.check({
            "ui_header.rs": '#[cfg(test)]\n#[path = "info_widget_fixture.rs"]\nmod fixture;\nfn fit() {}',
            "info_widget_fixture.rs": 'fn fixture() { s.chars().count(); }',
        }), [])

    def test_unrelated_char_metrics_in_prose_and_input_are_not_linted(self):
        self.assertEqual(self.check({
            "ui_header.rs": "fn render() {}",
            "ui_messages.rs": "fn render_prose() { s.chars().count(); }",
            "ui_input.rs": "fn input_prompt_len() { input.chars().count() }",
        }), [])

    def test_function_body_extraction_handles_nested_blocks(self):
        source = "fn one() { if ok { call(); } } fn two() { 2 }"
        self.assertEqual([(name, source[a:b].strip()) for name, a, b in function_bodies(source)],
                         [("one", "if ok { call(); }"), ("two", "2")])

    def test_missing_source_tree_cannot_silently_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(FileNotFoundError):
                violations(Path(directory))


if __name__ == "__main__":
    unittest.main()
