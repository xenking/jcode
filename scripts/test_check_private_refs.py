#!/usr/bin/env python3
from pathlib import Path
import subprocess
import tempfile
import unittest

import check_private_refs as gate

HARNESS = "o" + "mp"
OTHER = "oh-my-" + "pi"


class PrivateRefTests(unittest.TestCase):
    def check(self, files, allow=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name, body in files.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(body)
            subprocess.run(["git", "-C", directory, "init", "-q"], check=True)
            subprocess.run(["git", "-C", directory, "add", "-A"], check=True)
            saved = dict(gate.ALLOWLIST)
            gate.ALLOWLIST.update(allow or {})
            try:
                return gate.violations(root)
            finally:
                gate.ALLOWLIST.clear()
                gate.ALLOWLIST.update(saved)

    def test_comment_naming_the_harness_is_rejected(self):
        errors = self.check({"src/lib.rs": f"// parity with {HARNESS} behaviour\n"})
        self.assertEqual(len(errors), 1)
        self.assertIn("src/lib.rs:1:", errors[0])

    def test_repo_name_and_file_name_are_rejected(self):
        errors = self.check({f"docs/{HARNESS.upper()}_PARITY.md": f"see {OTHER}\n"})
        self.assertEqual(len(errors), 2)

    def test_ordinary_words_that_contain_the_letters_pass(self):
        self.assertEqual(self.check({"a.rs": "compact compile company\n"}), [])

    def test_allowlisted_alias_is_skipped(self):
        files = {"cfg.rs": f'#[serde(alias = "{HARNESS}_key")]\n'}
        self.assertEqual(len(self.check(files)), 1)
        self.assertEqual(self.check(files, {"cfg.rs": "old config name stays loadable"}), [])


if __name__ == "__main__":
    unittest.main()
