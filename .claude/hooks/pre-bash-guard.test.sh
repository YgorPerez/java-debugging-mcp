#!/usr/bin/env bash
# Test matrix for pre-bash-guard.py. Run: bash .claude/hooks/pre-bash-guard.test.sh
#
# This file exists because the guard's two real bugs were both found here and neither was visible
# in the code. `shlex.split` leaves `40);` as ONE token, so `;` never separated and the soak-loop
# rule silently matched nothing; then `punctuation_chars=True` re-grouped `)` and `;` into `);`
# and it silently matched nothing again. Both read as correct.
#
# Half the cases are must-NOT-fire, and they are the half that matters. A guard that fires on a
# heredoc, an `echo`, or a `grep` of the docs is a guard that gets switched off within the day.

set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

pass=0
fail=0

check() {
  local want="$1" desc="$2" cmd="$3"
  local got
  got=$(
    printf '%s' "$cmd" | python3 -c "
import json, sys
print(json.dumps({'tool_name': 'Bash', 'cwd': '$PWD',
                  'tool_input': {'command': sys.stdin.read()}}))" \
      | python3 .claude/hooks/pre-bash-guard.py \
      | python3 -c "
import json, sys
raw = sys.stdin.read().strip()
if not raw:
    print('allow')
else:
    h = json.loads(raw)['hookSpecificOutput']
    print(h.get('permissionDecision', 'warn'))"
  )
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
check warn  "JDWP_TEST_THREADS override"    'while true; do JDWP_TEST_THREADS=8 /tmp/arm.bin; done'
check warn  "unbounded workspace cargo"     'cargo build --workspace'
check warn  "unbounded, time-prefixed"      'time cargo test --workspace'
check warn  "soak loop on cargo test"       'for i in $(seq 40); do cargo test --test mcp_integration -- --ignored force_return; done'

echo
echo "Rules that must NOT fire (the half that keeps the guard switched on):"
check allow "plain ls"                      'ls -la'
check allow "BOOTSTRAP on the test binary"  'RUSTC_BOOTSTRAP=1 ./target/debug/deps/mcp_integration-abc --ignored'
check allow "shard-plan --which"            'scripts/shard-plan.py --tests f --which launch_suspends'
check allow "workspace cargo, tailed"       'cargo build --workspace 2>&1 | tail -20'
check allow "workspace cargo, redirected"   'cargo test --workspace > /tmp/o.log 2>&1'
check allow "push named inside an echo"     'echo "remember to git push later"'
check allow "grep for shard in the docs"    'grep -n "shard" CLAUDE.md'
check allow "soak against a copied binary"  'for i in $(seq 40); do /tmp/arm.bin --ignored force_return; done'
check allow "unbalanced quote"              'echo "unterminated'
check allow "heredoc naming BOOTSTRAP"      'cat <<EOF
RUSTC_BOOTSTRAP=1 cargo test
EOF'
check allow "the wrapper script itself"     'scripts/integration-test.sh'

echo
if [ "$fail" -eq 0 ]; then
  echo "$pass passed, 0 failed"
  exit 0
fi
echo "$pass passed, $fail FAILED"
exit 1
