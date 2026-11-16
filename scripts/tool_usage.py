#!/usr/bin/env python3
"""Read-only count of tool calls in saved sessions over a rolling window.

Snapshots and append journals overlap. Messages are deduplicated by session and
message ID. Counts are calls, not shell subprocesses or nested codemode calls.
"""
import argparse
import collections
import datetime as dt
import json
import pathlib
import re
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--sessions', type=pathlib.Path, default=pathlib.Path.home() / '.jcode/sessions')
    parser.add_argument('--hours', type=float, default=24)
    args = parser.parse_args()
    cutoff = dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=args.hours)
    calls = collections.Counter()
    sessions = collections.defaultdict(set)
    commands = collections.Counter()
    seen = set()
    errors = 0

    def consume(session, messages):
        for message in messages:
            key = (session, message.get('id'))
            if key in seen or key[1] is None:
                continue
            seen.add(key)
            try:
                timestamp = dt.datetime.fromisoformat(message['timestamp'].replace('Z', '+00:00'))
                if timestamp < cutoff:
                    continue
            except (KeyError, ValueError, TypeError):
                continue
            content = message.get('content', [])
            if not isinstance(content, list):
                continue
            for block in content:
                if not isinstance(block, dict) or block.get('type') != 'tool_use':
                    continue
                name = block.get('name', '').removeprefix('functions.')
                calls[name] += 1
                sessions[name].add(session)
                if name == 'bash':
                    command = block.get('input', {}).get('command', '')
                    if isinstance(command, str):
                        for category, pattern in (
                            ('grep', r'\bgrep\b'), ('sed -n', r'\bsed\s+-n\b'),
                            ('rg', r'\brg\b'), ('cat', r'\bcat\b'), ('find', r'\bfind\b'),
                        ):
                            if re.search(pattern, command):
                                commands[category] += 1

    for path in sorted(args.sessions.glob('session_*.json')):
        try:
            data = json.loads(path.read_text())
            consume(data.get('id', path.stem), data.get('messages', []))
        except (OSError, ValueError, TypeError) as exc:
            errors += 1
            print(f'warning: {path.name}: {exc}', file=sys.stderr)
    for path in sorted(args.sessions.glob('session_*.journal.jsonl')):
        session = path.name.removesuffix('.journal.jsonl')
        try:
            with path.open() as stream:
                for line in stream:
                    try:
                        event = json.loads(line)
                        consume(session, event.get('append_messages', []))
                    except (ValueError, TypeError):
                        errors += 1
        except OSError as exc:
            errors += 1
            print(f'warning: {path.name}: {exc}', file=sys.stderr)
    print(f'Window: last {args.hours:g} hours; {len(set.union(*sessions.values())) if sessions else 0} sessions with calls')
    print('Tool          Calls  Sessions')
    for name in ('bash', 'agentgrep', 'codemode', 'read'):
        print(f'{name:10} {calls[name]:8} {len(sessions[name]):9}')
    print('Bash commands containing (categories may overlap):')
    for name in ('grep', 'sed -n', 'rg', 'cat', 'find'):
        print(f'  {name:8} {commands[name]:8}')
    if errors:
        print(f'Warnings: {errors} unreadable or malformed records', file=sys.stderr)


if __name__ == '__main__':
    main()
