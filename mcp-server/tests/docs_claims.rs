//! Tests for CLAIMS rather than for code (DOC-15, #145).
//!
//! `CLAUDE.md` is the most load-bearing document here and several of its assertions are *measurements* —
//! true when written, checked by nobody afterwards. The file already carries three post-mortems about
//! exactly that, including one whose own figures rotted in the weeks it took to read it, and the response
//! so far has been to write better warnings beside the numbers. That does not work: it asks a reader to
//! distrust a document at the moment they are consulting it for a fact.
//!
//! WHAT BELONGS HERE, and it is a narrower set than "every number in the docs". Two filters:
//!
//!   1. A stale value has to COST something. The ignored-test count has cost two investigations; a
//!      rounded wall-clock figure in a paragraph about why sharding is worth it has cost nothing.
//!   2. The claim has to have ONE authoritative source that a test can reach cheaply. Where it does not,
//!      #145's own alternative is better and is taken elsewhere in this commit: delete the number from the
//!      prose rather than pin it.
//!
//! The `--shard N/M` rule is the worked example of NOT testing something. `CLAUDE.md` concludes that a
//! written-down shard number is always stale, and a grep for `--shard \d/\d` looked like a way to
//! mechanise it — but every occurrence in this tree today is either the usage line of the script that
//! takes the flag or prose explaining why not to write one down. The test would fire only on the
//! documentation of its own rule, which is the must-not-fire failure that gets a check deleted.
//!
//! Several assertions below guard a `sed` rather than a number. Those are the cheapest kind to get wrong:
//! a sed that stops matching returns EMPTY, and empty reads like "nothing to report" everywhere it lands.

use std::fs;

fn read(path: &str) -> String {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/..");
    fs::read_to_string(format!("{root}/{path}")).unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
}

/// `scripts/doctor.sh` and `.github/workflows/toolchain-pin.yml` both read the pinned toolchain out of
/// `rust-toolchain.toml` with the same sed (LINT-5, #141). If it stops matching, doctor's
/// "your rustc is not the gate's" warning silently never fires and toolchain-pin errors out.
#[test]
fn the_toolchain_pin_is_readable_by_the_sed_two_scripts_use() {
    let toml = read("rust-toolchain.toml");
    let channel = toml
        .lines()
        .find_map(|l| l.trim().strip_prefix("channel")?.trim().strip_prefix('=')?.trim().strip_prefix('"')?.split('"').next())
        .expect("no `channel = \"...\"` in rust-toolchain.toml — the sed in scripts/doctor.sh and toolchain-pin.yml reads exactly this shape");
    assert!(
        channel.starts_with(char::is_numeric),
        "the pinned channel is {channel:?}. Both readers expect a version; `stable` here would make the \
         gate track whatever stable is, which is the drift LINT-1 (#18) pinned it to avoid."
    );

    for (path, needle) in [
        ("scripts/doctor.sh", "rust-toolchain.toml"),
        (".github/workflows/toolchain-pin.yml", "rust-toolchain.toml"),
    ] {
        assert!(
            read(path).contains(needle),
            "{path} no longer reads the pin from {needle}. If the pin moved, move both readers with it — \
             two copies of this number is what #141 removed."
        );
    }
}

/// The rust-doctor version, which is a different number from the toolchain above and had a worse story
/// (BUILD-3, #174). `.github/workflows/toolchain-pin.yml` used to run `npx -y "rust-doctor@${DOCTOR}"`
/// with `${DOCTOR}` sed'd out of `rust-doctor.yml` — and since BUILD-1 (#66) moved that gate off npm, the
/// only line in the file the sed still matched was the COMMENT explaining the npm removal. It answered
/// `0.2.0` by reading a sentence about the bug, and was right by accident.
///
/// That workflow now runs `scripts/doctor.sh`, so it names no version at all. Two declarations are left
/// and nothing else holds them together: doctor.sh's `RUST_DOCTOR_VERSION` default, which is what a
/// maintainer and the advisory scan both get, and the release URL `rust-doctor.yml` curls, which is the
/// gate. Drift between them means the advisory scan and the gate are different tools reporting under one
/// name — the shape #174 was filed about, moved rather than removed.
#[test]
fn the_two_rust_doctor_declarations_agree_and_the_advisory_scan_carries_neither() {
    let local = read("scripts/doctor.sh")
        .lines()
        .find_map(|l| {
            l.trim().strip_prefix("RUST_DOCTOR_VERSION=\"${RUST_DOCTOR_VERSION:-")?.split('}').next()
        })
        .map(str::to_owned)
        .expect(
            "no `RUST_DOCTOR_VERSION=\"${RUST_DOCTOR_VERSION:-<version>}\"` in scripts/doctor.sh. That \
             default is the version the local gate AND toolchain-pin.yml's advisory scan both run.",
        );

    // Anchored on `rust-doctor/` rather than on `/releases/download/v`, because rust-doctor.yml curls
    // actionlint from a URL of exactly that shape a few steps later. Comment lines are dropped before the
    // search for the same reason they are below: this whole test exists because a version was read out of
    // a sentence, and reading one out of a sentence to check that is not better for being a different file.
    let gate = read(".github/workflows/rust-doctor.yml")
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let ci = gate
        .split_once("rust-doctor/releases/download/v")
        .and_then(|(_, rest)| rest.split('/').next())
        .expect(
            "no pinned rust-doctor release URL in .github/workflows/rust-doctor.yml — if the gate's fetch \
             moved, move this with it (BUILD-1, #66 is why it is a release asset and not npx).",
        );
    assert_eq!(
        local, ci,
        "scripts/doctor.sh pins rust-doctor {local} and the gate curls {ci}. A local run that says \
         \"would pass\" would then be a different tool's verdict than the one that gates, and since \
         BUILD-3 (#174) the monthly advisory scan runs doctor.sh's, so it would disagree with the gate too."
    );

    // COMMENT LINES ARE EXCLUDED, and that exclusion IS the finding restated. The workflow's own comment
    // quotes the `npx -y "rust-doctor@${DOCTOR}"` call it removed, because the removal is the thing worth
    // explaining — so a check that could not tell prose from configuration would fire on the paragraph
    // describing the defect, which is precisely the mistake #174 is about.
    let pin_wf = read(".github/workflows/toolchain-pin.yml");
    let executable =
        pin_wf.lines().filter(|l| !l.trim_start().starts_with('#')).collect::<Vec<_>>().join("\n");
    assert!(
        !executable.contains("npx"),
        "toolchain-pin.yml invokes npx again. The `rust-doctor` npm package was unpublished on \
         2026-07-29 and the registry serves an empty version list, so every version is an ETARGET \
         (BUILD-1 #66, BUILD-3 #174)."
    );
    assert!(
        !executable.contains("rust-doctor@"),
        "toolchain-pin.yml carries a `rust-doctor@<version>` again. It should carry no rust-doctor \
         version: `scripts/doctor.sh` owns that number, and a third copy is what #174 declined to add."
    );
    assert!(
        executable.contains("scripts/doctor.sh --sarif"),
        "toolchain-pin.yml's advisory scan no longer goes through scripts/doctor.sh. That call is what \
         makes it run the same tool, at the same version, as a maintainer's local gate — and doctor.sh \
         `exec`s the binary for --sarif, so the redirect gets the tool's own bytes with nothing prepended."
    );
}

/// `scripts/doctor.sh` reads the gate's tool list with `head -1` over `tool:` lines, deliberately, and the
/// long comment there explains why repairing the previous bug naively would have been worse. That only
/// stays correct while the FIRST `tool:` line in the file is the health job's — the second belongs to the
/// `semver` job, whose tool is deliberately not part of the scan's environment.
#[test]
fn the_first_tool_line_in_rust_doctor_yml_is_the_health_jobs() {
    let wf = read(".github/workflows/rust-doctor.yml");
    let tool_lines: Vec<&str> = wf.lines().filter(|l| l.trim_start().starts_with("tool:")).collect();
    assert!(
        !tool_lines.is_empty(),
        "no `tool:` line at all; scripts/doctor.sh would read an empty tool list"
    );

    let first = tool_lines[0].trim();
    for expected in ["cargo-deny", "cargo-machete", "cargo-shear"] {
        assert!(
            first.contains(expected),
            "the FIRST `tool:` line is {first:?}, which does not install {expected}. scripts/doctor.sh takes \
             this line as the gate's environment (`head -1`); if the health job gained a second install \
             step, read the comment at CI_TOOLS in that script before moving anything."
        );
    }
}

/// Every check `scripts/doctor.sh` tells you "GATES in CI" has to actually be a step in the gate. This is
/// the direction that matters: the script's whole claim is that a clean local run is a green gate, and a
/// line saying a check gates when it does not is worse than not mentioning it.
///
/// **What it cannot check is that the command RUNS**, and that gap has been paid for once. The zizmor step
/// read `uvx zizmor` and this test asserted the workflow contained `uvx zizmor` — both true, and the step
/// still exited 127 on every CI run, because GitHub's runners have no `uvx`. The two files agreed with each
/// other about a command that did not exist. Consistency between a script and a workflow is worth asserting
/// and is not the same as either of them working; only a real run tells you that, which is why the needle is
/// now the full `run:` line and why the fix installs zizmor from the same pinned step as every other tool.
#[test]
fn every_check_doctor_says_gates_in_ci_is_a_step_in_rust_doctor_yml() {
    let wf = read(".github/workflows/rust-doctor.yml");
    let doctor = read("scripts/doctor.sh");

    for (label, command) in [
        ("unused dependencies (cargo-shear)", "cargo shear"),
        ("documentation (rustdoc)", "cargo doc --workspace --no-deps --document-private-items"),
        ("dependency policy (cargo-deny)", "cargo deny check"),
        ("spelling (typos)", "typos"),
        ("workflow lint (zizmor)", "run: zizmor --persona=regular"),
        ("workflow semantics (actionlint)", "run: /tmp/actionlint -shellcheck= -pyflakes="),
        ("CI script fixtures (scripts/tests/run.sh)", "run: bash scripts/tests/run.sh"),
    ] {
        assert!(
            doctor.contains(label),
            "scripts/doctor.sh no longer reports {label:?}. If the check was removed, remove it from the \
             gate too; if it was renamed, rename it here."
        );
        assert!(
            wf.contains(command),
            "scripts/doctor.sh says {label:?} gates in CI, but .github/workflows/rust-doctor.yml runs no \
             `{command}` step. One of the two is lying, and the script is the one people trust."
        );
    }
}

/// actionlint's version is written down in two places and they have to agree (CI-9, #166): the gate curls
/// a pinned release URL, and `scripts/doctor.sh` tells you which version the gate pins so a local binary
/// finding something CI will not is diagnosable rather than baffling. That second copy exists because the
/// script cannot read the URL — but two copies of a number is what this file is for.
///
/// taiki-e/install-action does not carry actionlint, which is why this one is a curl with a literal version
/// in it rather than an entry on the `tool:` line the test above guards.
#[test]
fn doctor_reports_the_actionlint_version_the_gate_actually_pins() {
    let wf = read(".github/workflows/rust-doctor.yml");
    let pinned =
        wf.lines().find_map(|l| l.split("/actionlint/releases/download/v").nth(1)?.split('/').next()).expect(
            "no pinned actionlint release URL in rust-doctor.yml — if the fetch moved, move this with it",
        );

    let doctor = read("scripts/doctor.sh");
    assert!(
        doctor.contains(&format!("CI pins {pinned}")),
        "the gate curls actionlint {pinned}, but scripts/doctor.sh does not say `CI pins {pinned}`. That \
         line is what tells you a local finding is your binary's rather than the gate's; a stale number \
         there sends you looking in the wrong place."
    );
    assert!(
        doctor.contains(&format!("actionlint_{pinned}_linux_amd64.tar.gz")),
        "scripts/doctor.sh's install hint does not offer actionlint {pinned}, which is what the gate runs. \
         Following it would install a different linter than the one whose verdict it is reporting."
    );
}

/// The published tool surface names its format in two files that have to agree (REL-8, #165):
/// `scripts/tool-surface.py` stamps `$schema` and `kind` into every document it emits, and
/// `docs/tool-surface.schema.json` declares the same two as `$id` and a `const`. A consumer pins the URL,
/// so a drift between them publishes a document that says it conforms to a schema it does not match.
///
/// The `surface_version` is deliberately NOT asserted equal to anything: it is a number that moves on its
/// own rule, written at the field in the schema, and pinning it here would be a third copy that has to be
/// bumped in lockstep with the two that matter.
#[test]
fn the_tool_surface_document_and_its_schema_name_the_same_format() {
    let script = read("scripts/tool-surface.py");
    let schema = read("docs/tool-surface.schema.json");

    let url = script
        .split("SCHEMA_URL = (\n    \"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("no SCHEMA_URL literal in scripts/tool-surface.py — if it moved, move this with it");
    assert!(
        url.contains("tool-surface.schema.json"),
        "SCHEMA_URL reads {url:?}, which does not look like the schema's URL"
    );
    assert!(
        schema.contains(&format!("\"$id\": \"{url}\"")),
        "the script stamps `$schema: {url}` into every published document, but \
         docs/tool-surface.schema.json does not declare that as its `$id`. A consumer pins that URL."
    );
    assert!(
        schema.contains(&format!("\"const\": \"{url}\"")),
        "the schema does not pin `$schema` to {url} as a const, so a document naming a different one \
         would still validate."
    );

    let kind = script
        .split("KIND = \"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("no KIND literal in scripts/tool-surface.py");
    assert!(
        schema.contains(&format!("\"const\": \"{kind}\"")),
        "the script stamps `kind: {kind}` and the schema does not pin that value. The root discriminator \
         exists so a reader branches on a tag instead of sniffing for fields; one that is not pinned is \
         not a discriminator."
    );
}

/// The guard's rules live in `scripts/guard.py` and nothing else may hold a copy of them (LINT-7, #167),
/// and `.claude/settings.json` is the one place its case counts are written down. Both are asserted here
/// because both are the kind of claim that goes quietly wrong.
///
/// The adapter check is the sharper half. `.claude/hooks/pre-bash-guard.py` is allowed to translate and
/// nothing more; the day it grows a rule of its own, two implementations start drifting, which is the
/// defect the move was for. A rule is recognisable by the strings every rule here ends with.
#[test]
fn the_guard_has_one_implementation_and_settings_json_states_its_real_case_count() {
    let matrix = read("scripts/guard.test.sh");
    let cases = matrix.lines().filter(|l| l.starts_with("check ") || l.starts_with("hook ")).count();
    assert!(cases > 20, "only {cases} cases in scripts/guard.test.sh — did the matrix lose its shape?");

    let settings = read(".claude/settings.json");
    assert!(
        settings.contains(&format!("runs {cases} cases")),
        "scripts/guard.test.sh has {cases} cases and .claude/settings.json does not say `runs {cases} \
         cases`. That comment block is the ONE place the guard's rationale and counts live, which only \
         works while the count is true."
    );

    let adapter = read(".claude/hooks/pre-bash-guard.py");
    for smell in ["SKIP_JDWP_AGENT_GUARD", "shlex", "RUSTC_BOOTSTRAP", "--test-threads", "--shard"] {
        assert!(
            !adapter.contains(smell),
            "the Claude Code adapter mentions {smell:?}, so it is holding policy rather than translating \
             it. Every rule belongs in scripts/guard.py — a rule with two implementations is the drift \
             LINT-7 (#167) moved them to avoid."
        );
    }
    assert!(
        adapter.contains("from guard import check"),
        "the adapter no longer calls scripts/guard.py's check(). If the entry point was renamed, rename \
         it here; if the adapter reimplemented it, that is the thing this test exists to refuse."
    );
}

/// `rust-version` is the MSRV the `msrv` job builds on, and that job reads it out of the manifest with a
/// sed rather than repeating it (BUILD-2, #142). An empty read there would install the empty string.
#[test]
fn the_msrv_job_can_read_rust_version_out_of_the_manifest() {
    let manifest = read("Cargo.toml");
    let declared = manifest
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("rust-version")?
                .trim()
                .strip_prefix('=')?
                .trim()
                .strip_prefix('"')?
                .split('"')
                .next()
        })
        .expect(
            "no `rust-version = \"...\"` in Cargo.toml — the sed in the msrv job reads exactly this shape",
        );

    let (major, minor) =
        declared.split_once('.').unwrap_or_else(|| panic!("rust-version {declared:?} is not X.Y"));
    assert_eq!(major, "1", "rust-version {declared:?} is not a 1.x release");
    assert!(
        minor.parse::<u32>().is_ok_and(|m| m >= 85),
        "rust-version is {declared:?}. 1.85 is a MEASURED floor, not a preference: below it `tempfile` -> \
         `getrandom 0.4.3` needs edition2024 and the test targets do not resolve at all. Lowering it needs \
         a run of `cargo +<version> check --workspace --all-targets`, not an edit here."
    );

    let tests = read(".github/workflows/tests.yml");
    assert!(
        tests.contains("rust-version") && tests.contains("Cargo.toml"),
        "the msrv job no longer reads rust-version from Cargo.toml. A workflow literal and a manifest \
         value that must move together is the second copy #142 declined to create."
    );
}

/// The licence allowlist is measured (CI-3, #148) and two of its four entries do no work today. That is
/// written down in `deny.toml` precisely so nobody removes them as clutter; this asserts the two that DO
/// bite are still there, since removing either turns the gate red for a reason the comment explains.
#[test]
fn the_load_bearing_licence_allowances_are_still_present() {
    let deny = read("deny.toml");
    for licence in ["MIT", "Unicode-3.0"] {
        assert!(
            deny.contains(licence),
            "deny.toml no longer allows {licence}, which was MEASURED as load-bearing: removing MIT \
             rejects 18 crates and removing Unicode-3.0 rejects unicode-ident, whose \
             \"(MIT OR Apache-2.0) AND Unicode-3.0\" has a clause the alternatives cannot satisfy."
        );
    }
}

/// The MCP registry manifest names a version, and a manifest that lags the release it describes tells a
/// searcher a version exists that nobody published (REL-3, #137). `scripts/release.sh` bumps it beside
/// Cargo.toml; this is what makes that non-optional rather than a step someone can quietly drop.
#[test]
fn the_registry_manifest_version_matches_the_crate() {
    let manifest = read("server.json");
    let declared = manifest
        .split("\"version\"")
        .nth(1)
        .and_then(|rest| rest.split('"').nth(1))
        .expect("no \"version\" in server.json");

    let crate_version = read("Cargo.toml")
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("version")?
                .trim()
                .strip_prefix('=')?
                .trim()
                .strip_prefix('"')?
                .split('"')
                .next()
        })
        .map(str::to_owned)
        .expect("no version in Cargo.toml");

    assert_eq!(
        declared, crate_version,
        "server.json says {declared} and Cargo.toml says {crate_version}. The registry manifest is \
         published to searchers; a version there that was never released is worse than no listing. \
         scripts/release.sh bumps both — if this drifted, that step was skipped or removed."
    );

    // REL-6 (#168). The `packages` block names the npm release the listing points at, and a package entry
    // pointing at a version nobody published is the same defect one level down — worse, because a reader
    // can act on it: `npx jdwp-mcp@<that>` is a command they will run.
    for entry in manifest.split("\"registryType\"").skip(1) {
        let pkg_version = entry
            .split("\"version\"")
            .nth(1)
            .and_then(|rest| rest.split('"').nth(1))
            .expect("a server.json package entry with no \"version\"");
        assert_eq!(
            pkg_version, crate_version,
            "a server.json package entry says {pkg_version} and Cargo.toml says {crate_version}. The \
             registry would advertise an install command for a version that does not exist."
        );
    }
}

/// Every npm manifest carries the crate's version, and the wrapper pins its platform packages to it
/// exactly (REL-6, #168).
///
/// The same hazard `the_registry_manifest_version_matches_the_crate` guards for `server.json`, six more
/// times over. #168 states it: *a manifest left behind tells a searcher a release exists that was never
/// published, and a third version number is a third chance at that.* There are now eight numbers that
/// must agree — `Cargo.toml`, `server.json` and six `package.json` files — and `scripts/release.sh` bumps
/// all of them.
///
/// The `optionalDependencies` pins are the second half and the sharper one. If the wrapper asked for
/// `jdwp-mcp-linux-x64@0.20.0` while the platform packages published `0.21.0`, `npx jdwp-mcp` would
/// install the *previous* release's binary and report the previous release's tool surface — a failure
/// with no error anywhere in it.
#[test]
fn every_npm_manifest_carries_the_crate_version() {
    const PLATFORMS: [&str; 5] = [
        "jdwp-mcp-linux-x64",
        "jdwp-mcp-linux-arm64",
        "jdwp-mcp-darwin-arm64",
        "jdwp-mcp-darwin-x64",
        "jdwp-mcp-win32-x64",
    ];

    let crate_version = read("Cargo.toml")
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("version")?
                .trim()
                .strip_prefix('=')?
                .trim()
                .strip_prefix('"')?
                .split('"')
                .next()
        })
        .map(str::to_owned)
        .expect("no version in Cargo.toml");

    let field = |json: &str, key: &str| -> Option<String> {
        json.split(&format!("\"{key}\"")).nth(1)?.split('"').nth(1).map(str::to_owned)
    };

    for pkg in std::iter::once("jdwp-mcp").chain(PLATFORMS) {
        let path = format!("npm/{pkg}/package.json");
        let manifest = read(&path);
        let declared = field(&manifest, "version").unwrap_or_else(|| panic!("no \"version\" in {path}"));
        assert_eq!(
            declared, crate_version,
            "{path} says {declared} and Cargo.toml says {crate_version}. `npx {pkg}` would serve a \
             version nobody released. scripts/release.sh bumps every npm manifest — if this drifted, \
             that step was skipped or removed."
        );
    }

    // The wrapper pins each platform package to an EXACT version, not a range: a caret would let npm
    // resolve a newer binary than the wrapper was published with.
    let wrapper = read("npm/jdwp-mcp/package.json");
    let optional = wrapper.split("\"optionalDependencies\"").nth(1).unwrap_or_else(|| {
        panic!(
            "npm/jdwp-mcp/package.json has no optionalDependencies — that block IS \
                                   how the platform binaries reach an installer"
        )
    });
    for pkg in PLATFORMS {
        let pinned = optional
            .split(&format!("\"{pkg}\""))
            .nth(1)
            .and_then(|rest| rest.split('"').nth(1))
            .unwrap_or_else(|| panic!("{pkg} is missing from the wrapper's optionalDependencies"));
        assert_eq!(
            pinned, crate_version,
            "the wrapper pins {pkg} at {pinned} but this release is {crate_version}. `npx jdwp-mcp` \
             would install the wrong release's binary and report its tool surface, with no error \
             anywhere. An exact pin is required — a caret or a tilde is the same bug with a delay."
        );
    }
}

/// Every script a workflow runs DIRECTLY is executable in git's index.
///
/// `scripts/sarif-for-code-scanning.py` lost mode 755 in 83c7c05 — an editor rewrote the file and git
/// recorded 100644 — and `rust-doctor.yml` invokes it as `scripts/sarif-for-code-scanning.py`, so the step
/// died with **exit 126**, "found but not executable". Nothing local caught it: `scripts/tests/run.sh`
/// calls the same script as `python3 scripts/…`, which needs no mode bit, so the fixture matrix stayed
/// green while CI could not start it at all.
///
/// The mode is a fact about the INDEX, not the working tree — `chmod +x` alone does not fix a committed
/// 100644 — so this reads `git ls-files -s`, which is what a fresh checkout will get.
#[test]
fn every_script_a_workflow_runs_directly_is_executable() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/..");
    let listing =
        std::process::Command::new("git").args(["ls-files", "-s", "scripts/"]).current_dir(root).output();
    let Ok(out) = listing else { return }; // no git (a vendored tarball) — nothing to assert against
    if !out.status.success() {
        return;
    }
    let modes: std::collections::HashMap<&str, &str> = std::str::from_utf8(&out.stdout)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (meta, path) = l.split_once('\t')?;
            Some((path, meta.split_whitespace().next()?))
        })
        .collect();

    // Every `scripts/…` token a workflow invokes with no interpreter in front of it.
    let mut wanted: Vec<String> = Vec::new();
    for wf in std::fs::read_dir(format!("{root}/.github/workflows")).into_iter().flatten().flatten() {
        let body = std::fs::read_to_string(wf.path()).unwrap_or_default();
        for line in body.lines() {
            let t = line.trim().trim_start_matches("run:").trim().trim_start_matches("- ").trim();
            for tok in t.split_whitespace() {
                let tok = tok.trim_start_matches('$').trim_start_matches('(');
                if let Some(rest) = tok.strip_prefix("scripts/") {
                    // Only the direct form. `python3 scripts/x.py` and `bash scripts/x.sh` are fine at
                    // any mode, and that difference is exactly what hid this bug.
                    let first = t.split_whitespace().next().unwrap_or("");
                    if first == tok && !rest.is_empty() {
                        wanted.push(format!("scripts/{rest}"));
                    }
                }
            }
        }
    }
    wanted.sort();
    wanted.dedup();
    assert!(!wanted.is_empty(), "no directly-invoked scripts/ found — this test stopped looking");

    for path in &wanted {
        let Some(mode) = modes.get(path.as_str()) else { continue };
        assert_eq!(
            *mode, "100755",
            "{path} is {mode} in git's index, and a workflow runs it directly — that is exit 126 on a \
             fresh checkout. Fix with `git update-index --chmod=+x {path}`; `chmod +x` alone changes the \
             working tree and not what CI clones."
        );
    }
}

/// `.githooks/commit-msg` accepts exactly the vocabulary `release-notes.py` categorises on, by asking the
/// script for it (REL-4, #147). The flag it asks with is the whole mechanism.
#[test]
fn release_notes_still_offers_the_list_the_commit_msg_hook_asks_for() {
    let script = read("scripts/release-notes.py");
    assert!(
        script.contains("--list-types"),
        "scripts/release-notes.py lost --list-types. .githooks/commit-msg calls it for the vocabulary and \
         SKIPS THE CHECK when the call fails, so this goes quiet rather than red."
    );
    let hook = read(".githooks/commit-msg");
    assert!(
        hook.contains("--list-types"),
        "the commit-msg hook no longer reads the vocabulary from release-notes.py. A second copy of that \
         list is exactly what #147 declined commitlint over."
    );
    for kind in ["merge", "feat", "fix"] {
        assert!(
            script.contains(&format!("\"{kind}\"")),
            "{kind:?} is gone from release-notes.py's vocabulary. `merge:` appears 10 times in this \
             history and the compound `fix(lint)+docs:` form 13 times; both were landing in Other Changes \
             before #147, one of them with its type stripped."
        );
    }
}

/// `scripts/semver-check.sh` runs `cargo semver-checks --workspace`, so what it covers is not a choice the
/// script makes — it is "every package in this workspace with a lib target", and the header only restates
/// it. That restatement rotted the moment CLEAN-3 (#186) gave `jdwp-mcp` a `[lib]`: the sentence there said
/// `jdwp-client` was "the only lib target in the workspace — `jdwp-mcp` is a `[[bin]]` … so it contributes
/// nothing to this check", which had been true right up to that commit and silently false after it.
///
/// This asserts both directions, because the claim is wrong either way round: if `mcp-server` has a lib
/// target the header must say so, and if it ever loses one the header must stop saying so.
#[test]
fn semver_check_names_the_lib_targets_it_actually_reads() {
    let script = read("scripts/semver-check.sh");
    assert!(
        script.contains("--workspace"),
        "scripts/semver-check.sh no longer passes --workspace, so what it covers is now a choice the \
         script makes rather than a fact about the manifests — and the reasoning below no longer applies."
    );

    let manifest = read("mcp-server/Cargo.toml");
    let mcp_is_a_lib = manifest.contains("[lib]");
    assert_eq!(
        mcp_is_a_lib,
        script.contains("`jdwp-mcp`"),
        "mcp-server/Cargo.toml {} a [lib], and scripts/semver-check.sh's `Covers:` paragraph {} name \
         `jdwp-mcp`. A --workspace run reads every lib target there is; a header that disagrees with the \
         manifests is the claim #186 had to correct in the commit that falsified it.",
        if mcp_is_a_lib { "has" } else { "does not have" },
        if mcp_is_a_lib { "does not" } else { "still does" },
    );

    assert!(
        !script.contains("the only lib target in the workspace"),
        "scripts/semver-check.sh is back to claiming one lib target. There are two."
    );
}

/// The snapshot regeneration command names a cargo target, and naming the wrong one is a VACUOUS PASS
/// rather than an error: `cargo test --bin jdwp-mcp _snapshot` runs 0 tests and exits 0. It was the right
/// command until CLEAN-3 (#186) moved the modules — and with them all 229 unit tests — behind a `[lib]`.
///
/// The command appears in 17 places, including the generated header of all three snapshot files, so this
/// checks the one property that makes any of them true: the tests are in the target the command selects.
/// `CLAUDE.md` calls reading the regenerated diff "the mechanism"; a command that regenerates nothing
/// retires the mechanism while still printing `ok`.
#[test]
fn the_snapshot_regeneration_command_names_the_target_the_tests_are_in() {
    let manifest = read("mcp-server/Cargo.toml");
    assert!(
        manifest.contains("[lib]"),
        "mcp-server has no [lib], so the unit tests are back in the bin and every `-p jdwp-mcp --lib` \
         below now runs 0 tests and exits 0."
    );

    for path in [
        "CLAUDE.md",
        "docs/tools.md",
        "docs/toolkit-contract.md",
        "mcp-server/src/tools.rs",
        "mcp-server/src/handlers.rs",
        "mcp-server/tests/tool-descriptions.txt",
        "mcp-server/tests/argument-schemas.txt",
        "mcp-server/tests/reply-fragments.txt",
    ] {
        let body = read(path);
        assert!(
            !body.contains("--bin jdwp-mcp"),
            "{path} tells a reader to regenerate with `cargo test --bin jdwp-mcp`. The bin has no tests: \
             that command runs 0 of them and exits 0, so the reader gets a green run and an unchanged \
             snapshot. The working form is `cargo test -p jdwp-mcp --lib _snapshot`."
        );
    }
}
