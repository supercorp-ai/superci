#!/usr/bin/env python3
"""Each test's time, kept to deal tests into parts of about the same length, and the dealing.

  test-times.py merge OLD.json JUNIT.xml...   each test's time (seconds) from nextest's JUnit reports, over OLD's
  test-times.py deal TIMES.json TESTS.txt N   TESTS (one name a line) dealt into N parts, longest first, each to the
                                              part with the least time so far; JSON, a list of N lists of names

A test's name is the one `cargo test -- --list` says (`dashboard::tests::x`, or `x` in an integration test); one in
several programs counts its longest time. A test not timed yet counts the average, so a new one is dealt somewhere
and timed by the next run. Sweeps are not dealt here: each part runs every sweep, with its slice of the faults.
"""
import json, sys, xml.etree.ElementTree as ET

def merge(old_path, reports):
    try: times = json.load(open(old_path))
    except (OSError, ValueError): times = {}
    fresh = {}
    for r in reports:
        try: root = ET.parse(r).getroot()
        except (OSError, ET.ParseError): continue
        for case in root.iter("testcase"):
            name, t = case.get("name", ""), float(case.get("time", "0") or 0)
            if name: fresh[name] = max(fresh.get(name, 0.0), t)
    times.update({k: round(v, 1) for k, v in fresh.items()})
    json.dump(dict(sorted(times.items())), sys.stdout, indent=0)

def deal(times_path, tests_path, n):
    try: times = json.load(open(times_path))
    except (OSError, ValueError): times = {}
    names = [l.strip() for l in open(tests_path) if l.strip()]
    known = [times[x] for x in names if x in times]
    average = sum(known) / len(known) if known else 1.0
    cost = {x: times.get(x, average) for x in names}
    parts, totals = [[] for _ in range(n)], [0.0] * n
    for x in sorted(names, key=lambda x: (-cost[x], x)):
        i = totals.index(min(totals))
        parts[i].append(x)
        totals[i] += cost[x]
    print(f"dealt {len(names)} tests ({len(known)} timed) into {n} parts of {min(totals):.0f}–{max(totals):.0f} s", file=sys.stderr)
    json.dump(parts, sys.stdout)

if __name__ == "__main__":
    if sys.argv[1:2] == ["merge"]: merge(sys.argv[2], sys.argv[3:])
    elif sys.argv[1:2] == ["deal"]: deal(sys.argv[2], sys.argv[3], int(sys.argv[4]))
    else: sys.exit(__doc__)
