#!/usr/bin/env python3
"""Assign the integration tests to shards by their measured duration, and say how good the split is.

    scripts/shard-plan.py --shard 1/2 --tests <(the-test-binary --ignored --list)
    scripts/shard-plan.py --plan --tests tests.list      # every shard's contents and projected cost

Prints one test name per line on stdout — exactly what `--exact` wants — and everything about *how well* the
split went on stderr, so a caller can pipe the names without also getting the report.

## Why by duration and not by name

TEST-29 (#106) named this as the design question sharding turns on, and #103 answered it with numbers: the
four `*_is_honest_from_every_suspended_state` tests are **29% of the suite's test time** between them, and the
slowest is **74 s** against a 158 s suite. A hash-of-the-name split has a 1-in-8 chance of putting all four in
one shard, and that shard *is* the workflow's wall clock. Splitting by measured duration is the difference
between sharding working and sharding looking like it worked on the run someone happened to check.

Greedy longest-processing-time: sort by duration descending, give each test to whichever shard is lightest so
far. It is the standard approximation for this and it is within 4/3 of optimal, which is far inside the noise
of a suite whose per-test durations move a few percent run to run.

## The 74 s floor is real and no assignment beats it

`resume_thread_is_honest_from_every_suspended_state` is one test; it cannot be split. So no shard is ever
shorter than 74 s of test time, and past two shards the extra runners buy very little — the report below
prints the floor next to the projection so that stays visible rather than being rediscovered.

## Staleness is reported, never guessed at silently

The timings file is a committed snapshot and the test list comes from the binary, so they drift apart the
moment anyone adds a test. Both directions are normal and both are reported: a test with no recorded
duration is charged the **median** and named, and a recorded duration for a test that no longer exists is
named too. What must not happen is a new slow test being charged nothing, landing in a shard silently, and
making one leg twice as long as the others for a reason nobody can see.
"""

import argparse
import statistics
import sys


def read_timings(path):
    """`seconds<TAB>name`, one per line. Comments and blanks ignored."""
    timings = {}
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            secs, _, name = line.partition("\t")
            try:
                timings[name.strip()] = float(secs)
            except ValueError:
                print(f"shard-plan: ignoring unparseable timings line: {line!r}", file=sys.stderr)
    return timings


def read_tests(path):
    """libtest's `--list` output: `name: test`, plus a trailing summary line this skips."""
    names = []
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            name, sep, kind = line.strip().partition(": ")
            if sep and kind == "test":
                names.append(name)
    return names


def assign(tests, timings, shards):
    """Greedy longest-processing-time. Returns (buckets, unknown, floor)."""
    known = [timings[t] for t in tests if t in timings]
    median = statistics.median(known) if known else 1.0
    unknown = sorted(t for t in tests if t not in timings)

    # Sorted by duration descending, then by name, so the plan is identical on every runner and every rerun.
    weighted = sorted(((timings.get(t, median), t) for t in tests), key=lambda p: (-p[0], p[1]))

    buckets = [[] for _ in range(shards)]
    loads = [0.0] * shards
    for secs, name in weighted:
        lightest = loads.index(min(loads))
        buckets[lightest].append(name)
        loads[lightest] += secs

    # The property that actually matters, checked rather than trusted: every test lands in exactly one shard.
    # A test in no shard has silently stopped running, and a green leg that tested less than it claimed is the
    # failure this repo has three separate guards against already. A test in two shards is cheaper — wasted
    # runner time — but it would make the projected costs above lies, so both are refused here.
    placed = [name for bucket in buckets for name in bucket]
    if sorted(placed) != sorted(tests):
        missing = sorted(set(tests) - set(placed))
        doubled = sorted({n for n in placed if placed.count(n) > 1})
        raise AssertionError(
            f"the shard plan is not a partition of the {len(tests)} tests: "
            f"{len(missing)} unassigned ({missing[:4]}), {len(doubled)} assigned twice ({doubled[:4]})"
        )

    floor = max((secs for secs, _ in weighted), default=0.0)
    return buckets, loads, unknown, median, floor


def report(buckets, loads, unknown, median, floor, timings, tests, shards):
    total = sum(loads)
    print(
        f"shard-plan: {len(tests)} tests over {shards} shards — {total:.0f}s of test time, "
        f"heaviest shard {max(loads):.0f}s, lightest {min(loads):.0f}s, single-test floor {floor:.0f}s",
        file=sys.stderr,
    )
    for index, (bucket, load) in enumerate(zip(buckets, loads), 1):
        print(f"shard-plan:   shard {index}/{shards}: {len(bucket)} tests, {load:.0f}s", file=sys.stderr)

    if max(loads) <= floor + 1.0:
        print(
            "shard-plan: the heaviest shard is at the single-test floor, so more shards cannot help — "
            "the slowest test is the wall clock now.",
            file=sys.stderr,
        )
    if unknown:
        shown = ", ".join(unknown[:6]) + (f" (and {len(unknown) - 6} more)" if len(unknown) > 6 else "")
        print(
            f"shard-plan: {len(unknown)} test(s) have no recorded duration and were charged the median "
            f"{median:.1f}s: {shown}. Refresh the timings file if any of them is slow.",
            file=sys.stderr,
        )
    stale = sorted(set(timings) - set(tests))
    if stale:
        print(
            f"shard-plan: {len(stale)} recorded duration(s) are for tests that no longer exist "
            f"({', '.join(stale[:4])}{'…' if len(stale) > 4 else ''}) — harmless, but the file is drifting.",
            file=sys.stderr,
        )


def main():
    ap = argparse.ArgumentParser(description="Assign tests to shards by measured duration.")
    ap.add_argument("--tests", required=True, help="file holding libtest `--list` output")
    ap.add_argument("--timings", default="mcp-server/tests/timings.tsv", help="committed durations")
    ap.add_argument("--shard", help="which shard to print, as N/M")
    ap.add_argument("--plan", action="store_true", help="print every shard instead of one")
    ap.add_argument("--shards", type=int, default=2, help="how many shards, when --shard is absent")
    args = ap.parse_args()

    if args.shard:
        index, _, count = args.shard.partition("/")
        try:
            index, shards = int(index), int(count)
        except ValueError:
            ap.error(f"--shard wants N/M, got {args.shard!r}")
        if not 1 <= index <= shards:
            ap.error(f"--shard {args.shard} is out of range")
    else:
        index, shards = None, args.shards

    tests = read_tests(args.tests)
    if not tests:
        print(f"shard-plan: no tests in {args.tests} — refusing to plan an empty run.", file=sys.stderr)
        return 1
    if shards > len(tests):
        print(
            f"shard-plan: {shards} shards for {len(tests)} tests would leave some empty, and an empty "
            "shard is a green run of nothing.",
            file=sys.stderr,
        )
        return 1

    try:
        timings = read_timings(args.timings)
    except OSError as why:
        print(f"shard-plan: cannot read {args.timings}: {why}", file=sys.stderr)
        print(
            "shard-plan: without durations every test weighs the same, which is the name-split this was "
            "written to avoid. Regenerate it with scripts/test-timings.py --emit-timings.",
            file=sys.stderr,
        )
        return 1

    buckets, loads, unknown, median, floor = assign(tests, timings, shards)
    report(buckets, loads, unknown, median, floor, timings, tests, shards)

    if args.plan:
        for number, bucket in enumerate(buckets, 1):
            for name in sorted(bucket):
                print(f"{number}\t{name}")
    elif index is None:
        ap.error("nothing to print: pass --shard N/M for one shard's names, or --plan for all of them")
    else:
        for name in sorted(buckets[index - 1]):
            print(name)
    return 0


if __name__ == "__main__":
    sys.exit(main())
