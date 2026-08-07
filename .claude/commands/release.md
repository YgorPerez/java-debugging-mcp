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

1. **A non-interactive `release.sh` writes only the commit *subject*.** The release commit's body **is** the
   lead of the release notes — `scripts/release-notes.py` reads it out of the tagged commit and the workflow
   publishes it with `--notes-file` — so a subject-only release commit ships a release whose body is a bare
   changelog and says nothing about tools, arguments or replies. Fixing it means **re-tagging**, because
   amending rewrites the commit the annotated tag names. Step 4.

   Until v0.8.0 this trap was worse than it read: the workflow published with `--generate-notes`, which
   lists merged **pull requests** and never looked at a commit body at all. So the amend-and-re-tag ritual
   was writing prose into git history that the releases page never showed, and every release from v0.2.1 to
   v0.8.0 published one line — the compare link. If you are looking at an old release and wondering where
   its notes went, they are in `git log` on the release commit, and `scripts/release-notes.py v0.7.0` will
   print what that release *should* have said.
2. **Verify CI on the commit you are about to tag, not after.** The tag push starts a workflow that
   *re-runs* the gates; finding out there is the expensive moment.
3. **The release gate can fail on a known flake.** Check it against the open issues before re-running, and
   re-run the failed jobs — never re-cut the tag.
4. **Closing an issue loses your explanation in three different ways.** `gh issue close` takes
   `--comment`, not `--body-file` — with `--body-file` it errors, and in a `||`/`&&` chain the close can
   still happen while the explanation is silently dropped. Worse, **`--comment` on an issue that is
   *already* closed prints `! Issue … is already closed` and posts nothing at all**, which is the normal
   case here because the release commit's trailer closes them before you get to step 9. And the trailer
   itself only half works: **GitHub needs a closing keyword per number**, so `Closes #73, #74, #75, #76`
   closes `#73` and merely *references* the rest. All three bit on v0.7.0. Step 9 has the order that
   survives them: comment first, close second, then verify both.
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

- **Minor** (`0.18.0` from today's `0.17.0`) — a new tool, a renamed tool or argument, or changed behaviour
  behind an existing name. Anything a caller could notice.
- **Patch** (`0.17.1`) — fixes only, no caller-visible surface change.
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

**What you do *not* have to write** is the per-commit list. `scripts/release-notes.py` appends a categorized
changelog under `## What's Changed` — the emoji headings `~/html/b2c-next` uses, derived from the
conventional-commit subjects since the previous tag — plus the compare link. Read the whole body before you
push, because this is exactly what the releases page will show:

```bash
python3 scripts/release-notes.py v<version>
```

Two things to look for. A commit whose subject is not conventional (`feat+docs:`, or no prefix) lands under
**Other Changes** with its subject intact — fine, but if that is a *caller-visible* change it belongs in your
prose above as well. And a `!` in the type or a `BREAKING CHANGE:` footer promotes the entry to
**⚠️ Breaking Changes** at the top; if something breaks callers and nothing appears there, the subject did
not say so.

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

**The `publish` job (crates.io) is the exception to "re-run the failed jobs".** It is last on purpose,
because a GitHub release can be deleted and cut again while a crates.io version can only be yanked and keeps
its number forever (ADR-0043). Two failures there mean different things:

- **`crate version is already uploaded`** — it worked. The version is on crates.io and cannot be replaced.
  Do not re-run, do not try to force it; note it and move on.
- **A failure at the auth step** — the trusted publisher config is gone or no longer matches. It was set up
  at v0.20.0 and verified working, so this is a regression rather than the bootstrap: check both crates'
  settings pages still name owner `YgorPerez`, repo `java-debugging-mcp`, workflow `release.yml` and a
  **blank** environment. The rest of the release is unaffected and already shipped; publish this version by
  hand rather than re-tagging:

  ```bash
  cargo publish --workspace   # --workspace, not two -p runs: see ADR-0043
  ```

## 6. Verify what actually shipped

```bash
gh release view v<version> --json tagName,isDraft,isPrerelease,assets \
  --jq '"draft=\(.isDraft) prerelease=\(.isPrerelease)", (.assets[] | "  \(.name)")'
```

Four platform binaries plus `SHA256SUMS`, not a draft. The asset **names** are the interface
(`docs/toolkit-contract.md`), not a workflow detail.

Then check the **body**, because that is the other half of the interface and it failed silently for six
releases:

```bash
gh release view v<version> --json body --jq '.body' | head -40
```

Your narrative first, then `## What's Changed`, then the compare link. A body that is *only* the compare
link means the notes step published nothing — the old `--generate-notes` failure mode — and the toolkit
audit in step 7 has nothing to read.

Then confirm crates.io agrees, asking the **registry** rather than the workflow log — a green `publish` job
and a version actually resolvable by `cargo install` are not the same claim.

Ask the **sparse index**, which is what `cargo` itself resolves against, rather than `/api/v1/`. The API is
the wrong instrument twice over: it rejects a request with no `User-Agent` — answering `200` with an
`errors` body — and a check that looks only for the success shape renders that refusal as "not published".
That false negative was delivered three times immediately after an irreversible publish (ADR-0043).

```bash
python3 - <<'PY'
import json, urllib.request
for name in ("jdwp-mcp", "java-debugging-jdwp-client"):
    n = name.lower()
    p = {1: f"1/{n}", 2: f"2/{n}", 3: f"3/{n[0]}/{n}"}.get(len(n), f"{n[:2]}/{n[2:4]}/{n}")
    try:
        body = urllib.request.urlopen(f"https://index.crates.io/{p}", timeout=20).read().decode()
        vers = [json.loads(l)["vers"] for l in body.splitlines() if l.strip()]
        print(f"{name:32} {vers[-1] if vers else 'no versions'}")
    except urllib.error.HTTPError as e:
        print(f"{name:32} {'NOT PUBLISHED' if e.code == 404 else f'COULD NOT ASK ({e.code})'}")
    except Exception as e:                       # network, DNS, timeout
        print(f"{name:32} COULD NOT ASK ({e})")
PY
```

Both must print the version you just cut. **`COULD NOT ASK` is deliberately not `NOT PUBLISHED`** — only a
`404` means absent, and folding the two together is the bug this recipe replaced.

`jdwp-mcp` alone matching means the workspace publish stopped between the two crates — the client goes up
first, so this is the *less* likely half to be missing; read the `publish` job's log.

The strongest check, if you want it, is the user's own path — it resolves both crates from the registry,
compiles, and produces a binary:

```bash
cargo install jdwp-mcp --version <version> --locked --root "$(mktemp -d)"
```

## 7. Propagate to infotravel-dev-toolkit

`~/html/infotravel-dev-toolkit` (same dir as `/var/www/html/infotravel-dev-toolkit`). Its
`docs/jdwp-contract.md` requires the pin bump and the skill audit **in one commit** — documenting a tool
the pin lacks advertises something nobody can call; bumping without documenting hides what people gained.

**The pin is a file, not a line in `install.sh`.** It moved to `jdwp-version` because two things read it
now — the installer and the plugin's SessionStart hook — and two copies of a version string was the drift
that repo kept paying for.

⚠️ **Do not `sed` `JDWP_VERSION=` in `install.sh`.** This used to say such a `sed` "silently matches
nothing", which is **false and wrong in the harmful direction** — `install.sh:44` is
`JDWP_VERSION="$(tr -d '[:space:]' < "$HERE/jdwp-version" …)"`, so a substitution *does* match it and
replaces the **read of the pin file with a hardcoded literal**. That restores the exact two-copies drift
this design removed, and it does it silently: the install works, the version is right that once, and the
file stops being the source of truth. Write the file (`echo "$V" > jdwp-version`) and change nothing else.

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

**Anything under `skills/` is a skill, so edit it as one.** Read
`~/.claude/plugins/cache/claude-plugins-official/mattpocock-skills/*/skills/productivity/writing-great-skills/SKILL.md`
before you touch one. It is `disable-model-invocation: true`, so it never fires on its own and is not in the
invocable skill list — you have to open the file.

Release time is where **sediment** comes from: every release tempts one more `since v0.X.Y` paragraph,
appending feels safe while pruning feels risky, and a skill that only ever grows is the default outcome. The
paragraph you add earns its context load like any other — recruit the skill's existing **leading word** rather
than restating its setup (`jdwp-trace` has `Rule 0`), delete the sentence the new one made redundant, and
prompt the **positive** instead of warning against a misreading. `README.md`, `mcp/README.md` and `docs/` are
ordinary docs; this is about `skills/`.

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

**Comment first, close second** — never in one call. Trap 4 is three separate ways to lose the explanation,
and the one that actually happens is this: the release commit's trailer has usually closed the issue
already, and `gh issue close --comment` on a closed issue posts nothing while looking like it worked.

```bash
gh issue comment <n> --body "Shipped in v<version> (<sha>). …"    # always lands, open or closed
gh issue close <n> --reason completed                             # no-op with a warning if already closed
```

Then **verify both happened**, because neither step fails loudly:

```bash
for n in <numbers>; do
  echo -n "#$n: "
  gh issue view "$n" --json state,comments --jq '.state + " (" + (.comments|length|tostring) + " comment)"'
done
```

A `CLOSED (0 comment)` row means the account of the work is gone and only the tracker's silence is left.

On the trailer: give **every** number its own keyword (`Closes #73, closes #74, closes #75`) or accept that
only the first closes and close the rest here. Either is fine; believing the list closed them all is not.

Reference the release and the implementing commit, and **if an issue's brief asked for something different
from what was done, say so on the issue** rather than only in the commit. That is the part worth the tokens:
#74 offered a doc-only fallback that was not taken, #75 was filed with the case *against* building it, and
#76 asked a question ("whose problem does this solve?") whose answer moved during implementation. An issue
closed with "shipped" and none of that loses the reasoning the triage paid for.

## If it goes wrong

Before pushing, the tag and commit are local and cheap:

```bash
git tag -d v<version> && git reset --hard HEAD~1
```

After pushing, do not amend. Cut the next patch version — a published tag can be deleted but not
unshipped, and the toolkit may already have pinned it.
