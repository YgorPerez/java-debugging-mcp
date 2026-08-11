#!/usr/bin/env python3
"""Check a shell command against the traps `CLAUDE.md` documents. One implementation, any host.

    scripts/guard.py check 'RUSTC_BOOTSTRAP=1 cargo test'   # deny, with the reason; exit 20
    scripts/guard.py check --json 'git push'                # {"verdict": "ask", "reason": "…"}
    bash scripts/guard.test.sh                              # the matrix

Every rule here corresponds to a trap already written down in `CLAUDE.md`, and each one is there
because it cost somebody real time. The file is not a general safety net: a rule earns its place by
naming an incident, and a rule nobody can attach an incident to should be deleted rather than kept
"just in case". Prose that has to be read to be obeyed is obeyed inconsistently; this is the half
that does not depend on reading.

WHY IT LIVES IN `scripts/` AND NOT IN `.claude/` (LINT-7, #167). It used to be a Claude Code
`PreToolUse` hook and nothing else, so five of its seven rules held for exactly one host: a plain
shell, a different agent, or a script in CI got none of them, and the tree could not check itself
against its own documented policy. fallow names the same gap out loud in its quality-gates doc —
"Codex does not execute `.claude/settings.json` hooks. Mirror the repository hooks manually when they
did not run." The rules are here now, and `.claude/hooks/pre-bash-guard.py` is a short adapter that
calls `check()` and renders this host's JSON. There is deliberately NO second implementation of any
rule: a rule with two of them is the drift this repo keeps writing post-mortems about.

WHAT A NON-CLAUDE HOST STILL DOES NOT GET, said plainly rather than implied. Nothing calls this
automatically outside Claude Code. The two checked-in git hooks (LINT-6/#146, REL-4/#147) cover the
`cargo fmt` half of one rule and nothing else, and they are opt-in per clone besides. So elsewhere
this is a command you run, not a guard that runs — which is still strictly more than the nothing that
was reachable before, and is why the entry point takes a COMMAND LINE rather than a hook payload.

WHY A TOKEN WALK RATHER THAN A REGEX OVER THE RAW STRING. The tokenizer is adapted from fallow's
`.claude/hooks/pre-bash-guard.py` (MIT), and the reason to copy it rather than grep is that this
repo's commands routinely *mention* the things being guarded as data — a heredoc writing a doc, an
`echo` of a recipe, a `grep` for `--shard` in `CLAUDE.md`. `shlex.split` is quote-aware, so those
stay data, while chained and env-prefixed real invocations (`cargo build && git push`,
`A=1 cargo test`) are still seen. An unbalanced quote returns None and the guard stands down: a
line we cannot tokenize is one we must not guess about.

**Quote-awareness covers the `echo` and never covered the heredoc**, and that gap was live for as long
as this paragraph has claimed otherwise: a heredoc body is not quoted, so every line of it was lexed as
though it were a command. `strip_heredoc_bodies` runs in `check` before anything reads the string, which
is what makes the sentence above true of raw-string rules and the escape hatch as well as of the token
walk. It cost two bugs in opposite directions: the soak-loop rule fired on `git commit` messages, and a
message that merely MENTIONED `SKIP_JDWP_AGENT_GUARD=1` stood the entire guard down.

TWO SEVERITIES, AND THE SPLIT IS DELIBERATE:

  deny()  — the command is silently wrong. It will appear to work and produce an answer that is
            not about what you think. `RUSTC_BOOTSTRAP=1 cargo test` is the type case.
  warn()  — the command is probably not what you meant, but "probably" is doing real work and a
            block would strand a legitimate run. Emitted as `additionalContext`, which the model
            reads while the command proceeds.

`ask` is used exactly once, for `git push`, because "the user has not heard about this yet" is
precisely what escalating to the user fixes.

The RATIONALE for each severity — which rule is which, and why — lives in `.claude/settings.json`'s
comment block and is deliberately not restated here or in `CLAUDE.md`. One place to change it.

Every deny names `SKIP_JDWP_AGENT_GUARD=1` as the escape. An escape hatch that is documented in the
denial is what keeps the guard from being disabled wholesale the first time it is wrong.
"""

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
from pathlib import Path

# The verdicts, and their exit codes for a shell caller. `warn` exits 0 on purpose: the command still
# runs, so a caller using this as a pre-flight must not be stopped by it. `ask` and `deny` are distinct
# numbers rather than a shared 1, because "escalate to a human" and "refuse" are different answers and a
# script wiring this into another host needs to tell them apart.
EXIT_CODES = {"allow": 0, "warn": 0, "ask": 10, "deny": 20}

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
# `--shard=1/2` as one token, and the `1/2` that follows a bare `--shard`. Matched per TOKEN rather
# than over the raw line: see shard_number_is_hardcoded. Only a warning, for the reason given there.
SHARD_JOINED = re.compile(r"^--shard=(\d+)/(\d+)$")
SHARD_VALUE = re.compile(r"^(\d+)/(\d+)$")
# `--test-threads=16`. The bare `--test-threads` form is matched by token with a digit required after it.
THREADS_JOINED = re.compile(r"^--test-threads=\d+$")
LOOP_KEYWORD = re.compile(r"\b(for|while|until)\b")

# The start of a heredoc: `<<WORD`, `<<'WORD'`, `<<"WORD"`, or the tab-stripping `<<-WORD`. The
# `(?!<)` is load-bearing — `<<<` is a here-STRING, which is one line with no body to skip.
HEREDOC_START = re.compile(r"<<(?!<)-?\s*(?P<q>['\"]?)(?P<delim>[A-Za-z_][A-Za-z0-9_]*)(?P=q)")


def check(command: str, cwd: str | Path | None = None) -> tuple[str, str]:
    """The whole policy, as one function: `("allow" | "warn" | "ask" | "deny", reason)`.

    THE ONE ENTRY POINT, and every host goes through it — the Claude Code adapter, the CLI below, and
    the matrix in `scripts/guard.test.sh`. Anything that grows a second way in is how the verdicts start
    to differ by caller.

    `allow` carries an empty reason. Everything else carries the text a human is meant to read, which
    is the same string in every host: a rule whose wording depends on where it fired is a rule two
    people describe differently.
    """
    if not command:
        return "allow", ""

    # BEFORE ANYTHING READS THE STRING, including the escape hatch below. A heredoc body is data on its
    # way to a program's stdin, and every check here — the token walk, the raw-string ones, and the
    # escape — was reading it as though it were part of the command.
    #
    # The escape is the case that matters most, and it is the reverse of a false positive: a body that
    # merely MENTIONS `SKIP_JDWP_AGENT_GUARD=1` used to stand the whole guard down. So a commit message
    # explaining the escape hatch — which the messages in this repo do — turned the guard off for its own
    # commit, and `RUSTC_BOOTSTRAP=1 cargo test <<EOF … SKIP_… … EOF` was allowed outright. Found by
    # noticing that a commit which should have tripped the soak rule went through *too* quietly.
    command = strip_heredoc_bodies(command)

    if "SKIP_JDWP_AGENT_GUARD=1" in command:
        return "allow", ""

    here = Path(str(cwd or os.getcwd())).resolve()
    repo = find_repo_root(here)
    if repo is None:
        return "allow", ""

    commands = command_positions(command)
    if commands is None:
        return "allow", ""

    # --- deny: silently wrong -------------------------------------------------------------

    if bootstraps_cargo(commands):
        return (
            "deny",
            "`RUSTC_BOOTSTRAP=1` is hashed into the build fingerprint, so setting it on `cargo` "
            "recompiles the whole workspace AND compiles it under a flag that lets nightly-only "
            "features in silently (CLAUDE.md, TEST-32). `scripts/integration-test.sh` exists to "
            "avoid exactly this: it builds with `cargo test --no-run` and runs the test binary "
            "directly, so the variable never reaches cargo. Use the script, or set the variable on "
            "the test binary rather than on cargo. Override with SKIP_JDWP_AGENT_GUARD=1."
        )

    if commits_via_git(commands) and (misformatted := unformatted_files(repo)):
        listing = ", ".join(misformatted[:5]) + ("…" if len(misformatted) > 5 else "")
        return (
            "deny",
            f"`cargo fmt --check` fails on {len(misformatted)} file(s): {listing}. CI fails on a "
            "misformatted diff (LINT-4, #44), so this commit would go red. Run `cargo fmt --all`, "
            "then re-stage — note the re-stage: formatting after `git add` leaves the fix "
            "UNSTAGED and commits the unformatted version, which is why this guard refuses rather "
            "than silently running fmt for you. Override with SKIP_JDWP_AGENT_GUARD=1.",
        )

    # --- ask: the user has not heard about this yet ---------------------------------------

    if pushes_via_git(commands):
        return (
            "ask",
            "This pushes to a remote. Work in this repo is carried to completion and committed, "
            "but pushing is the user's call — `scripts/release.sh` deliberately stops before the "
            "push for the same reason. Confirm this is wanted.",
        )

    # --- warn: probably not what you meant ------------------------------------------------

    notes = [
        note
        for note in (
            soaks_against_the_working_tree(command, commands),
            shard_number_is_hardcoded(commands),
            overrides_test_threads(commands),
            floods_the_context(command, commands),
        )
        if note
    ]
    if notes:
        return "warn", "\n\n".join(notes)

    return "allow", ""


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


def shard_number_is_hardcoded(commands: list[list[str]]) -> str | None:
    """A literal `--shard N/M` copied from prose is stale by construction.

    TOKEN-SCOPED, and it was not always. This searched the raw line, which is precisely what the
    module docstring says not to do and for precisely the reason given there: this repo's commands
    routinely MENTION the guarded thing as data. Found in the wild by a `gh issue comment` whose body
    quoted a soak recipe — the guard fired on prose it was writing about itself. A rule that cries wolf
    on documentation is a rule that gets switched off, which is what half of the test matrix is for.
    """
    match = None
    for segment in commands:
        # Asking WHICH shard a test is in is the remedy this rule points at, not the mistake.
        if any(Path(token).name == "shard-plan.py" for token in segment):
            continue
        for index, token in enumerate(segment):
            match = SHARD_JOINED.match(token)
            if not match and token == "--shard" and index + 1 < len(segment):
                match = SHARD_VALUE.match(segment[index + 1])
            if match:
                break
        if match:
            break
    if not match:
        return None
    return (
        f"`--shard {match.group(1)}/{match.group(2)}` is a literal shard number. The split is by "
        "MEASURED duration and moves whenever `timings.tsv` is refreshed, so a number copied from "
        "a doc or an issue is stale by construction — #118's recipe named `--shard 1/2`, six runs "
        "of it passed cleanly, and the test had moved to shard 2/2. Confirm membership first:\n"
        "    scripts/shard-plan.py --tests <(<the-test-binary> --ignored --list) --which <name>\n"
        "or prefer the unsharded form, which has no number to rot."
    )


def overrides_test_threads(commands: list[list[str]]) -> str | None:
    """Overriding the thread count stops the run being CI-shaped.

    Token-scoped for the same reason as `shard_number_is_hardcoded` above, and found the same way: a
    `gh issue comment` recording a soak result quoted `--test-threads 16` in its body and tripped this.
    `shlex` keeps a quoted body as ONE token, so an exact-token test sees the flag only when it is
    really being passed.
    """
    explicit = False
    for segment in commands:
        for index, token in enumerate(segment):
            # A VALUE HAS TO FOLLOW, which is the same discipline `shard_number_is_hardcoded` uses and
            # for the same reason: `grep -rn -- "--test-threads" CLAUDE.md` passes the flag as a
            # standalone token without ever running a test. libtest accepts only `--test-threads N` and
            # `--test-threads=N`, so requiring the number costs nothing real and drops the false positive.
            if THREADS_JOINED.match(token):
                explicit = True
            elif token == "--test-threads" and index + 1 < len(segment):
                explicit = segment[index + 1].isdigit()
            if explicit:
                break
        if explicit:
            break
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


def strip_heredoc_bodies(command: str) -> str:
    """Drop the BODY of every heredoc, keeping the command that opened it.

    **This makes good on a promise the module docstring above already makes.** It says the reason for a
    token walk is that this repo's commands routinely *mention* the guarded thing as data — "a heredoc
    writing a doc, an `echo` of a recipe" — and that `shlex` being quote-aware keeps those as data. That
    is true of the `echo`, whose recipe is inside quotes. It was never true of the heredoc: a body is not
    quoted, so every line of it was lexed as though it were a command.

    Found in the wild, twice in one session, on the shape every commit message in this repo has:

        git commit -F - <<'EOF'
        One site, not thirty-nine, while a caller can act.
        Verified: cargo fmt clean; cargo test 319 passed, 0 failed.
        EOF

    `;` starts a segment, so `cargo test 319 passed` became an argv whose first two tokens are `cargo`
    and `test`; `while` in the prose satisfied the loop keyword; and the soak-loop rule fired on a
    `git commit`. Every token-scoped rule had the same exposure — the fix is here, in the parse, rather
    than in any one rule.

    A body is data being passed to a program's stdin, so no rule should ever read it. The one form where
    that is arguable is `bash <<EOF`, where the body really is commands — and it does not matter, because
    this guard is advisory by construction: every deny documents `SKIP_JDWP_AGENT_GUARD=1` as its escape,
    so there is nothing here to smuggle past. A false positive is the expensive failure, and CLAUDE.md
    says why: a guard that trips on a heredoc gets switched off within the day.

    The terminator is matched with `strip()` rather than at column 0 as POSIX requires for a plain `<<`.
    Deliberately lenient: erring toward skipping more of a body can only reduce false positives, while
    being strict about indentation would leave them in.
    """
    kept: list[str] = []
    # Delimiters awaiting their bodies, in the order the shell will consume them — `cmd <<A <<B` reads
    # A's body first, then B's.
    pending: list[str] = []
    for line in command.split("\n"):
        if pending:
            if line.strip() == pending[0]:
                pending.pop(0)
            continue
        found = list(HEREDOC_START.finditer(line))
        # The OPERATOR goes with its body, which is what makes this idempotent — and idempotence is
        # load-bearing, not tidiness. Leaving `<<EOF` behind would make a second pass queue `EOF`, find
        # no terminator (the first pass took it), and swallow every remaining line. A `<<` with nothing
        # after it is meaningless anyway.
        kept.append(HEREDOC_START.sub("", line) if found else line)
        pending.extend(match.group("delim") for match in found)
    return "\n".join(kept)


def command_positions(command: str) -> list[list[str]] | None:
    """Split a command line into the argv of each pipeline/list segment.

    Returns None when the line cannot be tokenized — an unbalanced quote means we should not guess.
    Populates the module-level raw segments as a side effect, so rules that need the env prefix
    (`bootstraps_cargo`, `overrides_test_threads`) can see it while the rest get it stripped.

    Heredoc bodies are stripped again here even though [`check`] has already done it — the function is
    idempotent for exactly this reason, so a direct caller cannot get the unprotected behaviour.
    """
    global _RAW_SEGMENTS
    command = strip_heredoc_bodies(command)
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
# CLI
# --------------------------------------------------------------------------------------------
#
# There is no hook protocol in this file any more (LINT-7, #167). `.claude/hooks/pre-bash-guard.py`
# renders Claude Code's JSON around `check()`; any other host wires up whatever it needs around the
# same function, or shells out to the command below.


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    sub = ap.add_subparsers(dest="mode", required=True)
    one = sub.add_parser("check", help="check one command line")
    one.add_argument("command", help="the command line, as one argument — quote it")
    one.add_argument("--json", action="store_true", help="emit {verdict, reason} instead of prose")
    one.add_argument("--cwd", help="where the command would run (default: here)")
    args = ap.parse_args(argv)

    verdict, reason = check(args.command, cwd=args.cwd)
    if args.json:
        print(json.dumps({"verdict": verdict, "reason": reason}))
    else:
        print(verdict)
        # Reason to STDERR, so `guard.py check … | read v` gets the verdict and nothing else while a
        # human still sees why.
        if reason:
            print(reason, file=sys.stderr)
    return EXIT_CODES[verdict]


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SystemExit:
        raise
    except Exception:
        # A guard that crashes must not block the caller. Fail open, loudly enough to notice — the same
        # posture the hook adapter takes, for the same reason.
        print("guard: internal error, allowing command", file=sys.stderr)
        raise SystemExit(0)
