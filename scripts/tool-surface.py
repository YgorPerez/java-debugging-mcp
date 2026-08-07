#!/usr/bin/env python3
"""Assemble the MCP tool surface into one JSON document, so two releases can be diffed with two curls.

    scripts/tool-surface.py --tag v0.20.0 > tool-surface-v0.20.0.json
    scripts/tool-surface.py --tag v0.20.0 --check tool-surface-v0.20.0.json

REL-8 (#165). The surface is generated and committed — `mcp-server/tests/tool-descriptions.txt`,
`mcp-server/tests/argument-schemas.txt` — and gated by snapshot tests, which is what keeps it honest after
DOC-7 (#108) shipped interleaved gibberish in a release. But it exists **only in the tree**. A release
publishes five binaries, `SHA256SUMS` and the notes, so the question a pinned consumer actually asks —
*what changed for callers between the tag I pin and the one I am moving to* — needed a clone at two tags
and a diff, and the release notes' prose was the only published form of the answer.

## Built from the committed snapshots, not from the binary

Deliberately, and #165 says so: an asset regenerated from the binary at release time is a **second source
of truth** next to the files the snapshot tests guard, and the two could disagree with nothing noticing.
Reading the committed files makes the asset a reshaping of what is already reviewed.

That leaves one thing to check, which is whether the snapshots are current — and that is already gated:
`tool_descriptions_match_the_committed_snapshot` and its argument-schema sibling run in plain `cargo test`,
`release.yml` calls the whole suite before it publishes anything, and a stale snapshot fails there. This
script therefore does not re-derive the surface; it refuses to emit one that is internally inconsistent.

## What it refuses to publish

Building JSON out of text fails quietly — a parser that stops matching returns *fewer tools*, and fewer
tools is a valid-looking document. So every consistency fact available from the inputs is checked and any
one of them is fatal:

- the two files name exactly the same tools, in the same number;
- the `N tools, M arguments` line in the argument file's own header matches what was parsed;
- `docs/tools.md`'s table names the same count;
- no tool has an empty description, and no argument has an unparseable schema.

That last set is what makes a silent half-parse impossible rather than unlikely.

## The descriptions are the WRAPPED form, and that is not a compromise to hide

The committed snapshots word-wrap at 110 columns, which normalises whitespace — so what is published here
is the snapshot's rendering rather than the raw string the server sends. It is the right form for the job:
the asset exists to be diffed, wrapping is what makes a corrupted clause a two-line diff instead of a
one-character one (DOC-7), and the raw string is one `tools/list` call away for anyone who needs it. The
schema says so at the field rather than letting a reader assume otherwise.
"""

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DESCRIPTIONS = ROOT / "mcp-server/tests/tool-descriptions.txt"
SCHEMAS = ROOT / "mcp-server/tests/argument-schemas.txt"
TOOLS_DOC = ROOT / "docs/tools.md"

# The format's stable identifier, and the reason `surface_version` exists separately from the crate
# version: the crate moves for reasons that have nothing to do with the tool surface, so a consumer asking
# "did the contract change" cannot read the answer off it. The bump rule lives in the schema file, beside
# the field, rather than here — one copy.
SCHEMA_URL = (
    "https://raw.githubusercontent.com/YgorPerez/java-debugging-mcp/main/docs/tool-surface.schema.json"
)
KIND = "mcp-tool-surface"
SURFACE_VERSION = 1

# `- name: {json}` or `- name: REQUIRED {json}`.
ARGUMENT = re.compile(r"^- (?P<name>[A-Za-z0-9_]+): (?P<required>REQUIRED )?(?P<schema>\{.*\})$")
# The argument file's own header states its totals. Reading them back is the cheapest possible check that
# this parser saw the whole file.
TOTALS = re.compile(r"^# (?P<tools>\d+) tools, (?P<arguments>\d+) arguments\.$", re.M)
# docs/tools.md's table, one row per tool.
DOC_ROW = re.compile(r"^\| `(?P<name>debug\.[A-Za-z0-9_]+)` \|", re.M)


class Inconsistent(Exception):
    """An input that cannot be published. Every one of these is a silent-half-parse in the making."""


def read(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError as why:
        raise Inconsistent(f"cannot read {path.relative_to(ROOT)}: {why}") from why


def parse_descriptions() -> dict[str, str]:
    """`{tool: wrapped description}` from the description snapshot."""
    tools: dict[str, str] = {}
    current: str | None = None
    lines: list[str] = []
    for line in read(DESCRIPTIONS).splitlines():
        if line.startswith("## "):
            if current:
                tools[current] = "\n".join(lines).strip()
            current, lines = line[3:].strip(), []
        elif line.startswith("#") or not line.strip():
            continue
        elif current:
            lines.append(line)
    if current:
        tools[current] = "\n".join(lines).strip()
    return tools


def parse_schemas() -> tuple[dict[str, list[dict]], tuple[int, int] | None]:
    """`{tool: [argument, …]}` plus the totals the file states about itself."""
    tools: dict[str, list[dict]] = {}
    current: str | None = None
    pending: dict | None = None
    body: list[str] = []

    def close_argument() -> None:
        nonlocal pending, body
        if pending is not None and current is not None:
            pending["description"] = "\n".join(body).strip()
            tools[current].append(pending)
        pending, body = None, []

    for line in read(SCHEMAS).splitlines():
        if line.startswith("## "):
            close_argument()
            current = line[3:].strip()
            tools[current] = []
        elif line.startswith("#") or not line.strip():
            continue
        elif (m := ARGUMENT.match(line)) and current:
            close_argument()
            try:
                schema = json.loads(m.group("schema"))
            except json.JSONDecodeError as why:
                raise Inconsistent(f"{current}.{m.group('name')} has an unparseable schema: {why}") from why
            pending = {
                "name": m.group("name"),
                "required": bool(m.group("required")),
                "schema": schema,
            }
        elif pending is not None:
            body.append(line.strip())
    close_argument()

    stated = TOTALS.search(read(SCHEMAS))
    totals = (int(stated.group("tools")), int(stated.group("arguments"))) if stated else None
    return tools, totals


def build(tag: str) -> dict:
    descriptions = parse_descriptions()
    schemas, stated = parse_schemas()

    if not descriptions:
        raise Inconsistent(f"parsed no tools out of {DESCRIPTIONS.name} — the `## <tool>` shape changed")
    only_described = sorted(set(descriptions) - set(schemas))
    only_scheduled = sorted(set(schemas) - set(descriptions))
    if only_described or only_scheduled:
        raise Inconsistent(
            f"the two snapshots disagree about the tool set. Only in {DESCRIPTIONS.name}: "
            f"{only_described}; only in {SCHEMAS.name}: {only_scheduled}. One of them was regenerated "
            "and the other was not, and publishing either would state a surface that is not this one."
        )
    if empty := sorted(name for name, text in descriptions.items() if not text):
        raise Inconsistent(f"these tools parsed with an empty description: {empty}")

    argument_count = sum(len(args) for args in schemas.values())
    if stated is None:
        raise Inconsistent(
            f"{SCHEMAS.name} no longer states its own `# N tools, M arguments` totals, which is the "
            "cheapest check that this parser saw the whole file."
        )
    if stated != (len(schemas), argument_count):
        raise Inconsistent(
            f"{SCHEMAS.name} says {stated[0]} tools and {stated[1]} arguments; this parsed "
            f"{len(schemas)} and {argument_count}. A parser that silently sees fewer tools produces a "
            "valid-looking document that understates the surface."
        )

    documented = set(DOC_ROW.findall(read(TOOLS_DOC)))
    if documented != set(descriptions):
        raise Inconsistent(
            f"docs/tools.md's table and the snapshots disagree. Only in the table: "
            f"{sorted(documented - set(descriptions))}; only in the snapshots: "
            f"{sorted(set(descriptions) - documented)}."
        )

    return {
        "$schema": SCHEMA_URL,
        "kind": KIND,
        "surface_version": SURFACE_VERSION,
        "release": tag,
        "tool_count": len(descriptions),
        "argument_count": argument_count,
        "tools": [
            {
                "name": name,
                "description": descriptions[name],
                "arguments": sorted(schemas[name], key=lambda a: a["name"]),
            }
            for name in sorted(descriptions)
        ],
    }


def render(document: dict) -> str:
    """Two-space indent and sorted keys, because this file exists to be read in a diff."""
    return json.dumps(document, indent=2, ensure_ascii=False, sort_keys=True) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(description="Assemble the MCP tool surface as one JSON document.")
    ap.add_argument("--tag", required=True, help="the release this surface describes, e.g. v0.20.0")
    ap.add_argument(
        "--check",
        metavar="FILE",
        help="compare FILE against what this tree would emit, instead of writing it",
    )
    args = ap.parse_args()

    try:
        wanted = render(build(args.tag))
    except Inconsistent as why:
        print(f"tool-surface: refusing to publish — {why}", file=sys.stderr)
        return 1

    if not args.check:
        sys.stdout.write(wanted)
        return 0

    # The release builds the asset and then checks it, which is a narrow guarantee stated narrowly: it
    # catches an asset that was edited, truncated or swapped between being built and being uploaded. What
    # it does NOT check is that the snapshots match the binary — `cargo test` does that, and release.yml
    # runs the whole suite before this job exists.
    try:
        got = Path(args.check).read_text(encoding="utf-8")
    except OSError as why:
        print(f"tool-surface: cannot read {args.check}: {why}", file=sys.stderr)
        return 1
    if got == wanted:
        print(f"tool-surface: {args.check} matches this tree's snapshots ({len(wanted)} bytes).")
        return 0
    print(
        f"tool-surface: {args.check} is not what this tree's snapshots produce. It was built from them "
        "moments ago, so something changed it in between — do not publish it.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
