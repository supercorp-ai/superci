#!/usr/bin/env python3
"""Each test's time, kept to deal tests into parts of about the same length, and the dealing.

  test-times.py merge OLD.json JUNIT.xml...   each test's time (seconds) from nextest's JUnit reports, over OLD's
  test-times.py deal TIMES.json TESTS.txt N   TESTS (a line each: its program, a tab, its name) dealt into N parts,
                                              longest first, each to the part with the least time so far, every part
                                              with a test of every program; JSON, a list of N lists of names

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
    # Each line: the test program, a tab, the test's name (or the name alone).
    rows = [l.rstrip("\n").split("\t") for l in open(tests_path) if l.strip()]
    rows = [(r[0], r[1]) if len(r) > 1 else ("", r[0]) for r in rows]
    names = sorted({x for _, x in rows})
    known = [times[x] for x in names if x in times]
    average = sum(known) / len(known) if known else 1.0
    # (A tenth of a second at least: the quick tests, most of them unit tests that take nothing, are spread over the
    # parts as they come, not all dealt to whichever part is shortest when they come, as nothing would change it.)
    cost = {x: max(times.get(x, average), 0.1) for x in names}
    parts, totals = [[] for _ in range(n)], [0.0] * n
    for x in sorted(names, key=lambda x: (-cost[x], x)):
        i = totals.index(min(totals))
        parts[i].append(x)
        totals[i] += cost[x]
    # Every part runs at least one test of every test program (its quickest, again where need be): a part measures the
    # code of the programs it runs, and the parts' runs merge only when each measured the same.
    programs = {}
    for prog, x in rows: programs.setdefault(prog, []).append(x)
    added = 0
    for prog, tests in sorted(programs.items()):
        quickest = min(tests, key=lambda x: (cost[x], x))
        for i, part in enumerate(parts):
            if not any(x in part for x in tests): part.append(quickest); totals[i] += cost[quickest]; added += 1
    print(f"dealt {len(names)} tests ({len(known)} timed) into {n} parts of {min(totals):.0f}–{max(totals):.0f} s ({added} quickest tests again, for every program in every part)", file=sys.stderr)
    json.dump(parts, sys.stdout)

if __name__ == "__main__":
    if sys.argv[1:2] == ["merge"]: merge(sys.argv[2], sys.argv[3:])
    elif sys.argv[1:2] == ["deal"]: deal(sys.argv[2], sys.argv[3], int(sys.argv[4]))
    else: sys.exit(__doc__)
