#!/usr/bin/env python3
"""Build the body of a GitHub release: the narrative someone wrote, then a categorized changelog.

    scripts/release-notes.py                 # the tag pointing at HEAD, or $GITHUB_REF_NAME
    scripts/release-notes.py v0.8.0          # preview what a tag would have published
    scripts/release-notes.py v0.8.0 --since v0.7.0   # override the baseline tag

Writes markdown to stdout. Never fails a release: every input it cannot find degrades to a section it
leaves out, because a publish step that dies over a changelog is worse than a thin changelog.

## Why this exists

`release.yml` used to publish with `gh release create --generate-notes`, and every release from v0.2.1 to
v0.8.0 came out as one line — the `**Full Changelog**` link and nothing else. Two reasons, and the second
is the expensive one:

**GitHub's generated notes list merged pull requests, not commits.** Plenty of work here lands as a direct
push to `main` (the whole v0.8.0 range did), and a release whose commits were all direct pushes generates
an empty "What's Changed". So the mechanism that reads as the default was, for this repo's actual workflow,
a no-op.

**`--generate-notes` never looked at the release commit's body.** `/release` step 4 has you write the
body — new tools by name, renames with *both* names, behaviour changes behind an unchanged name — then
**re-tag**, because amending moves the commit an annotated tag names. That ritual was writing prose into
git history that the releases page never showed, while `docs/toolkit-contract.md` was pointing the
downstream toolkit at release notes as the one mitigation for five silent failure modes. The notes it was
told to read did not contain any of it.

So the body is assembled here instead, from the two things that actually exist at tag time:

1. **The release commit's message body**, verbatim, as the lead. That is the hand-written part and no tool
   can generate it. `release.sh` opens an editor for it; when there is no tty it writes the subject alone,
   which is the documented trap — a release with no narrative still gets a changelog from step 2, so the
   failure is now visible rather than total.
2. **A categorized changelog from conventional commits** in `<previous tag>..<tag>`, with the emoji
   headings `~/html/b2c-next` uses, so the two repos' releases read the same way.

## Why conventional commits and not `.github/release.yml`

b2c-next categorizes with `.github/release.yml`, which keys off **pull request labels**. That is the right
tool there — every change arrives as a labelled PR. Here it would categorize almost nothing: direct pushes
have no PR to label, and this repo's labels are a triage vocabulary (`needs-triage`, `ready-for-agent` —
`docs/agents/triage-labels.md`), not a changelog one. Commit subjects, on the other hand, have been
conventional (`fix(stop-points):`, `docs(context):`, `ci(semver):`) for the whole history, so they are the
signal that is actually there. No `.github/release.yml` is committed on purpose: it would be config that
looks load-bearing and decides nothing.

## What it does not do

It does not invent the narrative, and it does not check the narrative says what the toolkit contract needs
it to say. A release whose body is only a changelog is a release that documented nothing about tool names,
arguments or replies — `/release` step 4 is still the step that matters.
"""

import argparse
import os
import re
import subprocess
import sys

# The headings and their order, matching b2c-next's `.github/release.yml` so a reader moving between the
# two repos sees one vocabulary. `⚠️ Breaking Changes` is the one addition: b2c-next has no such label, and
# this workspace has `scripts/semver-check.sh` gating a public API, so a break is worth its own section at
# the top rather than buried under Features.
CATEGORIES: list[tuple[str, tuple[str, ...]]] = [
    ("🚀 Features", ("feat",)),
    ("🐛 Bug Fixes", ("fix",)),
    ("⚡ Performance", ("perf",)),
    ("♻️ Refactoring", ("refactor",)),
    ("🎨 Styling", ("style",)),
    ("📝 Documentation", ("docs", "doc")),
    ("🧪 Tests", ("test", "tests")),
    ("🔧 Chores & CI", ("chore", "ci", "build")),
    ("⏪ Reverts", ("revert",)),
]
BREAKING = "⚠️ Breaking Changes"
OTHER = "Other Changes"

# Types this repo writes on purpose that deliberately get no heading. `merge:` is used 10 times for a
# commit landing a series of issues at once — it spans several categories by definition, so filing it
# under any one of them would misreport it, and Other Changes with the type visible is the honest place.
# They are listed here so `--list-types` reports the vocabulary the repo actually uses: a commit-msg hook
# that rejected 23 of this repo's own 351 subjects would be uninstalled the same day, which is the
# weighting `scripts/guard.test.sh` gives most of its cases to.
UNCATEGORISED_TYPES: tuple[str, ...] = ("merge",)

# The trailing `(?:\+...)*` is this repo's compound form, and it was measured rather than imagined
# (REL-4, #147): `fix(lint)+docs:` and `docs(dump)+measure:` appear 13 times in 351 commits, and every
# one of them FAILED this regex outright — landing in Other Changes with its type stripped, which is
# strictly worse than the unknown-type case below that keeps the type visible. The first type wins the
# category, because it is the primary one in every use here.
CONVENTIONAL = re.compile(
    r"^(?P<type>[A-Za-z]+)(?:\((?P<scope>[^)]*)\))?"
    r"(?P<extra>(?:\+[A-Za-z]+(?:\([^)]*\))?)*)"
    r"(?P<bang>!)?:\s*(?P<desc>.+)$"
)
# A trailing `(#77)` or `(#73, #74, #75, #76)`. Both forms are in this history: squash-merged PRs get the
# number appended by GitHub, and a direct push that closes issues often lists them the same way.
TRAILING_REFS = re.compile(r"\s*\((#\d+(?:\s*,\s*#\d+)*)\)\s*$")
# `chore(release): 0.8.0` — the release commit itself. Its body is the narrative above the changelog, so
# listing it as a chore says the same thing twice and adds a line nobody reads.
# Trailing `(#123)` tolerated (REL-4, #147): a release commit that closes issues the way the rest of this
# history does was listed as a chore, saying the same thing twice. Still anchored on the version, so an
# ordinary `chore(release):` about something else is not swallowed.
RELEASE_COMMIT = re.compile(
    r"^chore\(release\):\s*v?\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?\s*(?:\(#\d+(?:\s*,\s*#\d+)*\))?\s*$"
)
# Case-insensitive, and `BREAKING:` accepted alongside the spec's two spellings (REL-4, #147). This is the
# one regex here whose miss is expensive: a genuine break written `Breaking change:` skipped the
# `⚠️ Breaking Changes` section, and that section exists because semver-check.sh gates a public API — so a
# break belongs at the top rather than under Features, and the miss is only visible after the tag.
BREAKING_FOOTER = re.compile(r"^(?:BREAKING[ -]CHANGE|BREAKING):", re.M | re.I)

RECORD = "\x1e"  # between commits
FIELD = "\x1f"  # between a commit's fields


def git(*args: str) -> str:
    """Run git and return stdout, or "" if it failed.

    Swallowing the error is deliberate: every caller here has a sensible empty answer, and the alternative
    is a release that publishes no assets because a `git describe` did not like a shallow clone.
    """
    try:
        return subprocess.run(
            ["git", *args], capture_output=True, text=True, check=True
        ).stdout.strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return ""


def repo_slug() -> str:
    """`owner/name`, for the compare link. `$GITHUB_REPOSITORY` in CI, the origin remote otherwise."""
    if env := os.environ.get("GITHUB_REPOSITORY"):
        return env
    url = git("remote", "get-url", "origin")
    m = re.search(r"github\.com[:/](?P<slug>[^/]+/[^/]+?)(?:\.git)?$", url)
    return m.group("slug") if m else ""


def resolve_tag(given: str | None) -> str:
    """The tag being released: the argument, else the CI ref, else a tag pointing at HEAD."""
    if given:
        return given
    if (ref := os.environ.get("GITHUB_REF_NAME", "")).startswith("v"):
        return ref
    return git("describe", "--exact-match", "--tags", "HEAD")


def previous_tag(tag: str) -> str:
    """The nearest tag that is an ancestor of `tag`'s parent, which is what a compare link wants.

    Ancestry rather than version sort, on purpose: it answers "what is new *on this branch* since the last
    release" even when a tag was cut elsewhere, and it is the same baseline `scripts/semver-check.sh` uses.
    Empty for the first release, which the caller handles by linking the commit list instead.
    """
    return git("describe", "--tags", "--abbrev=0", f"{tag}^")


def narrative(tag: str) -> str:
    """The release commit's message body — everything after the subject line.

    This is the part `/release` step 4 writes and `docs/toolkit-contract.md` depends on. Taken from the
    commit rather than the annotated tag because `release.sh` tags with `-m v<version>` and nothing else;
    the prose has always lived in the commit.
    """
    message = git("log", "-1", "--format=%B", f"{tag}^{{commit}}")
    if not message:
        return ""
    _subject, _, body = message.partition("\n")
    return body.strip()


def commits(rev_range: str) -> list[tuple[str, str, str]]:
    """`(short sha, subject, body)` oldest first, merges dropped.

    Oldest first so the changelog reads in the order the work happened. `--no-merges` because a merge
    commit's subject is `Merge pull request #N from …`, which categorizes as nothing and duplicates the
    squashed subject that carries the actual description.
    """
    out = git(
        "log",
        "--reverse",
        "--no-merges",
        f"--format=%h{FIELD}%s{FIELD}%b{RECORD}",
        rev_range,
    )
    parsed = []
    for record in out.split(RECORD):
        if not (record := record.strip("\n")):
            continue
        sha, _, rest = record.partition(FIELD)
        subject, _, body = rest.partition(FIELD)
        if sha.strip() and subject.strip():
            parsed.append((sha.strip(), subject.strip(), body))
    return parsed


def entry(sha: str, subject: str) -> tuple[str, str, bool]:
    """Turn a commit subject into `(category, rendered bullet, is breaking)`.

    Bare `#77` and a bare short sha are left as plain text rather than markdown links: GitHub autolinks
    both inside a release body, and `#77` resolves correctly whether the number is a PR or an issue — which
    matters here, because both appear (v0.7.0's subject listed four *issues*).
    """
    refs = ""
    if m := TRAILING_REFS.search(subject):
        refs = ", ".join(part.strip() for part in m.group(1).split(","))
        subject = subject[: m.start()].rstrip()

    category, description, breaking = OTHER, subject, False
    if m := CONVENTIONAL.match(subject):
        kind = m.group("type").lower()
        breaking = bool(m.group("bang"))
        description = m.group("desc").strip()
        for title, kinds in CATEGORIES:
            if kind in kinds:
                category = title
                break
        else:
            # A conventional-looking subject with a type nobody here uses. Keeping the type visible beats
            # dropping it into Other Changes unlabelled — the reader can see why it landed there.
            description = f"{kind}: {description}"
        if scope := (m.group("scope") or "").strip():
            description = f"**{scope}**: {description}"

    # `·` between the refs and the sha, comma only *within* the refs. A subject listing four issues
    # (v0.7.0's did) otherwise ends in five comma-separated tokens of which one is not a ref.
    trail = " · ".join(filter(None, [refs, sha]))
    return category, f"* {description} ({trail})", breaking


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("tag", nargs="?", help="the tag being released (default: $GITHUB_REF_NAME, or HEAD's tag)")
    ap.add_argument("--since", metavar="TAG", help="baseline tag (default: the nearest ancestor tag)")
    ap.add_argument(
        "--list-types",
        action="store_true",
        help="print the accepted conventional-commit types, one per line, and exit",
    )
    args = ap.parse_args()

    # The .githooks/commit-msg hook calls this rather than carrying its own copy of the list. A second
    # copy of the vocabulary is the duplicated fact this repo keeps writing post-mortems about, and it
    # is the specific objection #147 raised against reaching for commitlint.
    if args.list_types:
        for _, kinds in CATEGORIES:
            for kind in kinds:
                print(kind)
        for kind in UNCATEGORISED_TYPES:
            print(kind)
        return 0

    tag = resolve_tag(args.tag)
    if not tag:
        print(
            "release-notes: no tag given and none points at HEAD. Pass one: scripts/release-notes.py v0.9.0",
            file=sys.stderr,
        )
        return 2

    slug = repo_slug()
    prev = args.since or previous_tag(tag)
    rev_range = f"{prev}..{tag}" if prev else tag

    sections: dict[str, list[str]] = {}
    for sha, subject, body in commits(rev_range):
        if RELEASE_COMMIT.match(subject):
            continue
        category, bullet, breaking = entry(sha, subject)
        if breaking or BREAKING_FOOTER.search(body or ""):
            category = BREAKING
        sections.setdefault(category, []).append(bullet)

    parts = []
    if lead := narrative(tag):
        parts.append(lead)

    ordered = [BREAKING, *(title for title, _ in CATEGORIES), OTHER]
    changelog = [f"### {title}\n" + "\n".join(sections[title]) for title in ordered if title in sections]
    if changelog:
        parts.append("## What's Changed\n\n" + "\n\n".join(changelog))

    if slug:
        if prev:
            parts.append(f"**Full Changelog**: https://github.com/{slug}/compare/{prev}...{tag}")
        else:
            # First release: there is no baseline to compare against, and `compare/` needs two refs.
            parts.append(f"**Full Changelog**: https://github.com/{slug}/commits/{tag}")

    if not parts:
        # Reachable only with no narrative, no commits in range and no remote — a hand-made tag on a
        # detached clone. Say so in the body rather than publishing an empty one, which reads as a bug.
        parts.append(f"Released from `{tag}`. No commit history was available to build a changelog from.")

    print("\n\n---\n\n".join(parts))
    return 0


if __name__ == "__main__":
    sys.exit(main())
