#!/usr/bin/env bash
# Cut a release: bump the manifest, run the local gates, commit, tag. Does NOT push.
#
# `.github/workflows/release.yml` triggers on `v*.*.*` and its first job refuses a tag that disagrees with
# `[workspace.package].version`. That check is the right shape — it is toolchain-free and cannot reach a
# publishing job — but it fires *after* the tag exists, which is the expensive moment to find out. This
# script exists so the two can never disagree in the first place: it derives the tag from the manifest it
# just wrote, rather than accepting both and hoping.
#
# **It stops before pushing, on purpose.** Pushing the tag is what publishes binaries to a public releases
# page, and it is the one step here nobody can take back — a released tag can be deleted but not unshipped.
# So the script ends by printing the exact command, leaving the irreversible action to a human who has read
# what the gates said.
#
# Usage:
#   scripts/release.sh 0.5.0          # bump, gate, commit, tag
#   scripts/release.sh 0.5.0 --dry-run  # say what would happen, change nothing
#
# The version is given without the leading `v`: the manifest holds `0.5.0` and the tag is `v0.5.0`, and
# taking the manifest form means the one thing you type is the one thing that has to be right.

set -euo pipefail

cd "$(dirname "$0")/.."

DRY_RUN=0
VERSION=""
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    -h | --help)
      sed -n '2,20p' "$0" | sed 's/^# \?//'
      exit 0
      ;;
    -*)
      echo "unknown flag: $arg" >&2
      exit 2
      ;;
    *) VERSION="$arg" ;;
  esac
done

die() {
  echo "ERROR: $*" >&2
  exit 1
}
step() { printf '\n==> %s\n' "$*"; }

# --- what was asked for -------------------------------------------------------------------------------

[ -n "$VERSION" ] || die "no version given. Usage: scripts/release.sh X.Y.Z [--dry-run]"

# Semver, optionally with a prerelease suffix. The workflow marks anything containing a hyphen as a
# prerelease so it stays out of /releases/latest, which unattended installers follow — so the shape of
# what you type here decides that, and a typo like `0.5.0.1` should not reach a tag.
if ! printf '%s' "$VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
  die "'$VERSION' is not X.Y.Z or X.Y.Z-suffix. Give the manifest form, without a leading 'v'."
fi

TAG="v$VERSION"
CURRENT="$(python3 -c 'import tomllib;print(tomllib.load(open("Cargo.toml","rb"))["workspace"]["package"]["version"])')"

step "Releasing $CURRENT -> $VERSION (tag $TAG)"
[ "$DRY_RUN" = 1 ] && echo "    --dry-run: nothing will be written, committed or tagged"

# --- preconditions ------------------------------------------------------------------------------------
#
# Checked before the manifest is touched, so a refusal leaves the tree exactly as it was found.

step "Checking the working tree and branch"

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[ "$BRANCH" = "main" ] || die "on branch '$BRANCH'. A release is cut from main; the release workflow's
       gates run tests.yml and rust-doctor.yml against this commit, and a tag on an unmerged branch would
       publish code that never faced a review."

[ -z "$(git status --porcelain)" ] || die "the working tree is dirty. A release commit should contain the
       version bump and nothing else — commit or stash your changes first:
$(git status --short | sed 's/^/         /')"

if [ "$(git rev-parse HEAD)" != "$(git rev-parse "@{u}" 2>/dev/null || echo none)" ]; then
  git fetch --quiet origin || die "could not reach origin to compare"
  behind="$(git rev-list --count "HEAD..@{u}" 2>/dev/null || echo 0)"
  ahead="$(git rev-list --count "@{u}..HEAD" 2>/dev/null || echo 0)"
  [ "$behind" = "0" ] || die "main is $behind commit(s) behind origin. Pull first, or the release will
       omit work that is already on main."
  [ "$ahead" = "0" ] || echo "    note: $ahead local commit(s) not yet pushed — they will be part of this
          release, and must be pushed with the tag."
fi

git rev-parse -q --verify "refs/tags/$TAG" >/dev/null &&
  die "tag $TAG already exists locally. Releases are not re-cut: pick the next version."
if git ls-remote --exit-code --tags origin "refs/tags/$TAG" >/dev/null 2>&1; then
  die "tag $TAG already exists on origin — it has been published. Pick the next version."
fi

# The manifest must actually move. A no-op bump produces a release commit that changes nothing and a tag
# pointing at a version already shipped under a different tag.
[ "$CURRENT" != "$VERSION" ] || die "the manifest is already $VERSION. Nothing to bump."

echo "    branch main, clean tree, $TAG is free"

# --- the gates ----------------------------------------------------------------------------------------
#
# Run BEFORE the bump. The point is to learn whether this commit is releasable; if it is not, there should
# be no version-bump commit to unpick afterwards. The release workflow re-runs tests and lint against the
# tagged commit regardless (they are `needs:` of the publishing job), so this is an early answer rather
# than the authority.

step "Gate: cargo fmt"
cargo fmt --check || die "rustfmt would rewrite the tree. Run 'cargo fmt' and commit that separately —
       CI fails on a misformatted diff (LINT-4, #44)."
echo "    formatted"

step "Gate: cargo test --workspace (unit + cassette)"
cargo test --workspace --quiet || die "tests failed. Nothing has been bumped."
echo "    unit and cassette tests pass"

step "Gate: scripts/doctor.sh --findings"
# Doctor is the gate, not clippy, and it fails on *warnings* (ADR-0007). This reads the `by rule:` line
# rather than the score, which is not the verdict — the repo has sat at 100/100 on top of 21 warnings.
#
# **The `unsafe-dependency` baseline this used to subtract is gone**, so on a current tree the clean branch
# below is the one that runs. `rust-doctor.toml` ignores the rule: every finding was about a third-party
# crate, none of them ever ran in CI, and naming a count here and in CLAUDE.md made the local verdict
# something you decoded against a number that drifts with `Cargo.lock`.
#
# The subtraction is kept anyway, and deliberately. It costs one `sed` and it is what stops this script from
# refusing every release on a machine where someone has installed `cargo-geiger` *and* the ignore has been
# removed or has stopped matching — which is a configuration this repo has already lived in for months. A
# gate that fails closed on a dependency's internals is not a gate anyone keeps.
doctor_out="$(./scripts/doctor.sh --findings 2>&1)" && doctor_rc=0 || doctor_rc=$?
by_rule="$(printf '%s' "$doctor_out" | grep -oE '^\s*by rule:.*' | head -1 || true)"

if [ -z "$by_rule" ]; then
  # No `by rule:` line at all: either a clean scan, or doctor failed in a way that produced no summary.
  # The two are told apart by the exit code, because a scan that could not run must not read as a pass.
  if [ "$doctor_rc" != 0 ]; then
    printf '%s\n' "$doctor_out" | tail -30
    die "doctor exited $doctor_rc without a findings summary — it did not complete, so nothing is known
       about this commit's lint state."
  fi
  echo "    doctor: clean"
else
  # Everything except `unsafe-dependency`, which is removed by name; whatever is left is a rule nobody has
  # signed off on. Not "the baseline" — there is no baseline any more (see above), and the messages below
  # must not send anyone looking for a documented count that no longer exists.
  beyond="$(printf '%s' "$by_rule" |
    sed -E 's/^\s*by rule:\s*//; s/unsafe-dependency[^,]*,?\s*//g; s/,\s*$//')"
  if [ -n "$beyond" ]; then
    printf '%s\n' "$doctor_out" | grep -E '^- \*\*(warning|error)\*\*' | grep -v 'unsafe-dependency' || true
    die "doctor found $beyond — a clean tree prints 'would pass', so every one of those is yours and it
       will fail CI. Fix it, or say why it is acceptable, before cutting a release."
  fi
  # Reached only when `unsafe-dependency` was the *only* thing found, which on a correctly configured tree
  # does not happen at all: `rust-doctor.toml` ignores the rule. So this says what it actually means — the
  # gate is not blocked, and the findings are a local configuration quirk rather than anything this release
  # introduced. The old wording called them "the documented baseline", which was a pointer to a count this
  # repo deleted.
  echo "    doctor: ${by_rule#*by rule: } — all of it third-party \`unsafe-dependency\`, which
    \`rust-doctor.toml\` ignores and CI never runs, so none of it is yours. Seeing it here means
    cargo-geiger is installed on this machine and the ignore is not matching."
fi

# The JVM tests are NOT run here and the script says so rather than implying a full verification. They
# need a JDK, they take a minute, and CI runs them across 11/17/21 as a `needs:` of the publish job, which
# is a stronger check than one local JDK (TEST-11, #36).
cat <<'NOTE'

    NOT run here: scripts/integration-test.sh (the #[ignore]d JVM tests).
    They need a JDK and CI runs them on 11/17/21 as a gate on the publish job, which is stronger than
    one local JDK would be. Run them yourself if you want the answer before the tag:
        ./scripts/integration-test.sh
NOTE

# --- the bump -----------------------------------------------------------------------------------------

step "Bumping [workspace.package].version"

if [ "$DRY_RUN" = 1 ]; then
  echo "    would set version = \"$VERSION\" in Cargo.toml and server.json"
  echo "    would refresh Cargo.lock, commit 'chore(release): $VERSION', and tag $TAG"
  step "Dry run complete — nothing changed"
  exit 0
fi

# Anchored to the [workspace.package] table rather than a bare version match: [workspace.dependencies]
# below it is full of version strings, and a loose substitution would rewrite a dependency instead.
python3 - "$VERSION" <<'PY'
import re, sys

new = sys.argv[1]
src = open("Cargo.toml").read()
pattern = re.compile(r'(\[workspace\.package\]\s*\n(?:[^\[]*?\n)?)(version\s*=\s*")([^"]+)(")', re.M)
out, n = pattern.subn(lambda m: f"{m.group(1)}{m.group(2)}{new}{m.group(4)}", src, count=1)
if n != 1:
    sys.exit("could not find version under [workspace.package] — has Cargo.toml been restructured?")
open("Cargo.toml", "w").write(out)
print(f"    Cargo.toml: version = \"{new}\"")
PY

# server.json is the MCP registry manifest (REL-3, #137). It carries its own `version`, and a manifest that
# silently lags the release it describes is worse than no manifest — a searcher is told a version exists
# that nobody published. Bumped HERE rather than by hand for the same reason Cargo.lock is, and asserted
# against Cargo.toml by `the_registry_manifest_version_matches_the_crate` in docs_claims.rs, so the two
# cannot drift even if this step is later removed.
python3 - "$VERSION" <<'PY'
import json, sys

new = sys.argv[1]
path = "server.json"
try:
    with open(path) as fh:
        doc = json.load(fh)
except FileNotFoundError:
    sys.exit(0)  # having no manifest is a decision someone can make; a stale one is not
except json.JSONDecodeError as why:
    sys.exit(f"{path} is not valid JSON ({why}) — refusing to guess at a registry manifest")
doc["version"] = new
with open(path, "w") as fh:
    json.dump(doc, fh, indent=2, ensure_ascii=False)
    fh.write("\n")
print(f'    server.json: version = "{new}"')
PY

# Cargo.lock records both workspace members' versions, is committed, and the release build uses --locked.
# A stale lock would fail that build at the very end of an otherwise green release, so refresh it now and
# include it in the same commit. `--offline` because this must not resolve anything newer than what the
# gates just tested; the only change wanted is our own version numbers.
cargo update --workspace --offline --quiet ||
  cargo check --workspace --quiet >/dev/null ||
  die "could not refresh Cargo.lock after the bump"
git diff --quiet -- Cargo.lock &&
  echo "    Cargo.lock: unchanged (already agreed)" ||
  echo "    Cargo.lock: refreshed"

# Belt and braces: reread the manifest and derive the tag from what is now on disk, so the thing being
# tagged is the thing that was written. This is the check release.yml makes, made locally and earlier.
WROTE="$(python3 -c 'import tomllib;print(tomllib.load(open("Cargo.toml","rb"))["workspace"]["package"]["version"])')"
[ "$WROTE" = "$VERSION" ] || die "wrote '$WROTE' but expected '$VERSION' — refusing to tag a mismatch.
       Cargo.toml has been modified; 'git checkout -- Cargo.toml Cargo.lock' restores it."

step "Committing and tagging"

git add Cargo.toml Cargo.lock server.json

# The subject only. Every release in this repo carries a written rationale in its commit body (0.4.0 is
# the precedent), but it is prose about what shipped and this script has no way to know that — so it
# writes the one line it can be sure of and hands the body over below.
#
# That body is not just for `git log`: `scripts/release-notes.py` reads it out of the tagged commit and
# `release.yml` publishes it as the lead of the release notes, above the changelog it generates from the
# commits. A subject-only release commit therefore ships a release that documents nothing a caller could
# have noticed — which is the one thing `docs/toolkit-contract.md` asks a release to do.
#
# `-e` opens an editor when there is one, which is the common interactive case. Under a non-interactive
# shell (`GIT_EDITOR=true`, CI, an agent) that is a no-op and the subject stands, so the script never
# hangs waiting for input that will not come.
if [ -t 0 ] && [ -t 1 ]; then
  git commit --quiet -e -m "chore(release): $VERSION" ||
    die "the release commit was abandoned. Cargo.toml and Cargo.lock are still bumped and staged;
       'git checkout -- Cargo.toml Cargo.lock' restores them."
else
  git commit --quiet -m "chore(release): $VERSION"
  echo "    no tty: committed the subject alone. See the note about amending below."
fi

# Tagged AFTER the commit is final, which is the whole reason the editor opens first. An annotated tag
# names one commit; amending afterwards rewrites that commit and leaves the tag pointing at an object no
# longer on the branch — a release that builds from a commit nobody can find. The instructions at the end
# therefore never suggest amending without re-tagging.
git tag -a "$TAG" -m "$TAG"
echo "    $(git log --oneline -1)"
echo "    tagged $TAG -> $(git rev-parse --short "$TAG^{commit}")"

# --- the irreversible step, left to a human ------------------------------------------------------------

cat <<EOF

==> Not pushed. Pushing the tag publishes binaries to the public releases page, which is the one step here
    that cannot be taken back. When you are satisfied with the gates above:

        git push origin main
        git push origin $TAG

    The tag push triggers .github/workflows/release.yml, which re-runs tests (JDK 11/17/21) and
    rust-doctor as gates on the publish job, builds four platform binaries, and attaches them with a
    SHA256SUMS. Watch it with:

        gh run watch \$(gh run list --workflow=release.yml --limit=1 --json databaseId --jq '.[0].databaseId')

    To undo before pushing:

        git tag -d $TAG && git reset --hard HEAD~1

    Read the body the release will actually publish first — this commit's message body leads it, and
    scripts/release-notes.py appends the categorized changelog and the compare link:

        python3 scripts/release-notes.py $TAG

    To reword the release notes before pushing, RE-TAG afterwards — an annotated tag names one commit,
    and amending rewrites it, leaving $TAG pointing at an object no longer on the branch:

        git tag -d $TAG && git commit --amend && git tag -a $TAG -m $TAG

    Consumers to update after the release publishes:
      - infotravel-dev-toolkit pins this tag in its jdwp-version file, verified against this release's SHA256SUMS
EOF
