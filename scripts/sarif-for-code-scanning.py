#!/usr/bin/env python3
"""Turn rust-doctor's SARIF into something GitHub code scanning can actually use.

    scripts/sarif-for-code-scanning.py rust-doctor.sarif rust-doctor-published.sarif

Two things are wrong with uploading the raw file, and both were invisible from the security tab itself —
they were found by reading the alerts the tab had accumulated (115 of them) rather than from a failing build.

**The paths do not resolve.** rust-doctor writes `"uri": "src/handlers.rs"` with
`"uriBaseId": "%SRCROOT%"`, and declares no `originalUriBaseIds` mapping for `%SRCROOT%` — so there is
nothing to resolve the URI against. GitHub falls back to treating it as repo-root-relative, where
`src/handlers.rs` does not exist: this is a Cargo *workspace*, and the file is `mcp-server/src/handlers.rs`.
Every alert in that tab pointed at a path that is not in the tree, so none of them could be clicked
through to code, and `src/handlers.rs` is ambiguous between the two crates besides. A few results carry
absolute paths (`/var/www/html/java-debugging-mcp/mcp-server/clippy.toml`) under the same base id, which
is the same bug from the other end.

**Note-level results do not belong in code scanning.** The gate is `--fail-on warning`, so `note` findings
never fail anything — but the upload published them anyway, and they accumulated into a security tab
reading "115 open alerts" for a repository whose gate is green. Two rules produced all of them:
`excessive-clone` (109, every one the same sentence, and it says "*If* the type implements Copy" because it
has no type information — the flagged sites are `Arc` handle clones and owned values moved into a
dispatcher) and `skipped-pass` (6, which is not a finding about the code at all — it says a tool was not
installed, anchored to `Cargo.toml` for lack of anywhere better).

So: publish `warning` and `error` with paths that resolve, and account for everything withheld on stdout.
The full SARIF is still uploaded as a build artifact and `scripts/doctor.sh --findings` still prints
locally, so nothing becomes unavailable — but "not in the security tab" must not be the same as "not
mentioned anywhere", which is why the caller is expected to put this summary in the job summary.

Exit status is 0 even when nothing survives the filter: an empty result set is a valid upload and is what
closes alerts GitHub is still holding from before this existed.
"""

import json
import subprocess
import sys
from pathlib import Path

PUBLISHED_LEVELS = {"warning", "error"}


def workspace_dirs(root: Path) -> list[Path]:
    """Directories a crate-relative path could be relative to, longest-shot last.

    Read from `cargo metadata` rather than globbed, so a crate added to the workspace is covered without
    anyone remembering this file exists. Falls back to any directory holding a Cargo.toml if cargo is not
    on PATH — this has to work in a checkout that has not been built.
    """
    try:
        out = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            capture_output=True,
            text=True,
            check=True,
            cwd=root,
        ).stdout
        members = [Path(p["manifest_path"]).parent for p in json.loads(out)["packages"]]
    except (OSError, subprocess.CalledProcessError, KeyError, json.JSONDecodeError):
        members = [p.parent for p in root.glob("*/Cargo.toml")]
    # The root itself first: a path that already resolves is never rewritten.
    return [root, *sorted(set(members))]


def resolve(uri: str, root: Path, candidates: list[Path]) -> tuple[str, str]:
    """Map one SARIF URI to a repo-root-relative path.

    Returns (path, why) where `why` is empty on success and explains the failure otherwise. Ambiguity is a
    failure, not a coin flip: `src/lib.rs` exists in both crates, and guessing would put an alert on the
    wrong file, which is worse than leaving it where a human can see it is wrong.
    """
    raw = Path(uri)
    if raw.is_absolute():
        try:
            rel = raw.relative_to(root)
        except ValueError:
            return uri, f"absolute path outside the workspace: {uri}"
        return (rel.as_posix(), "") if (root / rel).exists() else (rel.as_posix(), f"no such file: {rel}")

    hits = [d for d in candidates if (d / raw).exists()]
    if not hits:
        return uri, f"no such file under the workspace root or any crate: {uri}"
    if len(hits) > 1:
        where = ", ".join((h.relative_to(root).as_posix() or ".") for h in hits)
        return uri, f"ambiguous across crates ({where}): {uri}"
    return (hits[0] / raw).relative_to(root).as_posix(), ""


def main() -> int:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <input.sarif> <output.sarif>", file=sys.stderr)
        return 2
    src, dst = Path(sys.argv[1]), Path(sys.argv[2])
    root = Path(__file__).resolve().parent.parent
    candidates = workspace_dirs(root)

    # A scan that did not happen must not publish anything. `--sarif` redirects to a file, so a rust-doctor
    # that dies early leaves it empty or truncated — and an empty *result set* is meaningful here: it is what
    # closes alerts code scanning is holding. Publishing one from a run that produced no data would report
    # "all clear" on the strength of a crash, so this fails the step instead and nothing is uploaded.
    try:
        sarif = json.loads(src.read_text())
    except (OSError, json.JSONDecodeError) as e:
        print(f"error: {src} is not a SARIF document ({e}).", file=sys.stderr)
        print("       rust-doctor did not complete a scan, so there is nothing to publish — and", file=sys.stderr)
        print("       publishing an empty result set would close alerts on the strength of a crash.", file=sys.stderr)
        return 1
    withheld: dict[tuple[str, str], int] = {}
    unresolved: list[str] = []
    published = 0

    for run in sarif.get("runs", []):
        kept = []
        for result in run.get("results", []):
            level = result.get("level", "none")
            if level not in PUBLISHED_LEVELS:
                key = (level, result.get("ruleId", "?"))
                withheld[key] = withheld.get(key, 0) + 1
                continue
            for loc in result.get("locations", []):
                art = loc.get("physicalLocation", {}).get("artifactLocation")
                if not art or "uri" not in art:
                    continue
                fixed, why = resolve(art["uri"], root, candidates)
                art["uri"] = fixed
                # The base id is the actual defect: it names a base nothing declares. A plain
                # repo-relative uri needs none, so drop it rather than invent a mapping for it.
                art.pop("uriBaseId", None)
                if why:
                    unresolved.append(f"{result.get('ruleId', '?')}: {why}")
            kept.append(result)
            published += 1
        run["results"] = kept

    dst.write_text(json.dumps(sarif))

    total = published + sum(withheld.values())
    print(f"## rust-doctor → code scanning: published {published} of {total} result(s)")
    print()
    if withheld:
        print("Withheld (below the `--fail-on warning` gate, so they never failed anything; the full SARIF")
        print("is in the `rust-doctor-sarif` artifact and `scripts/doctor.sh --findings` prints locally):")
        print()
        for (level, rule), n in sorted(withheld.items(), key=lambda kv: -kv[1]):
            print(f"- `{rule}` × {n} ({level})")
        print()
    if unresolved:
        print("**Paths that could not be resolved to a file in this tree** — published as-is, so the alert")
        print("exists but will not link to code. Fix the emitter, not this list:")
        print()
        for line in sorted(set(unresolved)):
            print(f"- {line}")
        print()
    if not published:
        print("Nothing to publish. The upload still happens: an empty result set is what tells code scanning")
        print("that alerts it is holding from earlier runs are gone.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
