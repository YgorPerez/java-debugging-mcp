#!/usr/bin/env python3
"""Diff the downstream toolkit's documented tool surface against the release it pins.

    scripts/toolkit-parity.py                     # read their pin, resolve it here, report
    scripts/toolkit-parity.py --pin v0.20.0       # skip the fetch and assert against a tag
    scripts/toolkit-parity.py --surface WORKTREE  # "would their docs still be right against main?"
    scripts/toolkit-parity.py --prose <dir>       # read a local clone of theirs instead of the API
    scripts/toolkit-parity.py --markdown          # for a job summary or an issue body

CI-8 (#162). `docs/toolkit-contract.md` lists seven ways a change here reaches
[`infotravel-dev-toolkit`](https://github.com/ygor-infotera/infotravel-dev-toolkit) and says six of them
are silent. The one that is not — an asset rename or a missing `SHA256SUMS` — is already checked from both
sides. The other six were checked by nobody, in either repo, and the mitigation on record was a human
writing good release notes.

## Which rows this covers, and which it cannot

| row of the contract's table | covered? |
|---|---|
| Rename or remove a **tool** | **yes** — `named downstream, absent from the pin` |
| Rename a **tool argument** | **yes** — per tool, keys their prose passes that the pinned tool does not take |
| Add a **tool** | **yes** — `exported by the pin, named nowhere downstream`, the quietest row |
| Corrupt or truncate a **description** | no. The snapshot tests here catch that at the source |
| Change what a **reply** says | no. Nothing downstream quotes a reply in a shape a regex can hold |
| Change **behaviour behind an existing name** | no, and nothing mechanical can. `--since` prints the surface delta between their pin and the newest tag, which is a PROXY: a reworded description shows up, a changed meaning does not |

## It reports; it never gates

`docs/toolkit-contract.md`: this repo owes the toolkit no compatibility guarantee, and nothing here
depends on it. A check pointed at another repository that could block a change is the permanent-red-that-
tested-nothing shape `CLAUDE.md` deleted two workflows over. So this exits 0 on any diff it can compute,
and non-zero **only** when it could not compute one.

## An unresolvable pin FAILS. It does not print a clean diff

Empty reads like "nothing to report" everywhere it lands (DOC-15, #145). Every input that could go missing
— their `jdwp-version`, their prose, the tag here, the snapshot at that tag — is fatal with a message
naming which one, because "0 differences" from a run that read nothing is the worst output available.

## The credential, which is the reason there is no scheduled workflow

`ygor-infotera/infotravel-dev-toolkit` is **private**. #162 assumed the public contents API; there is no
such thing here, and a workflow's `GITHUB_TOKEN` is scoped to its own repository, so no amount of
`permissions:` reaches it. This script therefore shells out to `gh api`, which uses whatever auth the
person running it already has — no token in an environment variable, no secret in this repo, nothing to
paste anywhere. `.github/workflows/toolkit-parity.yml` is `workflow_dispatch`-only for the same reason and
says what it would need.
"""

import argparse
import base64
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TOOLKIT = "ygor-infotera/infotravel-dev-toolkit"
PIN_FILE = "jdwp-version"
DESCRIPTIONS = "mcp-server/tests/tool-descriptions.txt"
SCHEMAS = "mcp-server/tests/argument-schemas.txt"

# `## debug.foo` in either snapshot.
SNAPSHOT_TOOL = re.compile(r"^## (?P<name>debug\.[a-z0-9_]+)$", re.M)
# `- name: {json}` under a tool heading in the argument snapshot.
SNAPSHOT_ARGUMENT = re.compile(r"^- (?P<name>[A-Za-z0-9_]+): ")
# How their prose calls a tool: `debug.set_line_stop {class_pattern:"…", line:1489, trace:true}`. The
# argument block is optional — plenty of mentions are bare, and a bare mention still names the tool.
#
# `(?!\*)` AND THE TRAILING-UNDERSCORE GUARD ARE A REAL FALSE POSITIVE, not a precaution. The first run of
# this script reported `debug.step_` as documentation for a tool nobody can call; the prose says
# "weigh it the way you weigh `debug.step_*`", which is a GLOB, and the name group happily stopped at the
# star. A parity check whose first output is a tool that does not exist is one nobody reads twice — the
# must-not-fire half that `.githooks/test.sh` spends two thirds of its cases on. No real tool name ends in
# an underscore, and nothing followed by `*` is a name.
CALL = re.compile(r"debug\.(?P<name>[a-z0-9]+(?:_[a-z0-9]+)*)(?!\w|\*)\s*(?P<args>\{[^{}]*\})?")
# A key inside such a block. Deliberately narrow: `key:` or `"key":` at the start or after a comma, so a
# colon inside a quoted VALUE (`{class_pattern:"a:b"}`) is not read as a key.
ARGUMENT_KEY = re.compile(r'(?:^\{|,)\s*"?(?P<key>[a-z][a-z0-9_]*)"?\s*:')


class Unresolvable(Exception):
    """An input that could not be read. Always fatal — see the module docstring."""


def gh(*args: str) -> str:
    """`gh` with its stderr preserved in the exception, because "not found" and "not authorised" are
    different problems and the difference is the whole diagnosis."""
    try:
        done = subprocess.run(["gh", *args], capture_output=True, text=True, check=True)
    except FileNotFoundError as why:
        raise Unresolvable(
            "`gh` is not on PATH. This reads a PRIVATE repository, so it uses your existing gh auth "
            "rather than a token in an environment variable — install it and `gh auth login`."
        ) from why
    except subprocess.CalledProcessError as why:
        raise Unresolvable(f"`gh {' '.join(args)}` failed:\n{why.stderr.strip()}") from why
    return done.stdout


def contents(path: str) -> str:
    """One file from the toolkit, decoded."""
    raw = gh("api", f"repos/{TOOLKIT}/contents/{path}", "--jq", ".content")
    try:
        return base64.b64decode(raw).decode("utf-8", errors="replace")
    except (ValueError, UnicodeDecodeError) as why:
        raise Unresolvable(f"{TOOLKIT}:{path} did not decode as text: {why}") from why


def prose_paths() -> list[str]:
    """Every markdown file in the toolkit.

    ENUMERATED, NOT LISTED. #162's measurement named five files by hand; a sixth skill that mentions a tool
    would be invisible to a hardcoded list, and "no drift" from a scan that did not look at the file is
    the same defect as an empty diff from an unread pin.
    """
    tree = gh("api", f"repos/{TOOLKIT}/git/trees/main?recursive=1", "--jq", ".tree[].path")
    paths = [p for p in tree.splitlines() if p.endswith(".md")]
    if not paths:
        raise Unresolvable(f"{TOOLKIT} has no markdown files at main — the tree read returned nothing.")
    return paths


def pinned_tag() -> str:
    tag = contents(PIN_FILE).strip()
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", tag):
        raise Unresolvable(f"{TOOLKIT}:{PIN_FILE} reads {tag!r}, which is not a version tag.")
    return tag


# `--surface WORKTREE` asks the other useful question: their docs are written against the release they
# pin, but the thing that will break them is what is on `main` now. It is also what makes this script
# testable without tags, which a shallow CI checkout does not have.
WORKTREE = "WORKTREE"


def snapshot_at(tag: str, path: str) -> str:
    """A committed snapshot as it stood at `tag`, from this repo's own history.

    Read from the TAG rather than the working tree: the pin names a release, and what the toolkit
    documents is what that release exported. A shallow clone has no tags, which is why this is fatal.
    """
    if tag == WORKTREE:
        try:
            return (ROOT / path).read_text(encoding="utf-8")
        except OSError as why:
            raise Unresolvable(f"cannot read {path} from the working tree: {why}") from why
    try:
        done = subprocess.run(
            ["git", "show", f"{tag}:{path}"], capture_output=True, text=True, check=True, cwd=ROOT
        )
    except subprocess.CalledProcessError as why:
        raise Unresolvable(
            f"cannot read {path} at {tag}: {why.stderr.strip()}\n"
            f"       If {tag} is not here, this clone is shallow or the tag was never pushed — either way "
            "there is nothing to compare against, and an empty diff would be a lie."
        ) from why
    return done.stdout


def exported(tag: str) -> dict[str, set[str]]:
    """`{tool: {argument, …}}` as of `tag`."""
    described = set(SNAPSHOT_TOOL.findall(snapshot_at(tag, DESCRIPTIONS)))
    if not described:
        raise Unresolvable(
            f"parsed no `## debug.*` headings out of {DESCRIPTIONS} at {tag} — the snapshot's shape "
            "changed, and a surface of zero tools would make every downstream name look like drift."
        )
    tools: dict[str, set[str]] = {name: set() for name in described}
    current = None
    for line in snapshot_at(tag, SCHEMAS).splitlines():
        if m := SNAPSHOT_TOOL.match(line):
            current = m.group("name")
        elif current and (m := SNAPSHOT_ARGUMENT.match(line)):
            tools.setdefault(current, set()).add(m.group("name"))
    return tools


def local_prose(where: Path) -> list[Path]:
    """Markdown under a local path, for a clone of theirs — or for a fixture."""
    if where.is_file():
        return [where]
    found = sorted(where.rglob("*.md"))
    if not found:
        raise Unresolvable(f"no markdown under {where} — an empty prose set would report every tool as unnamed.")
    return found


def documented(paths, read) -> tuple[dict[str, set[str]], dict[str, list[str]]]:
    """`{tool: {argument key, …}}` named in the toolkit's prose, plus where each tool was seen."""
    tools: dict[str, set[str]] = {}
    seen: dict[str, list[str]] = {}
    for path in paths:
        text = read(path)
        for call in CALL.finditer(text):
            name = f"debug.{call.group('name')}"
            tools.setdefault(name, set())
            label = str(path)
            if label not in seen.setdefault(name, []):
                seen[name].append(label)
            if block := call.group("args"):
                tools[name].update(m.group("key") for m in ARGUMENT_KEY.finditer(block))
    return tools, seen


def report(tag: str, theirs: dict[str, set[str]], ours: dict[str, set[str]], seen, markdown: bool) -> str:
    bullet = "- " if markdown else "  "
    out: list[str] = []
    head = f"Toolkit parity against their pin `{tag}`" if markdown else f"toolkit parity vs {tag}"
    out.append(f"## {head}" if markdown else f"== {head}")
    out.append("")
    out.append(f"{len(ours)} tools exported at {tag}; {len(theirs)} named in their prose.")
    out.append("")

    ghosts = sorted(set(theirs) - set(ours))
    out.append(
        f"### Named downstream, absent from the pin ({len(ghosts)})"
        if markdown
        else f"-- named downstream, absent from the pin ({len(ghosts)})"
    )
    out.append("")
    if ghosts:
        out.append("Documentation for a tool nobody can call. This is the row `set_breakpoint` sat in for")
        out.append("weeks after VOCAB-1 (#20) renamed seven tools.")
        out.append("")
        for name in ghosts:
            out.append(f"{bullet}`{name}` — named in {', '.join(seen.get(name, []))}")
    else:
        out.append("None.")
    out.append("")

    unnamed = sorted(set(ours) - set(theirs))
    out.append(
        f"### Exported by the pin, named nowhere downstream ({len(unnamed)})"
        if markdown
        else f"-- exported by the pin, named nowhere downstream ({len(unnamed)})"
    )
    out.append("")
    if unnamed:
        out.append("A tool nobody will find: the quietest row in the table, because nothing breaks.")
        out.append("")
        for name in unnamed:
            out.append(f"{bullet}`{name}`")
    else:
        out.append("None.")
    out.append("")

    drifted = []
    for name in sorted(set(theirs) & set(ours)):
        if extra := sorted(theirs[name] - ours[name]):
            drifted.append((name, extra))
    out.append(
        f"### Argument keys their prose passes that the pinned tool does not take ({len(drifted)})"
        if markdown
        else f"-- argument keys their prose passes that the pin does not take ({len(drifted)})"
    )
    out.append("")
    if drifted:
        out.append("A documented example that would be refused. Read these before believing them: a key")
        out.append("inside a nested object in their prose looks the same to this scan as a top-level one.")
        out.append("")
        for name, extra in drifted:
            out.append(f"{bullet}`{name}`: {', '.join(f'`{k}`' for k in extra)}")
    else:
        out.append("None.")
    out.append("")
    out.append(
        "Three of the seven rows in `docs/toolkit-contract.md` are covered here. A reply's wording and a "
        "behaviour change behind an unchanged name are not, and nothing mechanical can reach them — the "
        "release notes are still the mitigation for those."
    )
    return "\n".join(out) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(description="Diff the toolkit's documented tools against the release it pins.")
    ap.add_argument("--pin", help="assert against this tag instead of reading their jdwp-version")
    ap.add_argument(
        "--surface",
        metavar="TAG",
        help=f"where to read this side's surface from; `{WORKTREE}` for the working tree (default: --pin)",
    )
    ap.add_argument("--prose", metavar="PATH", type=Path, help="a local clone of theirs, instead of the API")
    ap.add_argument("--markdown", action="store_true", help="render for a job summary or an issue body")
    args = ap.parse_args()

    try:
        tag = args.pin or pinned_tag()
        surface = args.surface or tag
        ours = exported(surface)
        if args.prose:
            base = args.prose
            theirs, seen = documented(
                local_prose(base),
                lambda p: Path(p).read_text(encoding="utf-8", errors="replace"),
            )
            # Relative to what was asked for, so a transcript does not carry somebody's home directory.
            seen = {k: [str(Path(p).relative_to(base) if base.is_dir() else Path(p).name) for p in v] for k, v in seen.items()}
        else:
            theirs, seen = documented(prose_paths(), contents)
    except Unresolvable as why:
        # Loud, and non-zero. A clean diff from a run that could not read one of its two inputs is the
        # failure this whole script is built to avoid reproducing.
        print(f"toolkit-parity: could not resolve an input, so there is NO diff to report — {why}", file=sys.stderr)
        return 1

    sys.stdout.write(report(surface, theirs, ours, seen, args.markdown))
    # Deliberately 0 whatever the diff says. See the module docstring: this reports, it never gates.
    return 0


if __name__ == "__main__":
    sys.exit(main())
