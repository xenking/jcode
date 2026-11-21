#!/usr/bin/env python3
"""Check the lexical lifetime of swarm registry guards in app-core Rust sources.

Registry guards must not cross an await except to acquire a later registry.
Use an explicit block or drop(guard) to make the lifetime visible to this check.
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
SRC = ROOT / "crates/jcode-app-core/src"
ORDER = {"swarm_members": 0, "members": 0, "swarms_by_id": 1,
         "swarm_plans": 2, "plans": 2, "swarm_coordinators": 3, "coordinators": 3}
NAMES = "|".join(ORDER)
ACQUIRE = re.compile(rf"\b({NAMES})\s*\.\s*(?:read|write)\s*\(\s*\)\s*\.\s*await\b")
BIND = re.compile(r"\blet\s+(?:mut\s+)?(\w+)(?:\s*:[^=;{}]+)?\s*=\s*(?:[\w.&]+\s*\.\s*)?$")
DROP = re.compile(r"\bdrop\s*\(\s*(\w+)\s*\)")
TOKEN = re.compile(r'//[^\n]*|/\*[\s\S]*?\*/|"(?:\\.|[^"\\])*"|\b(?:r#+"[\s\S]*?"#+)|[{};]|\.\s*await\b')


def check(path, text):
    # Mask comments and strings but retain offsets and line numbers.
    def mask(match):
        value = match.group()
        if value.startswith(("//", "/*", '"', 'r#')):
            return ''.join('\n' if c == '\n' else ' ' for c in value)
        return value
    code = TOKEN.sub(mask, text)
    # Test modules intentionally construct lock contention schedules.
    for module in reversed(list(re.finditer(r'#\[cfg\(test\)\]\s*mod\s+\w+\s*\{', code))):
        level = 1
        end = module.end()
        while end < len(code) and level:
            level += (code[end] == '{') - (code[end] == '}')
            end += 1
        code = code[:module.start()] + re.sub(r'[^\n]', ' ', code[module.start():end]) + code[end:]
    guards = []  # (name, rank, lexical depth, acquisition line)
    depth = 0
    start = 0
    errors = []
    for token in TOKEN.finditer(code):
        value = token.group()
        if value.startswith(('//', '/*', '"', 'r#')):
            continue
        if value.startswith('.'):
            before = code[start:token.end()]
            line = code.count('\n', 0, token.start()) + 1
            acquire = ACQUIRE.search(before)
            rank = ORDER[acquire.group(1)] if acquire else None
            for name, held_rank, _, held_line in guards:
                if rank is None or rank <= held_rank:
                    errors.append(f'{path}:{line}: await while holding {name} (line {held_line})' +
                                  (f', acquiring {acquire.group(1)} out of order' if acquire else ''))
            if acquire:
                prefix = before[:acquire.start()]
                binding = BIND.search(prefix)
                # Only a bare acquisition is a guard; chained access snapshots.
                suffix = code[token.end():].lstrip()
                if binding and suffix.startswith(';'):
                    guards.append((binding.group(1), rank, depth, line))
                elif re.search(r'\b(?:if|while)\s+let\b|\bmatch\b', prefix):
                    # Rust extends temporaries in a pattern scrutinee through
                    # the body, even when the expression clones the value.
                    next_brace = suffix.find('{')
                    next_semi = suffix.find(';')
                    if next_brace >= 0 and (next_semi < 0 or next_brace < next_semi):
                        guards.append((f'temporary {acquire.group(1)}', rank, depth + 1, line))
        elif value == '{':
            depth += 1
            start = token.end()
        elif value == '}':
            depth -= 1
            guards = [guard for guard in guards if guard[2] <= depth]
            start = token.end()
        else:
            for match in DROP.finditer(code[start:token.end()]):
                guards = [guard for guard in guards if guard[0] != match.group(1)]
            start = token.end()
    return errors


def main():
    failures = []
    for path in sorted(SRC.rglob('*.rs')):
        if any('test' in part for part in path.relative_to(SRC).parts):
            continue
        failures.extend(check(path.relative_to(ROOT), path.read_text()))
    if failures:
        print('\n'.join(failures), file=sys.stderr)
        return 1
    print('swarm registry lock order: OK')
    return 0


if __name__ == '__main__':
    sys.exit(main())
