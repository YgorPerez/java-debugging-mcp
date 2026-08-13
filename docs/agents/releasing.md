# Releasing

**Use `/release [X.Y.Z]`.** That instruction and the traps that come with it are in `CLAUDE.md`, because
they cost time when they are read late. This file is what a tag actually publishes and why — a catalogue
you consult during a release rather than carry into every session.

## What a tag publishes

| asset | since | notes |
|---|---|---|
| Five platform binaries | REL-9 (#173) added the fifth | `linux-x86_64`, `linux-aarch64`, `macos-aarch64`, `macos-x86_64`, `windows-x86_64.exe` |
| `SHA256SUMS` | REL-1 (#34) | The binaries' manifest, and what the downstream installer matches host names against |
| `tool-surface-<tag>.json` | REL-8 (#165) | Deliberately **not** in `SHA256SUMS` |
| Build provenance | REL-7 (#164) | Over the binaries *and* the surface asset |
| Both crates, to crates.io | REL-5 (ADR-0043) | Runs last, because it is the only irreversible step |
| Six npm packages | REL-6 (#168) | The `jdwp-mcp` wrapper and one binary package per platform. Publishes **after** crates.io, and the wrapper goes **last of the six** — npm has no transaction, so making the package `npx` actually names the final one is what stops a half-published set being installable |

**Windows `npx` is broken on every published version, and npm took the name it needed** (REL-10, #194). The
`403 … Package name triggered spam detection` that refused `jdwp-mcp-win32-x64` on the v0.21.0 bootstrap run
was read here as a heuristic to wait out. It was not: on **2026-08-12** that name became an npm
security-holder package owned by `npm-support`, so `npm publish` to it is now refused on **ownership**, and
there is no clearing to wait for. Measured:

```
$ npm view jdwp-mcp-win32-x64 --json
  "dist-tags":   { "latest": "0.0.1-security" },
  "description": "security holding package",
  "repository":  "npm/security-holder",
  "maintainers": [ "npm-support <support@npmjs.com>" ]
```

The platform package is therefore **`jdwp-mcp-windows-x64`**. The binary is unchanged and its manifest's
`"os"` field still says `win32` — only the package name diverges from `process.platform`, which the shim
carries as its one deliberate exception (`PACKAGE_OS` in `npm/jdwp-mcp/bin/jdwp-mcp.cjs`). **Do not "fix"
that back to the derived name**; it is the only name here npm will not accept.

**The already-published wrappers cannot be repaired.** v0.21.0 and v0.22.0 both pin
`jdwp-mcp-win32-x64@<their version>` exactly, and the exact pin is what normally makes a missing platform
fixable with no release — it is how v0.22.0's linux-x86_64 gap was closed, on attempt 3 of its own run. That
only works for a name we can still publish to. Windows needs the **next release** and a bootstrap publish of
the new name. Until then a Windows `npx` prints the shim's two-causes message and points at
`cargo install`, which works there and always has.

**npm needs a one-time bootstrap, and `scripts/bootstrap-npm.sh` is it.** Trusted publishing attaches to a package that already exists, so the first version of all six is published by hand — the same bootstrap ADR-0043 records for crates.io. The wizard gates on the release carrying all five binaries first (v0.20.0 carried four; `linux-aarch64` arrived with REL-9), verifies them against `SHA256SUMS`, publishes platforms-then-wrapper, and walks the trusted-publisher form for each package. **Until it has run once, the `publish-npm` job fails on every tag** — deliberately, since a missing bootstrap must not look like a success.
| The release body | v0.9.0 | Built by `scripts/release-notes.py`, not `--generate-notes` |

## crates.io runs last, on purpose

**A tag publishes both crates** (REL-5, ADR-0043) — the one step here nothing can undo, since a version can
be yanked but keeps its number forever. It runs last for that reason, over OIDC with no stored token, and
needs nothing from you: the manual bootstrap happened at v0.20.0. ADR-0043 has the sequence if it ever has
to be done again, and `/release` step 5 has what to do when that job goes red.

**The library's package name is `java-debugging-jdwp-client`, not `jdwp-client`** — the obvious name belongs
to an unrelated project on crates.io, the collision `scripts/semver-check.sh` was built around. `CLAUDE.md`
carries the consequence, because it bites outside a release: anything taking a `-p` package name wants the
long one.

## ARM Linux, and the smoke test the matrix now runs

**Five platform binaries now, not four** (REL-9, #173). The new asset is
**`jdwp-mcp-<tag>-linux-aarch64`**, built natively on `ubuntu-24.04-arm` and statically linked against musl
like its x86_64 sibling. That name is the *interface* (`docs/toolkit-contract.md`), and it is what
downstream's `jdwp-platforms` — which currently documents ARM Linux as "absent on purpose" — will match
against `SHA256SUMS`. It is caller-visible and belongs in the release notes as a new platform.

**The build matrix also runs its own output** — initialize / `tools/list` over stdio, before the upload —
on every slice except `x86_64-apple-darwin`, which is the one binary its own builder cannot execute (both
macOS slices cross-compile from the arm64 runner, which has no Rosetta). The gates test the *code*, on one
host; until this, nothing had ever started the artifact a release publishes.

## Provenance, and why `SHA256SUMS` is not enough

**`SHA256SUMS` proves the download, not the build** (REL-7, #164), because the manifest ships beside the
binaries it lists — anything able to replace one can replace the other. `gh attestation verify <asset>
--repo YgorPerez/java-debugging-mcp` answers the half a checksum cannot, over OIDC with **no stored token**
and **no file added to `dist/`**, so REL-2's guard there is untouched. The consumer half — teaching
`ensure-jdwp.sh` to run it — is the toolkit's.

## The tool surface asset

**A tag publishes `tool-surface-<tag>.json`, so "what changed for callers" is two curls** (REL-8, #165).
Every `debug.*` tool, its description and every argument's schema in one document. It is **built from the
committed snapshots, never regenerated from the binary** — an asset re-derived at release time would be a
second source of truth beside the files `cargo test` gates — and `scripts/tool-surface.py` **refuses to
publish** rather than emit a half-parse: the two snapshots must name the same tools, the
`# N tools, M arguments` line in the argument file must match what was parsed, and `docs/tools.md`'s table
must agree.

Three decisions are written at the step: it lives **outside `dist/`** so REL-2's guard stays as strict as it
was, it is **deliberately absent from `SHA256SUMS`** because that file is the *binaries'* manifest the
downstream installer matches host names against, and it is therefore **an attestation subject** — the one
asset for which that is the only integrity story. The format is versioned by `surface_version`, independent
of the crate; the bump rule and the pinnable `$id` live in `docs/tool-surface.schema.json`, and
`docs_claims.rs` asserts the script and the schema name the same one.

## The registry manifest

**`server.json` is the MCP registry manifest and `release.sh` bumps it** (REL-3, #137). It carries its own
`version`, so a manifest left behind tells a searcher a release exists that was never published — the same
class of defect as a stale pin. `the_registry_manifest_version_matches_the_crate` asserts it against
`Cargo.toml`, so the bump step cannot be quietly dropped. **It is deliberately metadata-only** — no
`packages` block — because the registry's direct-download type is `mcpb`, which wants a bundle this repo
does not build and a `fileSha256` for it; a listing that named an install method we do not have would
advertise something nobody can run. Validate any edit against the published schema before committing:
`description` is capped at **100 characters** and the first draft here was well over it.

## The release notes

**The release body reaches the releases page through `scripts/release-notes.py`**, and it did not until
v0.9.0. The workflow published with `--generate-notes`, which lists merged **pull requests** — so a release
of direct pushes to main generated an empty "What's Changed", and the commit body it never read is where all
the caller-visible detail lives. Every release from v0.2.1 to v0.8.0 published one line: the compare link.

The script now leads with that commit body verbatim and appends a changelog categorized from the
conventional-commit subjects since the previous tag, under the same emoji headings `~/html/b2c-next` uses.
Preview it with `python3 scripts/release-notes.py v<version>`; it is byte-for-byte what will be published,
and it also lands in the run's job summary. There is deliberately **no `.github/release.yml`** — that is
b2c-next's mechanism and it categorizes by PR *label*, which here would categorize almost nothing and look
load-bearing while deciding nothing.
