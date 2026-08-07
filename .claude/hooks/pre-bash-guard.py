#!/usr/bin/env python3
"""Claude Code `PreToolUse` adapter for `scripts/guard.py`. It holds no rules.

LINT-7 (#167). Every rule used to live here, which meant five of the seven held for exactly one host:
a plain shell, a different agent, or a script in CI got none of them, and the tree could not check
itself against its own documented policy. The rules moved to `scripts/guard.py`, where any host can
reach them, and what is left is the translation between this host's protocol and that checker.

This file's whole job is three lines of mapping:

    allow -> no output          (the normal permission flow continues)
    warn  -> additionalContext  (the model reads it; the command still runs)
    ask   -> permissionDecision (escalate to the user)
    deny  -> permissionDecision (refuse, with the reason and the escape hatch)

Nothing here decides anything. If you are changing what is guarded, change `scripts/guard.py` — a rule
with two implementations is the drift this repo keeps writing post-mortems about, and this file existing
at all is only justified while it stays a translation.
"""

import json
import sys
from pathlib import Path

# `.claude/hooks/<this>` -> the repo root -> `scripts/`. Resolved from __file__ rather than from the
# payload's `cwd`, because the checker is what knows how to recognise this repo and we need to import it
# before we can ask it anything.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))

try:
    from guard import check  # pyright: ignore[reportMissingImports]  # resolved via sys.path above
except ImportError:  # pragma: no cover - a guard that cannot load must not block the session
    print("pre-bash-guard: scripts/guard.py is missing, allowing command", file=sys.stderr)
    raise SystemExit(0) from None


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except (json.JSONDecodeError, ValueError):
        return 0

    if payload.get("tool_name") != "Bash":
        return 0

    command = str(payload.get("tool_input", {}).get("command", "") or "")
    verdict, reason = check(command, cwd=payload.get("cwd"))

    if verdict == "allow":
        return 0
    if verdict == "warn":
        # No decision, so the normal permission flow is untouched; the model reads the note.
        return emit({"additionalContext": reason})
    return emit({"permissionDecision": verdict, "permissionDecisionReason": reason})


def emit(payload: dict) -> int:
    print(json.dumps({"hookSpecificOutput": {"hookEventName": "PreToolUse", **payload}}))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SystemExit:
        raise
    except Exception:
        # A guard that crashes must not block the session. Fail open, loudly enough to notice.
        print("pre-bash-guard: internal error, allowing command", file=sys.stderr)
        raise SystemExit(0)
