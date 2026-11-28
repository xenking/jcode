#!/usr/bin/env python3
"""Only quota and progress primitives construct TUI bar glyphs."""
from pathlib import Path
import re

from guardrail_source import diagnostic, function_bodies, production_code, run, rust_sources

QUOTA = Path("info_widget_usage.rs")
PROGRESS = Path("progress_bar.rs")
# Parsing a legacy text report with find/contains is not constructing a gauge.
BUILD = re.compile(r"""(?P<literal>"[█░▰▱]+"|'[█░▰▱]')\s*\.\s*(?P<method>repeat)\s*\(|\.\s*(?P<push>push(?:_str)?)\s*\(\s*["'][█░▰▱]+["']""")
QUOTA_SURFACE = re.compile(r"(?:ui_header|info_widget|usage)")
FIX = "use info_widget::LimitBar/render_inline_limit_bar for quota gauges; progress-only gauges belong in tui::progress_bar, never a copied bar builder"


def violations(root: Path) -> list[str]:
    directory = root / "crates/jcode-tui/src/tui"
    errors = []
    for path in rust_sources(directory):
        relative = path.relative_to(directory)
        if relative in (QUOTA, PROGRESS):
            continue
        raw = path.read_text()
        code = production_code(path)
        for match in BUILD.finditer(raw):
            # Literal contents are masked by the existing safety lexer. The
            # method token must survive, excluding comments and test fixtures.
            group = "method" if match["method"] else "push"
            at = match.start(group)
            if code[at:match.end(group)] == match[group]:
                errors.append(diagnostic(root, path, code, at, FIX))
        if QUOTA_SURFACE.search(str(relative)):
            regions = [(0, len(code))]
        else:
            regions = [(start, end) for name, start, end in function_bodies(code)
                       if re.search(r"usage|quota|limit_bar|remaining_bar", name)
                       or re.search(r"\b(?:used_pct|left_pct|utilization)\b", code[start:end])]
        for start, end in regions:
            for match in re.compile(r"\bprogress_bar\b").finditer(code, start, end):
                errors.append(diagnostic(root, path, code, match.start(),
                    "quota surfaces must use info_widget::LimitBar, not the progress-only primitive"))
    return errors


if __name__ == "__main__":
    raise SystemExit(run(__doc__, violations))
