#!/usr/bin/env python3
"""Test counts for the sharded Linux test job in ci.yml.

    test-counts.py shard <n> <nextest list JSON> <test log>   > counts-<n>.json
    test-counts.py total <counts-*.json>...

`shard` reads `cargo nextest list --partition hash:<n>/<shards> --message-format json` and
the shard's log, checks that nextest ran exactly the tests the list assigns to the shard,
and writes its counts. `total` checks that the shards are 1 to N with the same full list
and that their tests add up to every test in it that is not ignored, then prints the totals
for the whole suite in the same form as before sharding: nextest's tests and cargo test's
result lines (doc tests and the vendored gjson tests), with ignored tests counted as ignored.
"""
import json
import re
import sys


def fail(message):
    print(f"::error::{message}")
    sys.exit(1)


def shard(number, list_path, log_path):
    listed = json.load(open(list_path))
    total = ignored = mine = 0
    for suite in listed["rust-suites"].values():
        for case in suite["testcases"].values():
            total += 1
            ignored += case["ignored"]
            mine += case["filter-match"]["status"] == "matches"
    log = re.sub(r"\x1b\[[0-9;]*m", "", open(log_path, errors="replace").read())
    summaries = re.findall(r"^ *Summary \[.*$", log, re.MULTILINE)
    if len(summaries) != 1:
        fail(f"shard {number}: expected one nextest summary line, found {len(summaries)}")
    words = dict((w, int(n)) for n, w in re.findall(r"(\d+) (tests run|passed|failed)", summaries[0]))
    if words.get("tests run") != mine:
        fail(f"shard {number}: nextest ran {words.get('tests run')} tests, its partition lists {mine}")
    passed, failed, cargo_ignored = words.get("passed", 0), words.get("failed", 0), 0
    for p, f, i in re.findall(r"^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored", log, re.MULTILINE):
        passed, failed, cargo_ignored = passed + int(p), failed + int(f), cargo_ignored + int(i)
    json.dump({"shard": number, "listed": total, "listed_ignored": ignored, "ran": mine,
               "passed": passed, "failed": failed, "cargo_ignored": cargo_ignored}, sys.stdout)
    print()


def totals(paths):
    shards = []
    for path in paths:
        text = open(path).read().strip()
        if not text:
            fail(f"{path}: no counts from that shard")
        shards.append(json.loads(text))
    shards.sort(key=lambda s: s["shard"])
    if [s["shard"] for s in shards] != list(range(1, len(shards) + 1)):
        fail(f"expected counts from shards 1 to {len(shards)}, got {[s['shard'] for s in shards]}")
    listed = {(s["listed"], s["listed_ignored"]) for s in shards}
    if len(listed) != 1:
        fail(f"the shards listed different test sets: {sorted(listed)}")
    (total, ignored), = listed
    ran = sum(s["ran"] for s in shards)
    print(f"nextest: {total} tests listed, {ignored} ignored; shards ran "
          + " + ".join(str(s["ran"]) for s in shards) + f" = {ran}")
    if ran != total - ignored:
        fail(f"the shards ran {ran} tests, the list has {total - ignored} that are not ignored")
    passed = sum(s["passed"] for s in shards)
    failed = sum(s["failed"] for s in shards)
    print(f"{passed} passed, {failed} failed, {ignored + sum(s['cargo_ignored'] for s in shards)} ignored")


if __name__ == "__main__":
    if sys.argv[1:2] == ["shard"] and len(sys.argv) == 5:
        shard(int(sys.argv[2]), sys.argv[3], sys.argv[4])
    elif sys.argv[1:2] == ["total"] and len(sys.argv) > 2:
        totals(sys.argv[2:])
    else:
        sys.exit(__doc__)
