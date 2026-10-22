#!/usr/bin/env python3
"""Alternate saved release binaries; retain raw trials and require exact hashes."""
import argparse
import json
import platform
import re
import statistics
import subprocess
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--before', required=True)
parser.add_argument('--after', required=True)
parser.add_argument('--rounds', type=int, default=5)
parser.add_argument('--output', required=True)
parser.add_argument('arguments', nargs=argparse.REMAINDER)
args = parser.parse_args()
trials = {'before': [], 'after': []}
for iteration in range(args.rounds):
    for label in (('before', 'after') if iteration % 2 == 0 else ('after', 'before')):
        output = subprocess.check_output([getattr(args, label), *args.arguments], text=True)
        values = dict(re.findall(r'(\w+)=([^\s]+)', output.splitlines()[-1]))
        rss = re.search(r'VmHWM:\s+(\d+) kB', output)
        trials[label].append({
            **{key: float(values[key]) for key in ('median_ms', 'min_ms', 'max_ms')},
            **{key: int(values[key]) for key in ('iterations', 'allocations', 'requested_bytes')},
            'checksum': values['checksum'], 'peak_rss_kib': int(rss[1]) if rss else None,
            'workload': output.splitlines()[0],
        })
checksums = {t['checksum'] for values in trials.values() for t in values}
assert len(checksums) == 1, f'output changed: {checksums}'
summary = {label: {key: statistics.median(t[key] for t in values)
                   for key in ('median_ms', 'allocations', 'requested_bytes', 'peak_rss_kib')
                   if all(t[key] is not None for t in values)}
           for label, values in trials.items()}
report = {'system': platform.platform(), 'before_binary': args.before,
          'after_binary': args.after, 'arguments': args.arguments,
          'trials': trials, 'summary': summary}
Path(args.output).write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps(summary))
