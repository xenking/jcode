#!/usr/bin/env python3
from pathlib import Path
import tempfile
import unittest

from check_usage_bar_owner import violations


class UsageOwnerTests(unittest.TestCase):
    def check(self, sources):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, source in sources.items():
                path = root / "crates/jcode-tui/src/tui" / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(source)
            return violations(root)

    def test_new_module_cannot_copy_a_quota_bar(self):
        errors = self.check({"future.rs": 'fn bar() { "█".repeat(filled); }'})
        self.assertEqual(len(errors), 1)
        self.assertIn("future.rs:1:", errors[0])
        self.assertIn("info_widget::LimitBar", errors[0])

    def test_remaining_bar_and_push_loop_are_rejected(self):
        self.assertEqual(len(self.check({"ui_header/account.rs": """
fn remaining_bar() { "▰".repeat(left); '▱'.repeat(empty); }
fn build() { bar.push('█'); bar.push_str("░"); }
"""})), 4)

    def test_canonical_owners_may_construct_their_different_glyphs(self):
        self.assertEqual(self.check({
            "info_widget_usage.rs": 'fn bar() { "▰".repeat(filled); }',
            "progress_bar.rs": "fn pulse() { bar.push('█'); }",
        }), [])

    def test_quota_surfaces_cannot_bypass_owner_using_progress(self):
        errors = self.check({"ui_header/future.rs": "fn render() { progress_bar::spans(); }"})
        self.assertEqual(len(errors), 1)
        self.assertIn("not the progress-only primitive", errors[0])

    def test_alias_and_new_quota_function_cannot_hide_progress_usage(self):
        self.assertEqual(len(self.check({"ui_header.rs": "use crate::tui::progress_bar as custom;"})), 1)
        self.assertEqual(len(self.check({"future.rs": "fn render_quota() { progress_bar::spans(); }"})), 1)

    def test_legacy_report_parsing_and_shared_rendering_are_allowed(self):
        self.assertEqual(self.check({"ui_messages/usage.rs": """
fn usage() { raw.find(['█', '░']); info_widget::LimitBar { used_pct }.render(); }
"""}), [])

    def test_comments_raw_strings_and_test_fixtures_do_not_construct_bars(self):
        self.assertEqual(self.check({"ui_header.rs": """
// "█".repeat(4)
/* bar.push('░'); */
const DOC: &str = r##"example: "█".repeat(4)"##;
#[cfg(test)]
mod fixture { fn old_bar() { "█".repeat(4); } }
fn real() { shared(); }
"""}), [])

    def test_checkout_name_cannot_hide_production_files(self):
        with tempfile.TemporaryDirectory(prefix="tests_context_") as directory:
            root = Path(directory)
            source = root / "crates/jcode-tui/src/tui/future.rs"
            source.parent.mkdir(parents=True)
            source.write_text('fn quota() { "█".repeat(3); }')
            self.assertEqual(len(violations(root)), 1)

    def test_shared_progress_is_allowed_outside_quota_surfaces(self):
        self.assertEqual(self.check({"ui_messages.rs": "fn progress() { progress_bar::spans(); }"}), [])


if __name__ == "__main__":
    unittest.main()
