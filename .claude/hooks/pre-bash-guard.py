#!/usr/bin/env python3
"""Pre-Bash guard for agent sessions in this repo.

Every rule here corresponds to a trap already written down in `CLAUDE.md`, and each one is there
because it cost somebody real time. The file is not a general safety net: a rule earns its place by
naming an incident, and a rule nobody can attach an incident to should be deleted rather than kept
"just in case". Prose that has to be read to be obeyed is obeyed inconsistently; this is the half
that does not depend on reading.

WHY A TOKEN WALK RATHER THAN A REGEX OVER THE RAW STRING. The tokenizer is adapted from fallow's
`.claude/hooks/pre-bash-guard.py` (MIT), and the reason to copy it rather than grep is that this
repo's commands routinely *mention* the things being guarded as data — a heredoc writing a doc, an
`echo` of a recipe, a `grep` for `--shard` in `CLAUDE.md`. `shlex.split` is quote-aware, so those
stay data, while chained and env-prefixed real invocations (`cargo build && git push`,
`A=1 cargo test`) are still seen. An unbalanced quote returns None and the guard stands down: a
line we cannot tokenize is one we must not guess about.

TWO SEVERITIES, AND THE SPLIT IS DELIBERATE:

  deny()  — the command is silently wrong. It will appear to work and produce an answer that is
            not about what you think. `RUSTC_BOOTSTRAP=1 cargo test` is the type case.
  warn()  — the command is probably not what you meant, but "probably" is doing real work and a
            block would strand a legitimate run. Emitted as `additionalContext`, which the model
            reads while the command proceeds.

`ask()` is used exactly once, for `git push`, because "the user has not heard about this yet" is
precisely what escalating to the user fixes.

Every deny names `SKIP_JDWP_AGENT_GUARD=1` as the escape. An escape hatch that is documented in the
denial is what keeps the guard from being disabled wholesale the first time it is wrong.
"""

import json
import os
import re
import shlex
import subprocess
import sys
from pathlib import Path

SEPARATORS = {";", "&&", "||", "|", "&", "|&"}
# Cargo subcommands whose full-workspace output floods the context window when unredirected.
CARGO_NOISY = {"build", "test", "clippy", "doc", "check"}
# Commands that, in the final pipeline position, bound what actually reaches the terminal.
BOUNDING_PAGERS = {"tail", "head", "less", "more", "wc", "grep", "rg", "jq"}
ENV_ASSIGN = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
# Shell keywords that sit in command position after a separator and are never the command itself.
# Without these, `for …; do cargo test; done` parses with `do` as argv[0] and the cargo rules all
# miss — which is exactly how the soak-loop rule failed its first test.
COMMAND_PREFIXES = {"do", "then", "else", "elif", "{", "(", "!", "time", "exec", "nohup"}
# `punctuation_chars=True` groups ADJACENT operators into one token, so `$(seq 40); do …` yields
# `);` rather than `)` and `;`. That token matches no separator, the line collapses to a single
# segment, and every command-position rule misses. Split punctuation runs back apart, longest
# operator first so `&&` survives.
PUNCT_CHARS = set("();|&<>")
PUNCT_OPERATORS = ("&&", "||", "|&", ">>", "<<", ";", "|", "&", "(", ")", ">", "<")
# `--shard 1/2`, `--shard=1/2`. See shard_number_is_hardcoded for why this is only a warning.
SHARD = re.compile(r"--shard[= ]\s*(\d+)\s*/\s*(\d+)")
LOOP_KEYWORD = re.compile(r"\b(for|while|until)\b")


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except (json.JSONDecodeError, ValueError):
        return 0

    if payload.get("tool_name") != "Bash":
        return 0

    command = str(payload.get("tool_input", {}).get("command", "") or "")
    if not command or "SKIP_JDWP_AGENT_GUARD=1" in command:
        return 0

    cwd = Path(str(payload.get("cwd") or os.getcwd())).resolve()
    repo = find_repo_root(cwd)
    if repo is None:
        return 0

    commands = command_positions(command)
    if commands is None:
        return 0

    # --- deny: silently wrong -------------------------------------------------------------

    if bootstraps_cargo(commands):
        return deny(
            "`RUSTC_BOOTSTRAP=1` is hashed into the build fingerprint, so setting it on `cargo` "
            "recompiles the whole workspace AND compiles it under a flag that lets nightly-only "
            "features in silently (CLAUDE.md, TEST-32). `scripts/integration-test.sh` exists to "
            "avoid exactly this: it builds with `cargo test --no-run` and runs the test binary "
            "directly, so the variable never reaches cargo. Use the script, or set the variable on "
            "the test binary rather than on cargo. Override with SKIP_JDWP_AGENT_GUARD=1."
        )

    if commits_via_git(commands) and (misformatted := unformatted_files(repo)):
        listing = ", ".join(misformatted[:5]) + ("…" if len(misformatted) > 5 else "")
        return deny(
            f"`cargo fmt --check` fails on {len(misformatted)} file(s): {listing}. CI fails on a "
            "misformatted diff (LINT-4, #44), so this commit would go red. Run `cargo fmt --all`, "
            "then re-stage — note the re-stage: formatting after `git add` leaves the fix "
            "UNSTAGED and commits the unformatted version, which is why this guard refuses rather "
            "than silently running fmt for you. Override with SKIP_JDWP_AGENT_GUARD=1."
        )

    # --- ask: the user has not heard about this yet ---------------------------------------

    if pushes_via_git(commands):
        return ask(
            "This pushes to a remote. Work in this repo is carried to completion and committed, "
            "but pushing is the user's call — `scripts/release.sh` deliberately stops before the "
            "push for the same reason. Confirm this is wanted."
        )

    # --- warn: probably not what you meant ------------------------------------------------

    notes = [
        note
        for note in (
            soaks_against_the_working_tree(command, commands),
            shard_number_is_hardcoded(command),
            overrides_test_threads(command, commands),
            floods_the_context(command, commands),
        )
        if note
    ]
    if notes:
        return warn("\n\n".join(notes))

    return 0


# --------------------------------------------------------------------------------------------
# Rules
# --------------------------------------------------------------------------------------------


def bootstraps_cargo(commands: list[list[str]]) -> bool:
    """`RUSTC_BOOTSTRAP=1 cargo …` — the env assignment must be on cargo itself to matter.

    Checked before `strip_env` discards the assignments, so the raw segments are re-derived here.
    Setting it on the test binary (`RUSTC_BOOTSTRAP=1 ./target/debug/deps/mcp_integration-…`) is
    the supported form and must NOT fire.
    """
    return any(
        any(token == "RUSTC_BOOTSTRAP=1" for token in raw[:index])
        and Path(raw[index]).name == "cargo"
        for raw, index in ((seg, env_prefix_len(seg)) for seg in commands_with_env())
        if index < len(raw)
    )


def soaks_against_the_working_tree(command: str, commands: list[list[str]]) -> str | None:
    """A loop that re-invokes `cargo test` rebuilds mid-soak and reports your edits as failures."""
    if not LOOP_KEYWORD.search(command):
        return None
    if not any(Path(argv[0]).name == "cargo" and len(argv) > 1 and argv[1] == "test" for argv in commands):
        return None
    return (
        "This looks like a soak loop invoking `cargo test` directly. An arm that rebuilds while "
        "you edit reports YOUR compile errors as test failures — CLAUDE.md records a confident "
        '"8 failures in 40" that were nothing of the kind. Copy the binary first:\n'
        "    cp $(cargo test --no-run --message-format=json 2>/dev/null | "
        'jq -r "select(.executable) | .executable" | tail -1) /tmp/arm.bin\n'
        "then loop over /tmp/arm.bin."
    )


def shard_number_is_hardcoded(command: str) -> str | None:
    """A literal `--shard N/M` copied from prose is stale by construction."""
    match = SHARD.search(command)
    if not match or "shard-plan.py" in command:
        return None
    return (
        f"`--shard {match.group(1)}/{match.group(2)}` is a literal shard number. The split is by "
        "MEASURED duration and moves whenever `timings.tsv` is refreshed, so a number copied from "
        "a doc or an issue is stale by construction — #118's recipe named `--shard 1/2`, six runs "
        "of it passed cleanly, and the test had moved to shard 2/2. Confirm membership first:\n"
        "    scripts/shard-plan.py --tests <(<the-test-binary> --ignored --list) --which <name>\n"
        "or prefer the unsharded form, which has no number to rot."
    )


def overrides_test_threads(command: str, commands: list[list[str]]) -> str | None:
    """Overriding the thread count stops the run being CI-shaped."""
    explicit = "--test-threads" in command
    env = any(token.startswith("JDWP_TEST_THREADS=") for seg in commands_with_env() for token in seg)
    if not (explicit or env):
        return None
    return (
        "Overriding the test-thread count changes the concurrency shape and stops this being a "
        "reproduction of CI. `scripts/integration-test.sh` computes 4x cores capped at 40 "
        "(TEST-32) and prints it; under `taskset -c 0-3` that comes out at 16, which is exactly "
        "what CI passes. If you are chasing a flake, the contention IS the variable — pin the "
        "whole suite with taskset instead of changing the thread count."
    )


def floods_the_context(command: str, commands: list[list[str]]) -> str | None:
    """Unbounded full-workspace cargo output. Warn only — a long run may be exactly the point."""
    noisy = any(
        Path(argv[0]).name == "cargo"
        and len(argv) >= 2
        and argv[1] in CARGO_NOISY
        and ("--workspace" in argv or "--all-targets" in argv)
        for argv in commands
    )
    if not noisy or output_is_bounded(command, commands):
        return None
    return (
        "Full-workspace cargo output is unbounded here. Prefer "
        "`… > /tmp/jdwp-build.log 2>&1; tail -80 /tmp/jdwp-build.log` so a long compile does not "
        "consume the context window. Proceeding anyway."
    )


# --------------------------------------------------------------------------------------------
# Plumbing
# --------------------------------------------------------------------------------------------

_RAW_SEGMENTS: list[list[str]] = []


def commands_with_env() -> list[list[str]]:
    """Segments WITH their env assignments intact, for rules that care about the prefix."""
    return _RAW_SEGMENTS


def env_prefix_len(segment: list[str]) -> int:
    """How many leading tokens are env assignments or shell keywords rather than the command."""
    index = 0
    while index < len(segment) and (
        ENV_ASSIGN.match(segment[index]) or segment[index] in COMMAND_PREFIXES
    ):
        index += 1
    return index


def split_punctuation(token: str) -> list[str]:
    """Split a run of shell operators into individual operators; pass anything else through."""
    if len(token) < 2 or not all(char in PUNCT_CHARS for char in token):
        return [token]

    pieces: list[str] = []
    rest = token
    while rest:
        for operator in PUNCT_OPERATORS:
            if rest.startswith(operator):
                pieces.append(operator)
                rest = rest[len(operator) :]
                break
        else:  # not reachable while every PUNCT_CHARS member is a single-char operator
            pieces.append(rest[0])
            rest = rest[1:]
    return pieces


def find_repo_root(cwd: Path) -> Path | None:
    """Gate on committed sentinels so the guard activates on every clone, not just this machine."""
    try:
        out = subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"],
            cwd=cwd,
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
    except (OSError, subprocess.CalledProcessError):
        return None

    root = Path(out).resolve()
    if (root / "rust-doctor.toml").is_file() and (root / "jdwp-client").is_dir():
        return root
    return None


def command_positions(command: str) -> list[list[str]] | None:
    """Split a command line into the argv of each pipeline/list segment.

    Returns None when the line cannot be tokenized — an unbalanced quote means we should not guess.
    Populates the module-level raw segments as a side effect, so rules that need the env prefix
    (`bootstraps_cargo`, `overrides_test_threads`) can see it while the rest get it stripped.
    """
    global _RAW_SEGMENTS
    try:
        # `punctuation_chars=True` is what makes `;`, `|`, `&&` separate tokens. Plain
        # `shlex.split` splits on whitespace only, so `$(seq 40); do cargo test` yields the single
        # token `40);` and the whole line collapses into one segment whose argv[0] is `for` — the
        # soak-loop rule silently matched nothing. Found by the test case, not by reading.
        lexer = shlex.shlex(command, posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        tokens = [piece for token in lexer for piece in split_punctuation(token)]
    except ValueError:
        return None

    segments: list[list[str]] = []
    current: list[str] = []
    for token in tokens:
        if token in SEPARATORS:
            if current:
                segments.append(current)
                current = []
        else:
            current.append(token)
    if current:
        segments.append(current)

    _RAW_SEGMENTS = [seg for seg in segments if seg]
    return [argv for argv in (seg[env_prefix_len(seg) :] for seg in _RAW_SEGMENTS) if argv]


def output_is_bounded(command: str, commands: list[list[str]]) -> bool:
    if ">" in command:
        return True
    # `tee` deliberately does not count: it passes everything through to stdout.
    return bool(commands) and Path(commands[-1][0]).name in BOUNDING_PAGERS


def commits_via_git(commands: list[list[str]]) -> bool:
    return any(Path(a[0]).name == "git" and len(a) >= 2 and a[1] == "commit" for a in commands)


def pushes_via_git(commands: list[list[str]]) -> bool:
    return any(Path(a[0]).name == "git" and len(a) >= 2 and a[1] == "push" for a in commands)


def unformatted_files(repo: Path) -> list[str]:
    """Files `cargo fmt` would rewrite. Empty on any failure — never block on a broken toolchain."""
    try:
        proc = subprocess.run(
            ["cargo", "fmt", "--all", "--", "--check", "-l"],
            cwd=repo,
            capture_output=True,
            text=True,
            timeout=60,
        )
    except (OSError, subprocess.SubprocessError):
        return []
    if proc.returncode == 0:
        return []
    return [
        str(Path(line.strip()).relative_to(repo))
        if line.strip().startswith(str(repo))
        else line.strip()
        for line in proc.stdout.splitlines()
        if line.strip()
    ]


# --------------------------------------------------------------------------------------------
# Hook protocol
# --------------------------------------------------------------------------------------------


def _emit(payload: dict) -> int:
    print(json.dumps({"hookSpecificOutput": {"hookEventName": "PreToolUse", **payload}}))
    return 0


def deny(reason: str) -> int:
    return _emit({"permissionDecision": "deny", "permissionDecisionReason": reason})


def ask(reason: str) -> int:
    return _emit({"permissionDecision": "ask", "permissionDecisionReason": reason})


def warn(context: str) -> int:
    """Non-blocking: `additionalContext` with no decision leaves the normal permission flow."""
    return _emit({"additionalContext": context})


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SystemExit:
        raise
    except Exception:
        # A guard that crashes must not block the session. Fail open, loudly enough to notice.
        print("pre-bash-guard: internal error, allowing command", file=sys.stderr)
        raise SystemExit(0)
