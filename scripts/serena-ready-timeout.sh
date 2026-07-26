#!/usr/bin/env bash
#
# Make Serena's rust-analyzer readiness wait configurable.
#
# Why this exists: Serena waits for rust-analyzer to report `quiescent` before semantic queries can
# work, with the limit hard-coded as a local variable:
#
#     _SERVER_READY_TIMEOUT = 120.0        # solidlsp/language_servers/rust_analyzer.py
#
# On this workspace rust-analyzer needs ~152s (Fetching, Building compile-time-deps, Building
# CrateGraph, Loading proc-macros over the dependency tree). Serena gives up at 120s and proceeds
# anyway, so the first semantic query wastes two minutes AND returns an empty result -- which reads
# like "no references" rather than "not ready yet". Raising the limit means the first query takes
# ~152s and is CORRECT.
#
# This rewrites the constant to read an environment variable, keeping 120 as the default, so the
# value can be set per project. `.mcp.json` sets SERENA_RUST_READY_TIMEOUT for this repo.
#
# It is idempotent, and `--revert` restores the original line. Re-run it after `uv tool upgrade
# serena-agent`, which replaces the file. Nothing here touches this repository's own code.
#
# Usage:
#   scripts/serena-ready-timeout.sh            # apply
#   scripts/serena-ready-timeout.sh --revert   # undo
#   scripts/serena-ready-timeout.sh --check    # report status only, exit 1 if not applied
set -euo pipefail

ORIGINAL='        _SERVER_READY_TIMEOUT = 120.0'
PATCHED='        _SERVER_READY_TIMEOUT = float(os.environ.get("SERENA_RUST_READY_TIMEOUT", "120"))  # patched: scripts/serena-ready-timeout.sh'

find_target() {
  # Locate the installed file across the platform-dependent uv tool layouts.
  local candidates=(
    "$APPDATA/uv/tools/serena-agent/Lib/site-packages/solidlsp/language_servers/rust_analyzer.py"
    "$HOME/AppData/Roaming/uv/tools/serena-agent/Lib/site-packages/solidlsp/language_servers/rust_analyzer.py"
    "$HOME/.local/share/uv/tools/serena-agent/lib/python3.13/site-packages/solidlsp/language_servers/rust_analyzer.py"
  )
  for c in "${candidates[@]}"; do
    [ -f "$c" ] && { echo "$c"; return 0; }
  done
  # Fall back to asking the interpreter that actually imports it.
  local viauv
  viauv=$(serena --version >/dev/null 2>&1 && python -c "
import importlib.util, sys
spec = importlib.util.find_spec('solidlsp.language_servers.rust_analyzer')
print(spec.origin if spec else '')
" 2>/dev/null || true)
  [ -n "${viauv:-}" ] && [ -f "$viauv" ] && { echo "$viauv"; return 0; }
  return 1
}

TARGET=$(find_target) || {
  echo "error: could not find Serena's rust_analyzer.py." >&2
  echo "       Is serena-agent installed? See the Serena section of README.md." >&2
  exit 1
}

mode="${1:-apply}"
case "$mode" in
  --check)
    if grep -qF 'SERENA_RUST_READY_TIMEOUT' "$TARGET"; then
      echo "applied: $TARGET"
      exit 0
    fi
    echo "NOT applied: $TARGET"
    exit 1
    ;;
  --revert)
    if grep -qF 'SERENA_RUST_READY_TIMEOUT' "$TARGET"; then
      # Match the patched line whatever its exact text, and restore the original.
      python - "$TARGET" "$ORIGINAL" <<'PY'
import sys, re
path, original = sys.argv[1], sys.argv[2]
src = open(path, encoding='utf-8').read()
out = re.sub(r'^ *_SERVER_READY_TIMEOUT = .*$', original, src, count=1, flags=re.M)
open(path, 'w', encoding='utf-8').write(out)
PY
      echo "reverted: $TARGET"
    else
      echo "already unpatched: $TARGET"
    fi
    ;;
  apply)
    if grep -qF 'SERENA_RUST_READY_TIMEOUT' "$TARGET"; then
      echo "already applied: $TARGET"
      exit 0
    fi
    if ! grep -qF "$ORIGINAL" "$TARGET"; then
      echo "error: the expected line was not found in $TARGET" >&2
      echo "       Serena may have changed it upstream -- check whether the timeout is configurable now" >&2
      echo "       before re-patching. Current value:" >&2
      grep -n '_SERVER_READY_TIMEOUT = ' "$TARGET" >&2 || true
      exit 1
    fi
    python - "$TARGET" "$ORIGINAL" "$PATCHED" <<'PY'
import sys
path, original, patched = sys.argv[1], sys.argv[2], sys.argv[3]
src = open(path, encoding='utf-8').read()
assert src.count(original) == 1, f"expected exactly one match, found {src.count(original)}"
open(path, 'w', encoding='utf-8').write(src.replace(original, patched, 1))
PY
    echo "applied: $TARGET"
    echo "  SERENA_RUST_READY_TIMEOUT now controls the wait (default 120 if unset)."
    echo "  .mcp.json sets it for this repo; re-run this script after 'uv tool upgrade serena-agent'."
    ;;
  *)
    echo "usage: $0 [apply|--revert|--check]" >&2
    exit 2
    ;;
esac
