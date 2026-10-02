"""Merges recorder output into one sorted, de-duplicated fixture (see README.md).

Usage: merge.py SRC_DIR DST [--by-signature]
--by-signature groups records of the same raw signature so gzip can share them.
"""
import json, pathlib, sys

src, dst = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
by_signature = '--by-signature' in sys.argv[3:]
lines = set()
dropped = []
for path in sorted(src.glob('*.jsonl')):
    for line in path.read_text().splitlines():
        if len(line) > 64 * 1024 or '"non_utf8":true' in line:
            reason = 'non-UTF-8' if '"non_utf8":true' in line else f'{len(line)} bytes'
            dropped.append(f"{json.loads(line)['fn']} ({reason})")
            continue
        lines.add(line)


def key(line):
    record = json.loads(line)
    if by_signature:
        given = record['in']
        raw = given.get('raw') if isinstance(given, dict) else given
        return (raw if isinstance(raw, str) else '', record['fn'], line)
    return (record['fn'], line)


records = sorted(lines, key=key)
dst.write_text('\n'.join(records) + '\n')
print(f'{len(records)} records, {len(dropped)} dropped (over 64 KiB or non-UTF-8)')
for item in sorted(dropped):
    print('  dropped', item)
