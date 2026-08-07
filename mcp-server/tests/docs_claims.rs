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
