---
description: Cut, publish and propagate a jdwp-mcp release — gates, tag, GitHub release, downstream toolkit pin, and issue closes.
argument-hint: "[X.Y.Z] (omit to be told what the bump should be)"
---

# Release jdwp-mcp

Version requested: **$ARGUMENTS** (empty means: work out the right bump and say so before doing anything).

`scripts/release.sh` does the bump, the gates, the commit and the tag. It deliberately stops before
pushing. This command is everything around it — the parts that are judgement, the parts that are a
different repo, the part that is this machine, and the traps below, every one of which has actually bitten.

## The traps, up front

1. **A non-interactive `release.sh` writes only the commit *subject*.** The body is the release notes
   (`gh release create --generate-notes` builds them from commits), so a subject-only release commit ships
   a release nobody can read. Fixing it means **re-tagging**, because amending rewrites the commit the
   annotated tag names. Step 4.
2. **Verify CI on the commit you are about to tag, not after.** The tag push starts a workflow that
   *re-runs* the gates; finding out there is the expensive moment.
3. **The release gate can fail on a known flake.** Check it against the open issues before re-running, and
   re-run the failed jobs — never re-cut the tag.
4. **`gh issue close` takes `--comment`, not `--body-file`.** With `--body-file` it errors, and in a
   `||`/`&&` chain the close can still happen while the explanation is silently dropped.
5. **A pushed pin is not an installed pin**, and *nothing in `install.sh` fetches the binary any more.*
   The plugin declares the MCP and a **SessionStart hook** downloads the pin — so after a successful
   `install.sh` the pin file says the new version while the binary on disk is still the old one, until a
   session starts. v0.7.0 hit exactly that: pin `v0.7.0`, binary v0.6.1 with 32 tools. Step 8 runs the
   hook's own script so the release is installed *now*, and checks it by capability. Not optional:
   v0.6.0 and v0.6.1 both shipped without any of this.

## 1. Preconditions

```bash
git rev-parse --abbrev-ref HEAD     # must be main
git status --short                  # must be empty
git fetch origin && git rev-list --count main..origin/main   # must be 0
```

`release.sh` refuses a dirty tree and a non-`main` branch itself, but check first — a refusal after you
have started reasoning about version numbers wastes the reasoning.

## 2. Pick the version

Read the commits since the last tag and classify them the way the **downstream consumer** will:

```bash
git log --oneline "$(git describe --tags --abbrev=0)..HEAD"
```

- **Minor** (`0.6.0`) — a new tool, a renamed tool or argument, or changed behaviour behind an existing
  name. Anything a caller could notice.
- **Patch** (`0.6.1`) — fixes only, no caller-visible surface change.
- Prerelease suffixes (`-rc1`) are marked prerelease by the workflow and stay out of `/releases/latest`,
  which unattended installers follow.

If `$ARGUMENTS` is empty, state the bump and why, then continue.

## 3. Gate, then cut

```bash
scripts/release.sh <version> --dry-run     # runs fmt, unit+cassette tests, doctor
```

The JVM tests are **not** in that gate. Run them, because CI's are a gate on the publish job and finding
out there costs a tag:

```bash
scripts/integration-test.sh                       # and quote the `JDK in use:` line
taskset -c 0-3 cargo test --test mcp_integration -- --ignored   # CI's 4-vCPU shape
```

Then confirm CI is green **on this exact commit** before tagging:

```bash
gh run list --limit 5 --json workflowName,conclusion,headSha \
  --jq '.[] | "\(.workflowName): \(.conclusion) (\(.headSha[0:7]))"'
```

Cut it:

```bash
GIT_EDITOR=true scripts/release.sh <version>
```

## 4. Write the release body, then re-tag

The commit now has a subject and no body. Write the body to a file, then:

```bash
git tag -d v<version>
git commit --quiet --amend -F <body-file>
git tag -a v<version> -m v<version>
[ "$(git rev-parse v<version>^{commit})" = "$(git rev-parse HEAD)" ] && echo "tag matches HEAD"
```

**What the body must contain** (`docs/toolkit-contract.md` — five of six downstream failure modes are
silent, so this is the whole mitigation):

- **New tools**, by exact name, with their arguments.
- **Renamed** anything — *both* names. The toolkit greps for old names.
- **Changed replies**, when downstream prose is likely to quote them.
- **Behaviour changes behind an unchanged name** — the worst case, because the docs still look right.
- **Fixes**, in caller-visible terms.
- A line naming what the toolkit needs to do.

Group under `## New tool` / `## Changed replies` / `## Fixed` / `## Internal`. `v0.6.0` is the shape to
copy.

## 5. Publish

```bash
git push origin main
git push origin v<version>
```

Watch it, and expect to have to think about a red leg:

```bash
gh run watch "$(gh run list --workflow=release.yml --limit=1 --json databaseId --jq '.[0].databaseId')" \
  --exit-status
```

If a gate leg fails, get the actual assertion before deciding:

```bash
gh run view <run-id> --log-failed | grep -E "panicked|FAILED|test result"
```

Cross-check the test name against `gh issue list --state open`. If it is a **filed** flake, re-run the
failed jobs and say in your report that you did and which issue it was — do not let a re-run on red pass
silently. If it is *not* filed, stop: that is a new failure and the release should wait.

```bash
gh run rerun <run-id> --failed
```

## 6. Verify what actually shipped

```bash
gh release view v<version> --json tagName,isDraft,isPrerelease,assets \
  --jq '"draft=\(.isDraft) prerelease=\(.isPrerelease)", (.assets[] | "  \(.name)")'
```

Four platform binaries plus `SHA256SUMS`, not a draft. The asset **names** are the interface
(`docs/toolkit-contract.md`), not a workflow detail.

## 7. Propagate to infotravel-dev-toolkit

`~/html/infotravel-dev-toolkit` (same dir as `/var/www/html/infotravel-dev-toolkit`). Its
`docs/jdwp-contract.md` requires the pin bump and the skill audit **in one commit** — documenting a tool
the pin lacks advertises something nobody can call; bumping without documenting hides what people gained.

**The pin is a file, not a line in `install.sh`.** It moved to `jdwp-version` because two things read it
now — the installer and the plugin's SessionStart hook — and two copies of a version string was the drift
that repo kept paying for. A `sed` against `JDWP_VERSION=` in `install.sh` silently matches nothing today;
that line reads the file.

```bash
cd ~/html/infotravel-dev-toolkit
V=v<version>
echo "$V" > jdwp-version && bash -n install.sh

# Integrity: the release's own SHA256SUMS is the whole story (their ADR-0001). Never vendor or patch it.
B=/tmp/jdwp-$V
curl -fsSL -o "$B" "https://github.com/YgorPerez/java-debugging-mcp/releases/download/$V/jdwp-mcp-$V-linux-x86_64"
curl -fsSL -o /tmp/SS "https://github.com/YgorPerez/java-debugging-mcp/releases/download/$V/SHA256SUMS"
grep "$(sha256sum "$B" | cut -d' ' -f1)" /tmp/SS || echo "CHECKSUM MISMATCH — stop"
chmod +x "$B"

# The binary is self-describing, so most of the audit is checkable rather than reviewable.
printf '{"jsonrpc":"2.0","id":1,"method":"tools/list"}\n' | "$B" > /tmp/tools.json
```

Audit every `debug.*` name and `{argument:}` in `skills/`, `README.md`, `mcp/README.md` against
`/tmp/tools.json`. Two documented traps: **match bare unprefixed names too** (`set_breakpoint` slipped
through a `debug.*`-only check), and **a count is not a check** (rename one, add one, count unchanged).
Also list tools the docs never mention — a new tool nobody names is invisible.

Then update the prose the audit cannot verify — behavioural claims, cost figures, "the reply says X" —
from the release body, and commit the pin and the docs together. Say in the commit which tools you left
undocumented and why, if any. **Push it** — the next step re-pulls the repo, so an unpushed commit is a
commit the installer will not see.

## 8. Install it, or the release changed nothing on this machine

Bumping the pin is a promise, and **`install.sh` is no longer what keeps it.** Since the plugin migration it
neither downloads the binary nor registers the MCP: the plugin declares the server
(`mcp/jdwp.mcp.json`, `command: ${CLAUDE_PLUGIN_DATA}/bin/jdwp-mcp`) and a **SessionStart hook** puts the
pinned binary at that path — `hooks/hooks.json` diffs `${CLAUDE_PLUGIN_ROOT}/jdwp-version` against
`${CLAUDE_PLUGIN_DATA}/jdwp-version` and runs `scripts/ensure-jdwp.sh` only when they differ.

So the installer updates the plugin checkout, and that is all it does for jdwp:

```bash
cd ~/html/infotravel-dev-toolkit && ./install.sh    # note the plugin commit it reports
```

**The binary is still the old one at this point.** The hook has not fired — that needs a session start — so
the pin file reads the new version while `${CLAUDE_PLUGIN_DATA}/bin/jdwp-mcp` is whatever the last pin
installed. This is the old stale-binary trap wearing new clothes, and it is why "I ran install.sh" is not
evidence of anything. Run what the hook runs, and the release is installed now:

```bash
R=~/.claude/plugins/cache/infotravel-dev-toolkit/infotravel-dev/<plugin-commit>
D=~/.claude/plugins/data/infotravel-dev-infotravel-dev-toolkit
cat "$R/jdwp-version"        # must be the version you just cut; if not, the plugin didn't update
CLAUDE_PLUGIN_ROOT="$R" CLAUDE_PLUGIN_DATA="$D" "$R/scripts/ensure-jdwp.sh" \
  && cp "$R/jdwp-version" "$D/jdwp-version"
```

That is byte-for-byte what the hook would do — `ensure-jdwp.sh` matches the asset name against the
release's own `SHA256SUMS`, downloads to a temp dir, and moves it into place only after the checksum
matches — so doing it by hand costs nothing and removes a whole restart's worth of uncertainty. The `cp` is
the hook's second half: skip it and the next session re-downloads.

Prove what is installed by **capability**, never by the pin file or the installer's stamp:

```bash
printf '{"jsonrpc":"2.0","id":1,"method":"tools/list"}\n' | "$D/bin/jdwp-mcp" 2>/dev/null \
  | python3 -c "import json,sys; t=[x['name'] for x in json.load(sys.stdin)['result']['tools']]; \
print(len(t), 'tools'); print('<a tool new in this release>' in t)"
```

**`~/.claude.json` is not part of this any more.** There is no user-scoped `jdwp` entry to inspect or
re-point: `install.sh` §4 removes the one older versions of it created, and the plugin's server is
namespaced `plugin:infotravel-dev:jdwp`, so its tools arrive as `mcp__plugin_infotravel-dev_jdwp__*`. Do
not `claude mcp add` anything, and do not edit that file.

One case still needs a human, and the installer will tell you: a **source build**. With
`IT_JDWP_FROM_SOURCE=1` in `~/.config/infotravel-dev.env`, or a registered command ending
`/target/release/jdwp-mcp`, `install.sh` deliberately leaves it alone and both servers connect — two
debuggers' worth of tools in context. That is the source-build user's call to make, so report it and leave
it; they develop the debugger.

Finally, **tell the user to restart Claude Code**, and when they have, spend four calls proving the
*registered* MCP works rather than only the file on disk — the release notes name what is new, so exercise
it. For v0.7.0 that was `debug.launch` on a throwaway class, a breakpoint that came back **deferred**
(nothing has loaded yet at `suspend=y`), `debug.continue`, and a hit at `{"method":"<clinit>"}` — which no
amount of `tools/list` grepping would have established.

## 9. Close what shipped

```bash
gh issue close <n> --reason completed --comment "Shipped in v<version> (<sha>). …"
```

`--comment`, not `--body-file`. Reference the release and the implementing commit, and if an issue's brief
asked for something different from what was done, say so on the issue rather than only in the commit.

## If it goes wrong

Before pushing, the tag and commit are local and cheap:

```bash
git tag -d v<version> && git reset --hard HEAD~1
```

After pushing, do not amend. Cut the next patch version — a published tag can be deleted but not
unshipped, and the toolkit may already have pinned it.
