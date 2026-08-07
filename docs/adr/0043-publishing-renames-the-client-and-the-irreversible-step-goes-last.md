# 0043 — Publishing renames the client, and the irreversible step goes last

## Context

Distribution was the four platform binaries `release.yml` attaches to each GitHub release (REL-1,
[#34](https://github.com/YgorPerez/java-debugging-mcp/issues/34)). That is the right default and it stays:
it needs no Rust toolchain, no compile, and no network beyond one download.

What it does not give is `cargo install jdwp-mcp`, which is how a Rust user expects to install a Rust
binary — and it is not a small gap, because the audience for a JDWP debugger overlaps heavily with people
who already have `cargo` on the path. Neither crate has ever been published.

Two facts about crates.io shape everything below, and both were measured rather than assumed.

**`jdwp-client` is not available.** It was registered on 2025-09-10 by
[bonk-dev/jdwp-client](https://github.com/bonk-dev/jdwp-client), an unrelated project, and had 368 downloads
when this was written. This is not new to the repo: it is the whole reason `scripts/semver-check.sh` exists,
since `cargo semver-checks` with no baseline compares our library against that stranger's package and returns
a confident "no semver update required".

**`cargo publish` rejects a bare path dependency.** `jdwp-mcp` depends on `jdwp-client` by path, so the binary
cannot be published while the library has no version a stranger could resolve — and it cannot have one under a
name it does not own. The two crates go up together or neither does.

## Decision

**Publish both crates, rename the library to `java-debugging-jdwp-client`, and make crates.io the last thing
a release touches.**

### The rename is in the manifest and nowhere else

`[package] name` becomes `java-debugging-jdwp-client`; `[lib] name` is pinned to `jdwp_client`. Registry names
must be globally unique, import paths only have to be unique within one dependency graph, and conflating the
two would have meant rewriting every `use jdwp_client::…` in both crates for a constraint that has nothing to
do with Rust. **Measured: `cargo check --workspace --all-targets` passes with zero source changes.**

The dependency moves into `[workspace.dependencies]` as
`jdwp-client = { package = "java-debugging-jdwp-client", path = "jdwp-client", version = "0.19.0" }`, so
`mcp-server` goes on saying `jdwp-client.workspace = true` like every other dependency here. Cargo resolves
`path` locally and rewrites the published manifest to `version` alone; the two never both apply.

**Rejected: folding the library into `mcp-server`** to publish one crate under the free name `jdwp-mcp`. It
buys a shorter name and costs the only lib target in the workspace — which is the entire subject of
`scripts/semver-check.sh`, since `cargo-semver-checks` reads libraries and not binaries. Trading a working
API-stability check for a nicer string is not a trade.

### That version number moves with the workspace, and `release.sh` owns it

A `^0.x` requirement matches only its own minor. Leave `0.19.0` in that entry while the workspace moves to
`0.20.0` and `jdwp-mcp` requires a client version nothing in the tree provides. So `scripts/release.sh` bumps
it in the same step as `[workspace.package].version`, with deliberately scoped regexes — table-anchored for
the package version, line-anchored for the dependency — so neither can reach `serde` or `tokio` below.

**This was first written up here as a silent hazard, and that was wrong.** The claim was that nothing local
would notice, because every local build resolves through `path` and ignores the number, so a stale entry
would survive to fail at the registry after the tag was public. Planting `version = "0.18.0"` and running
`cargo check` disproves it in one line:

```
error: failed to select a version for the requirement `java-debugging-jdwp-client = "^0.18.0"`
candidate versions found which didn't match: 0.19.0
```

Cargo validates a path dependency's `version` against the path package at **resolution**. A mismatch is
therefore caught by the next `cargo check` anyone runs, by every gate, and — inside `release.sh` itself — by
the `cargo update` step three lines after the bump, which runs before anything is committed or tagged. There
is no window in which it reaches crates.io.

Recorded because a **test was written against the false version of this claim** and then deleted. It would
have pinned a value the compiler already validates on every build, which fails DOC-15's first filter — a
stale value has to cost something — and would have read, to anyone who found it later, as evidence that this
needs guarding. `release.sh` still reads both numbers back through `tomllib` before tagging, for the one
thing cargo's error does not give: which of the two is wrong.

### Trusted Publishing, and no stored credential

Authentication is OIDC via `rust-lang/crates-io-auth-action`: GitHub mints a short-lived token about the
workflow, crates.io verifies it came from this repository, and returns an access token that expires. The job
grants `id-token: write` — which is not a write to the repository, but permission to ask GitHub about itself.

**Rejected: `CARGO_REGISTRY_TOKEN` as a repository secret.** It is simpler and it would let the workflow
perform the first publish too. It also adds this repo's first long-lived credential, and the repo currently
having *no secrets at all* is a property CLAUDE.md relies on when explaining why the scaffolded AI-review
workflows failed five times in silence. A short-lived token that cannot leak into a fork PR is worth one
manual bootstrap.

**Rejected: OIDC with a token fallback.** A silent fallback path means a run that quietly authenticated the
weaker way is indistinguishable from one that did what it said — the exact failure shape this repo keeps
writing post-mortems about, and the reason `.github/actions/setup-rust` prints `Rust in use:`.

### The bootstrap is manual and cannot be automated away

crates.io has no equivalent of PyPI's "pending publishers". A trusted publisher is configured *on* a crate, so
the crate must exist first. **The first version of each crate is published by hand**, then the publisher is
configured in the crates.io UI, and every release after that is this workflow. Until that happens the job
fails at the auth step on every tag. Recorded here because it is the one step in this decision that no amount
of workflow YAML can take on.

Note the first publish must be `cargo publish --workspace`, not two `-p` invocations. **Measured:**
`cargo package -p jdwp-mcp` fails with `no matching package named java-debugging-jdwp-client found` until the
client is actually on the index; `--workspace` succeeds because cargo stands up a temporary registry and
resolves the sibling through it.

### `publish` runs after `release`, and that ordering is the point

Job order is `version → {gate-tests, gate-lint, build, package} → release → publish`.

A GitHub release can be deleted and cut again. **A crates.io version can only be yanked, and a yanked version
still occupies its number forever.** So the recoverable publish goes first and the irreversible one last. The
bad case becomes "crates.io is one release behind, re-run the job"; the alternative ordering makes it "0.20.0
exists on crates.io and nowhere else", which no re-run repairs.

`package` is the same argument moved earlier: a full `cargo publish --workspace --dry-run` running in parallel
with the binary matrix, which `release` waits on. Packaging has failure modes nothing else in this pipeline
exercises — a source file cargo declines to copy into the tarball, an unpublishable dependency, a manifest
field left empty — and each is cheap before the tag is public and expensive after. It does **not** catch a
version already uploaded, because `--dry-run` skips that check; that is the publish job's to find, and it is
why `release.sh` refuses to reuse a tag.

## Consequences

`cargo install jdwp-mcp` compiles from source and needs a toolchain, so this **adds** a distribution channel
rather than replacing one. The release binaries remain the better path for anyone who does not want to wait
for a compile, and the README says so at the point of choice.

Twelve `warning: ignoring example …` lines are now expected output on every packaging run: `jdwp-client`
declares example targets under `../examples/`, outside its own package root, and `cargo package` copies
nothing from outside that root. They are manual probes you point at a live JVM and are useless to a consumer,
so being dropped is correct. Both manifests say so, because twelve warnings scrolling past during a release
is exactly when nobody has time to work out whether they matter.

`scripts/semver-check.sh` keeps its `--baseline-rev` git-tag baseline. Once the client is published its
crates.io default would finally point at *our* package, but a tag baseline still answers for commits between
releases, which a registry baseline cannot. The script's header is updated: the collision it documents is now
the reason for the crate's name rather than the reason for its own existence.
