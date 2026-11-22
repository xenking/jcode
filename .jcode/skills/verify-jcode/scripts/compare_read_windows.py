#!/usr/bin/env python3
"""Compare warm first-window reads in two private daemons via the provider boundary."""
import argparse
import contextlib
import datetime
import json
import runpy
import re
import statistics
import subprocess
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before', type=Path)
    parser.add_argument('after', type=Path)
    parser.add_argument('evidence', type=Path)
    parser.add_argument('--mib', type=int, choices=(1, 32, 128), default=32)
    args = parser.parse_args()
    args.evidence.mkdir(mode=0o700, parents=True, exist_ok=False)
    runner = Path(__file__).with_name('verify-jcode')
    harness = runpy.run_path(str(runner))['Harness']
    seats = {}
    times = {name: [] for name in ('before', 'after')}
    execution = {name: [] for name in times}
    with contextlib.ExitStack() as cleanup:
        for name in times:
            folder = args.evidence / name
            subprocess.run([runner, 'launch', '--evidence', folder, '--binary', getattr(args, name)], check=True)
            cleanup.callback(subprocess.run, [runner, 'cleanup', '--evidence', folder], check=True)
            seat = seats[name] = harness(folder)
            block = (b'fixture line 01\n' * 65536)  # exactly 1 MiB
            with (seat.root / 'workspace/verify-large.txt').open('wb') as target:
                for _ in range(args.mib):
                    target.write(block)
        for i in range(11):  # one warmup per arm, then ten pairs in alternating order
            for name in (list(times) if i % 2 == 0 else list(reversed(times))):
                seat = seats[name]
                offset = len(seat.records())
                label = f'window-{i}-{name}'
                seat.cli(label, label + ' [e2e:read-window]')
                requests = [r for r in seat.records()[offset:] if r['agent'] and label in json.dumps(r['messages'])]
                assert len(requests) == 2, (label, len(requests))
                results = [m for m in requests[-1]['messages'] if m.get('tool_call_id') == 'call_window']
                assert len(results) == 1 and 'fixture line 01' in str(results[0]['content']), results
                if i:
                    timing = re.search(r'tool timing: start=(\S+) finish=(\S+)', str(results[0]['content']))
                    assert timing, results
                    begin, end = [datetime.datetime.fromisoformat(s.replace('Z', '+00:00')) for s in timing.groups()]
                    execution[name].append((end - begin).total_seconds() * 1000)
                    times[name].append(1000 * (requests[1]['time'] - requests[0]['time']))
    report = {'fixture_mib': args.mib, 'metric': 'ms between provider request and next request containing tool result',
              'registry_execution_ms': execution, 'registry_median_ms': {k: statistics.median(v) for k, v in execution.items()},
              'warm_samples_ms': times, 'median_ms': {k: statistics.median(v) for k, v in times.items()}}
    (args.evidence / 'comparison.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report['median_ms']))


if __name__ == '__main__':
    main()
