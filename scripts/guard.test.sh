#!/usr/bin/env bash
# Test matrix for scripts/guard.py. Run: bash scripts/guard.test.sh
#
# This file exists because the guard's two real bugs were both found here and neither was visible
# in the code. `shlex.split` leaves `40);` as ONE token, so `;` never separated and the soak-loop
# rule silently matched nothing; then `punctuation_chars=True` re-grouped `)` and `;` into `);`
# and it silently matched nothing again. Both read as correct.
#
# Half the cases are must-NOT-fire, and they are the half that matters. A guard that fires on a
# heredoc, an `echo`, or a `grep` of the docs is a guard that gets switched off within the day.
#
# IT DRIVES THE CHECKER DIRECTLY NOW (LINT-7, #167), not a Claude Code hook payload. That is the point
# of the refactor: the rules are host-neutral, so their tests must be runnable by any host too. The last
# three cases are the exception and they earn it — they go through `.claude/hooks/pre-bash-guard.py` to
# prove the ADAPTER still renders each verdict into that host's JSON. Without them the translation could
# break while every rule below stayed green, which is the same silence the rules themselves guard against.

set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

pass=0
fail=0

check() {
  local want="$1" desc="$2" cmd="$3"
  local got
  got=$(python3 scripts/guard.py check --json -- "$cmd" 2>/dev/null |
    python3 -c "import json,sys; print(json.load(sys.stdin)['verdict'])")
  if [ "$got" = "$want" ]; then
    pass=$((pass + 1))
    printf '  ok   %-34s %s\n' "$desc" "$got"
  else
    fail=$((fail + 1))
    printf '  FAIL %-34s want %s, got %s\n' "$desc" "$want" "$got"
  fi
}

echo "Rules that must fire:"
check deny  "RUSTC_BOOTSTRAP on cargo"      'RUSTC_BOOTSTRAP=1 cargo test --workspace'
check ask   "git push"                      'git push origin main'
check ask   "git push, chained"             'cargo fmt --all && git push'
check warn  "literal --shard"               'scripts/integration-test.sh --shard 1/2'
check warn  "--test-threads override"       'cargo test --test mcp_integration -- --ignored --test-threads 4'
check warn  "--test-threads=N joined form"  'cargo test -- --ignored --test-threads=4'
check warn  "--shard=N/M joined form"       'scripts/integration-test.sh --shard=1/2'
check warn  "JDWP_TEST_THREADS override"    'while true; do JDWP_TEST_THREADS=8 /tmp/arm.bin; done'
check warn  "unbounded workspace cargo"     'cargo build --workspace'
check warn  "unbounded, time-prefixed"      'time cargo test --workspace'
check warn  "soak loop on cargo test"       'for i in $(seq 40); do cargo test --test mcp_integration -- --ignored force_return; done'

echo
echo "Rules that must NOT fire (the half that keeps the guard switched on):"
check allow "plain ls"                      'ls -la'
check allow "BOOTSTRAP on the test binary"  'RUSTC_BOOTSTRAP=1 ./target/debug/deps/mcp_integration-abc --ignored'
check allow "shard-plan --which"            'scripts/shard-plan.py --tests f --which launch_suspends'
# THESE THREE ARE THE REGRESSION, and they were found in the wild rather than imagined. Both rules used
# to search the RAW command line, which the module docstring explicitly says not to do — so recording a
# soak result with `gh issue comment` made the guard fire on the prose it was writing about itself. A
# rule that cries wolf on documentation of that rule is the fastest way to get the guard switched off.
check allow "flag quoted in a gh comment"   'gh issue comment 45 --body "ran with --test-threads 16 under taskset"'
check allow "shard quoted in a gh comment"  'gh issue comment 118 --body "the recipe named --shard 1/2 and it had moved"'
check allow "grep for the flag in the docs" 'grep -rn -- "--test-threads" CLAUDE.md'
check allow "workspace cargo, tailed"       'cargo build --workspace 2>&1 | tail -20'
check allow "workspace cargo, redirected"   'cargo test --workspace > /tmp/o.log 2>&1'
check allow "push named inside an echo"     'echo "remember to git push later"'
check allow "grep for shard in the docs"    'grep -n "shard" CLAUDE.md'
check allow "soak against a copied binary"  'for i in $(seq 40); do /tmp/arm.bin --ignored force_return; done'
check allow "unbalanced quote"              'echo "unterminated'
check allow "heredoc naming BOOTSTRAP"      'cat <<EOF
RUSTC_BOOTSTRAP=1 cargo test
EOF'
# THE CASE ABOVE WAS PASSING BY ACCIDENT, and these two are why it needed company. Its body holds no
# `;`, so the whole line lexed as ONE segment whose argv[0] is `cat` — nothing about the heredoc was
# being skipped, the body simply never started a segment of its own. Add a semicolon, which every
# commit message in this repo has, and the body's second half becomes an argv beginning `cargo test`.
# Found in the wild: the soak-loop rule fired on a `git commit` whose body said
# "Verified: cargo fmt clean; cargo test 319 passed" and whose prose happened to contain "while".
check allow "commit message quoting a run" 'git commit -F - <<'"'"'EOF'"'"'
One site, not thirty-nine, while a caller can act.
Verified: cargo fmt clean; cargo test 319 passed, 0 failed.
EOF'
check allow "heredoc quoting a shard"      'gh issue comment 118 --body-file - <<EOF
The recipe said; --shard 1/2 and it had moved.
EOF'
# THE SAME BUG IN THE OTHER DIRECTION, and this is the one that mattered. The escape hatch was tested
# with `in command` against the RAW line, so a heredoc body that merely MENTIONED it stood the whole
# guard down — and the commit messages in this repo mention it, because every deny names it. A commit
# explaining the escape therefore ran with no guard at all, and the deny below was allowed outright.
# Found by noticing a commit that should have tripped the soak rule went through TOO quietly.
check deny  "escape merely quoted in a body" 'RUSTC_BOOTSTRAP=1 cargo test <<EOF
every deny documents SKIP_JDWP_AGENT_GUARD=1 as its escape
EOF'
check allow "the wrapper script itself"     'scripts/integration-test.sh'
check allow "the documented escape hatch"   'SKIP_JDWP_AGENT_GUARD=1 RUSTC_BOOTSTRAP=1 cargo test'

# ── the adapter, which is the only part that is host-specific ───────────────────────────────────
#
# One case per verdict shape, because each maps to a DIFFERENT key in Claude Code's reply and getting
# one of them wrong is invisible from the rules: `deny`/`ask` are a `permissionDecision`, `warn` is an
# `additionalContext` with no decision at all (so the normal permission flow is untouched), and `allow`
# is no output whatsoever. A `warn` rendered as a decision would start blocking commands that are meant
# to proceed; an `allow` that printed anything would be a protocol error on every command in the session.
echo
echo "The Claude Code adapter renders each verdict into that host's JSON:"

hook() {
  local want="$1" desc="$2" cmd="$3"
  local got
  got=$(printf '%s' "$cmd" | python3 -c "
import json, sys
print(json.dumps({'tool_name': 'Bash', 'cwd': '$PWD',
                  'tool_input': {'command': sys.stdin.read()}}))" |
    python3 .claude/hooks/pre-bash-guard.py |
    python3 -c "
import json, sys
raw = sys.stdin.read().strip()
if not raw:
    print('no-output')
else:
    out = json.loads(raw)['hookSpecificOutput']
    print(out.get('permissionDecision') or ('additionalContext' if 'additionalContext' in out else '?'))")
  if [ "$got" = "$want" ]; then
    pass=$((pass + 1))
    printf '  ok   %-34s %s\n' "$desc" "$got"
  else
    fail=$((fail + 1))
    printf '  FAIL %-34s want %s, got %s\n' "$desc" "$want" "$got"
  fi
}

hook deny              "deny -> permissionDecision"  'RUSTC_BOOTSTRAP=1 cargo test --workspace'
hook ask               "ask -> permissionDecision"   'git push origin main'
hook additionalContext "warn -> additionalContext"   'cargo build --workspace'
hook no-output         "allow -> nothing at all"     'ls -la'

echo
if [ "$fail" -eq 0 ]; then
  echo "$pass passed, 0 failed"
  exit 0
fi
echo "$pass passed, $fail FAILED"
exit 1
