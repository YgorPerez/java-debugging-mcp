#!/usr/bin/env python3
"""Rank the slowest tests in a libtest log, so a claim about test speed can be checked against numbers.

    scripts/integration-test.sh                          # prints the ranking at the end of every run
    scripts/test-timings.py integration.log              # or rank a log you already have
    scripts/test-timings.py --markdown integration.log >> "$GITHUB_STEP_SUMMARY"
    cargo test 2>&1 | scripts/test-timings.py -          # stdin, for the unit layer

Writes to stdout and **never fails the run**: a log with no timings in it produces a report saying so and
exits 0. This is an instrument, not a gate. Failing the suite because the stopwatch broke would trade a
measurement for a red build, and the three guards in `scripts/integration-test.sh` — no JDK, nothing
selected, no `JDK in use:` line — are the things that are allowed to fail a run.

## Why this exists

TEST-26 (#103). The integration suite reported exactly one number — ~180 s under `taskset -c 0-3`, which is
CI's 4-vCPU shape — and nothing said *which* tests were in it. Every speed proposal in the tracker therefore
rested on a guess, and the guess that got measured first was wrong by about 4x: per-test `javac` runs were
estimated as the dominant cost and turned out to be ~4% (TEST-28, #105). `CLAUDE.md` already records two
flake investigations that reasoned backwards about contention before anyone measured. So this file exists
before the optimisations do, and the optimisations are expected to quote it.

## What the two headline numbers mean, because they are easy to swap

**Test time** is the sum of every test's own duration. It is occupancy, not wall clock: the suite runs
tests concurrently, so this number is larger than the run took and is the right one for "how much work is
there".

**Wall clock** is libtest's own `finished in …`, which is what a maintainer waits for. The ratio between
them is the effective concurrency, and it is the number TEST-29 (#106) needs in order to answer whether
sharding buys anything: sharding divides test time, and it cannot touch per-leg fixed cost.

Neither number includes the Rust build, the JDK install, or the cache restore. Those are per-leg fixed
cost, they are TEST-27's (#104) subject, and a report that folded them in would make the suite look slower
and the fixed cost invisible — which is backwards, since the fixed cost is the part that was duplicated
three times.

## Why libtest's `--report-time` and not cargo-nextest

Recorded at length in `docs/adr/0024-per-test-timings-come-from-libtest.md`, because nextest is the obvious
suggestion and it costs more here than it looks. The short form: nextest schedules a process per test, and
changing how many probe JVMs contend at once is precisely the variable this repo has already misread twice
— #103 puts a scheduling change out of scope for that reason, and there are four open flakes (#45, #56,
#64, #71) that a timing change landing in the same breath would make unreadable.

## The parse is deliberately loose, and it accounts for what it missed

The suite runs with `--nocapture`, so a test's own stdout interleaves with libtest's result lines and a
result line can arrive with another test's output stuck to the front of it. The pattern below therefore
allows a prefix. That is a small risk of counting a line that merely looks like a result, and a much
smaller one than dropping real results silently — so every result line that carried no duration is counted
and reported rather than skipped. If that count is not zero, the ranking is incomplete and says so.
"""

import argparse
import re
import sys

# `ok`/`FAILED`/`ignored` with an optional `<1.234s>` from --report-time. The `.*?` prefix is the
# --nocapture interleaving described in the module docstring; `$` still anchors the tail, so a result line
# cannot be matched out of the middle of a sentence.
RESULT = re.compile(
    r"^.*?\btest (?P<name>[A-Za-z0-9_:]+) \.\.\. (?P<verdict>ok|FAILED|ignored)"
    r"(?: <(?P<secs>[0-9]+\.[0-9]+)s>)?\s*$"
)

# One per test binary, so a `cargo test` log has several and they accumulate.
SUMMARY = re.compile(
    r"^test result: \w+\. (?P<passed>\d+) passed; (?P<failed>\d+) failed; (?P<ignored>\d+) ignored;"
    r" \d+ measured; (?P<filtered>\d+) filtered out; finished in (?P<wall>[0-9.]+)s"
)

# libtest's own refusal, printed when the timing flags arrive without the nightly gate satisfied. Worth
# recognising by name: without it the report would be an indistinguishable "no timings found", and the fix
# for this one is a specific missing environment variable rather than anything about the log.
#
# Matched on the shared tail, because there are TWO of these and matching one sentence missed the other —
# found by defeating the fallback on purpose. `-Z` with no bypass gives "the option `Z` is only accepted on
# the nightly compiler"; `--report-time` with no `-Z` gives "The "report-time" flag is only accepted on the
# nightly compiler with -Z unstable-options".
# Unanchored on purpose: with the suite's stderr merged into the log, a refusal can arrive with other
# output stuck to the front of it.
REFUSED = re.compile(r"is only accepted on the nightly compiler")


class Report:
    """What one log said. Every field is either measured or explicitly absent."""

    def __init__(self):
        self.timed = []  # (seconds, name, verdict)
        self.untimed = []  # names of result lines that carried no duration
        self.ignored = 0
        self.passed = 0
        self.failed = 0
        self.filtered = 0
        self.wall = 0.0
        self.summaries = 0
        self.refused = False

    @property
    def test_time(self):
        return sum(secs for secs, _, _ in self.timed)


def parse(lines, refused=False):
    report = Report()
    report.refused = refused
    for line in lines:
        line = line.rstrip("\n")

        if REFUSED.search(line):
            report.refused = True
            continue

        summary = SUMMARY.match(line)
        if summary:
            report.summaries += 1
            report.passed += int(summary["passed"])
            report.failed += int(summary["failed"])
            report.ignored += int(summary["ignored"])
            report.filtered += int(summary["filtered"])
            report.wall += float(summary["wall"])
            continue

        result = RESULT.match(line)
        if result:
            if result["verdict"] == "ignored":
                continue
            if result["secs"] is None:
                report.untimed.append(result["name"])
            else:
                report.timed.append((float(result["secs"]), result["name"], result["verdict"]))
    return report


def headline(report):
    """One line that has to survive being read on its own, collapsed, in a job summary."""
    if not report.timed:
        return "no per-test timings in this log"
    slowest_secs, slowest_name, _ = max(report.timed)
    plural = "test" if len(report.timed) == 1 else "tests"
    line = f"{len(report.timed)} {plural}, {report.test_time:.1f}s of test time"
    if report.wall:
        concurrency = report.test_time / report.wall
        line += f" in {report.wall:.1f}s wall clock ({concurrency:.1f}x concurrent)"
    return f"{line} — slowest {slowest_name} at {slowest_secs:.2f}s"


def caveats(report):
    """Everything the ranking cannot account for. Printed even when it is boring."""
    notes = []
    if report.refused:
        notes.append(
            "libtest refused --report-time: it is nightly-gated, and on a stable toolchain the run needs"
            " RUSTC_BOOTSTRAP=1 set for the test binary only — see scripts/integration-test.sh."
        )
    # Suppressed under a known refusal: every result line is untimed in that case, and naming a hundred of
    # them adds nothing to the one sentence above that already accounts for all of them.
    if report.untimed and not report.refused:
        shown = ", ".join(sorted(report.untimed)[:5])
        more = f" (and {len(report.untimed) - 5} more)" if len(report.untimed) > 5 else ""
        notes.append(
            f"{len(report.untimed)} result line(s) carried no duration, so the ranking is incomplete:"
            f" {shown}{more}."
        )
    if report.failed:
        notes.append(
            f"{report.failed} test(s) failed; a failed test's duration is when it gave up, not what the"
            " work costs."
        )
    if not report.timed and not report.refused:
        notes.append(
            "No timings and no refusal either, so the log is probably from a run that did not pass"
            " --report-time at all."
        )
    return notes


def render_text(report, top, label):
    out = []
    title = f"Slowest tests — {label}" if label else "Slowest tests"
    out.append("")
    out.append(f"{title}: {headline(report)}")
    for secs, name, verdict in sorted(report.timed, reverse=True)[:top]:
        mark = "" if verdict == "ok" else f"  [{verdict}]"
        out.append(f"  {secs:7.2f}s  {name}{mark}")
    if len(report.timed) > top:
        rest = sorted(report.timed, reverse=True)[top:]
        out.append(
            f"  … {len(rest)} more, {sum(s for s, _, _ in rest):.1f}s between them"
            f" (rerun with --top {len(report.timed)} for all of it)"
        )
    for note in caveats(report):
        out.append(f"  note: {note}")
    return "\n".join(out) + "\n"


def render_markdown(report, top, label):
    title = f"Slowest tests — {label}" if label else "Slowest tests"
    out = [f"### {title}", "", headline(report), ""]
    if report.timed:
        out.append("<details><summary>Ranked durations</summary>")
        out.append("")
        out.append("| rank | duration | test |")
        out.append("| --- | --- | --- |")
        for rank, (secs, name, verdict) in enumerate(sorted(report.timed, reverse=True)[:top], 1):
            mark = "" if verdict == "ok" else f" **{verdict}**"
            out.append(f"| {rank} | {secs:.2f}s | `{name}`{mark} |")
        if len(report.timed) > top:
            rest = sorted(report.timed, reverse=True)[top:]
            out.append(
                f"| | {sum(s for s, _, _ in rest):.1f}s | _{len(rest)} further tests, not listed_ |"
            )
        out.append("")
        out.append("</details>")
        out.append("")
    for note in caveats(report):
        out.append(f"> **note** {note}")
        out.append("")
    return "\n".join(out) + "\n"


def main():
    ap = argparse.ArgumentParser(
        description="Rank the slowest tests in a libtest log.",
        epilog="Reads stdin when given no path, or `-`.",
    )
    ap.add_argument("log", nargs="?", default="-", help="libtest output; `-` or omitted for stdin")
    ap.add_argument("--top", type=int, default=15, help="how many to rank (default 15)")
    ap.add_argument("--markdown", action="store_true", help="emit markdown for a CI job summary")
    ap.add_argument("--label", default="", help="what this run was, e.g. 'Integration (JDK 17)'")
    # For the caller that already knows: `scripts/integration-test.sh` retries without the timing flags when
    # libtest refuses them, and the retried log no longer contains the refusal, so the report would
    # otherwise diagnose a missing flag rather than a rejected one.
    ap.add_argument(
        "--refused",
        action="store_true",
        help="the caller already saw libtest refuse the timing flags",
    )
    args = ap.parse_args()

    if args.log == "-":
        lines = sys.stdin.read().splitlines()
    else:
        with open(args.log, encoding="utf-8", errors="replace") as handle:
            lines = handle.read().splitlines()

    report = parse(lines, refused=args.refused)
    render = render_markdown if args.markdown else render_text
    sys.stdout.write(render(report, args.top, args.label))
    return 0


if __name__ == "__main__":
    sys.exit(main())
