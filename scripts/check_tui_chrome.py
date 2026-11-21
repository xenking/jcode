#!/usr/bin/env python3
"""Chrome width uses display cells, never Unicode scalar counts or scalar slicing."""
from pathlib import Path
import re

from guardrail_source import diagnostic, function_bodies, production_code, run, rust_sources

SCALAR_WIDTH = re.compile(r"\.\s*chars\s*\(\s*\)\s*\.\s*(?:count|take)\s*\(")
FIX = (
    "chrome is measured in display cells, not chars; use UnicodeWidthStr::width "
    "and jcode_tui_render::truncate_line_with_ellipsis_to_width "
    "(truncate_line_to_width for an existing fixed-cell bar)"
)


def violations(root: Path) -> list[str]:
    directory = root / "crates/jcode-tui/src/tui"
    errors = []
    checked = 0
    for path in rust_sources(directory):
        relative = path.relative_to(directory)
        code = production_code(path)
        if relative.parts[0].startswith(("ui_header", "info_widget")):
            regions = [(0, len(code))]
        elif relative.parts[0].startswith("ui_input"):
            regions = [(start, end) for name, start, end in function_bodies(code)
                       if name.startswith(("overscroll_", "compact_status_", "status_with_"))]
        else:
            continue
        checked += 1
        for start, end in regions:
            for match in SCALAR_WIDTH.finditer(code, start, end):
                errors.append(diagnostic(root, path, code, match.start(), FIX))
    if not checked:
        errors.append(f"{directory}: no chrome renderers found; restore them or update check_tui_chrome.py")
    return errors


if __name__ == "__main__":
    raise SystemExit(run(__doc__, violations))
