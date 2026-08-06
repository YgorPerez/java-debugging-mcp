#!/usr/bin/env python3
"""Status line for this repo: the states that are true, consequential, and otherwise invisible.

fallow's `tt-statusline.py` reads `.trigger-tree/` session state and is meaningless here, so this
is the same *idea* — surface what you would otherwise have to remember to check — applied to this
repo's own standing traps. Four of them, each earning its slot:

  branch      `main` is shown in amber. Committing straight to main is normal here, but the harness
              default is to branch first, so the reminder is worth one word.
  dirty       Uncommitted file count. Cheap and obvious, and the anchor the rest hang off.
  ahead       Commits on this branch that the upstream does not have. This repo's release flow
              COMMITS AND TAGS BUT DELIBERATELY DOES NOT PUSH (`scripts/release.sh`), so "ahead"
              is the expected steady state after a release rather than an anomaly — which is
              exactly why it needs a display: an expected anomaly is one nobody notices growing.
  tag         An annotated tag pointing at HEAD that the remote does not have. This is the single
              highest-value cell. After `/release` the tree looks ordinary while carrying an
              unpushed tag, and `.claude/commands/release.md` warns that repairing a release
              commit means RE-TAGGING because an annotated tag names one commit and amending
              leaves it pointing at an object no longer on the branch. A tag you forgot is local
              is how you get there.

Stdlib only, no network, and every git call is bounded by a timeout. Any failure degrades to a
quieter line rather than an error: a status line that can break the session is worse than none.
"""

import json
import os
import subprocess
import sys
from pathlib import Path

RESET = "\033[0m"
DIM = "\033[38;5;245m"
AMBER = "\033[38;5;178m"
GREEN = "\033[1;38;5;114m"
RED = "\033[1;38;5;203m"


def git(repo: Path, *args: str) -> str:
    """Run git and return stripped stdout, or "" on any failure.

    Swallowing errors is deliberate and matches `scripts/release-notes.py`'s reasoning: every
    caller here has a sensible empty answer, and the alternative is a status line that vanishes
    because a `git describe` disliked a shallow clone.
    """
    try:
        out = subprocess.run(
            ["git", *args],
            cwd=repo,
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    return out.stdout.strip() if out.returncode == 0 else ""


def workspace_version(repo: Path) -> str:
    """`version = "…"` from the first `[workspace.package]` entry. No TOML parser in stdlib < 3.11."""
    try:
        lines = (repo / "Cargo.toml").read_text(encoding="utf-8").splitlines()
    except OSError:
        return ""
    in_section = False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("["):
            in_section = stripped == "[workspace.package]"
            continue
        if in_section and stripped.startswith("version"):
            _, _, value = stripped.partition("=")
            return value.strip().strip('"')
    return ""


def unpushed_tag(repo: Path) -> str:
    """An annotated tag on HEAD that the remote does not carry.

    `git ls-remote` would be authoritative but costs a network round trip on every refresh, which
    is not acceptable in a status line. `refs/tags` under `remotes/` is not a thing either, so the
    local proxy is: does the tag's commit exist on the upstream branch? A tag on an unpushed commit
    is certainly unpushed. This under-reports (a tag pushed separately from its commit looks
    unpushed) and never over-reports the safe direction, which is the right way round.
    """
    tag = git(repo, "tag", "--points-at", "HEAD")
    if not tag:
        return ""
    name = tag.splitlines()[0]
    upstream = git(repo, "rev-parse", "--abbrev-ref", "@{u}")
    if not upstream:
        return name
    contains = git(repo, "branch", "-r", "--contains", "HEAD")
    return "" if upstream in contains else name


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except (json.JSONDecodeError, ValueError):
        payload = {}

    root = (
        payload.get("workspace", {}).get("project_dir")
        or os.environ.get("CLAUDE_PROJECT_DIR")
        or os.getcwd()
    )
    repo_root = git(Path(root), "rev-parse", "--show-toplevel")
    if not repo_root:
        print(f"{DIM}not a git repo{RESET}")
        return 0
    repo = Path(repo_root)

    cells: list[str] = []

    branch = git(repo, "rev-parse", "--abbrev-ref", "HEAD") or "detached"
    cells.append(f"{AMBER}{branch}{RESET}" if branch == "main" else f"{GREEN}{branch}{RESET}")

    if version := workspace_version(repo):
        cells.append(f"{DIM}v{version}{RESET}")

    porcelain = git(repo, "status", "--porcelain")
    dirty = len([line for line in porcelain.splitlines() if line.strip()])
    cells.append(f"{DIM}clean{RESET}" if dirty == 0 else f"{AMBER}{dirty} dirty{RESET}")

    if ahead := git(repo, "rev-list", "--count", "@{u}..HEAD"):
        if ahead.isdigit() and int(ahead) > 0:
            cells.append(f"{AMBER}⇡{ahead} unpushed{RESET}")

    if tag := unpushed_tag(repo):
        cells.append(f"{RED}⇡tag {tag}{RESET}")

    print(f" {DIM}·{RESET} ".join(cells))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SystemExit:
        raise
    except Exception:
        print("")
        raise SystemExit(0)
