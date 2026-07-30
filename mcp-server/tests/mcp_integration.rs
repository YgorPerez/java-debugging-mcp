// MCP-level integration tests: the real `jdwp-mcp` binary, driven over JSON-RPC on stdio, against a
// real JVM. These cover the handler glue that unit tests can't reach — expression resolution, the
// event pump, deferred-breakpoint arming, force-return, watchpoints, session bookkeeping.
//
// Each test launches and reaps its own probe JVM on its own port, so they are independent and can run
// concurrently. They are `#[ignore]`d because they need a JDK and take seconds:
//
//     scripts/integration-test.sh
//     cargo test --test mcp_integration -- --ignored --nocapture   # also shows SKIP reasons
//
// Without a JDK each test prints SKIP and passes, so a JDK-less CI stays green rather than red.
//
// **Four tests here are NOT `#[ignore]`d**, and they are the point of TEST-12 (#37): they drive the same
// server against a recorded JDWP session instead of a JVM (`common/cassette.rs`, ADR-0014). They need no
// JDK and no Java, so they run in the default `cargo test` — a test that needs no JDK must not hide behind
// the flag that exists for tests that do (TEST-9, #25). Note the corollary:
// `scripts/integration-test.sh` passes `--ignored`, which runs ONLY ignored tests, so it does not run
// them. Both commands are needed to see everything in this file.

mod common;

use common::cassette::{cassette_path, rerecording, Cassette, CassetteRecorder, ReplayServer, RERECORD_ENV};
use common::{
    assert_contains_all, jdk_or_skip, probe_line, probe_source, probe_source_path, refusal_verdict,
    resume_verdict, EventFault, Fault, FaultRelay, Jdk, JvmState, LatencyRelay, Probe, Server,
    EVENT_KIND_BREAKPOINT, EVENT_TIMEOUT,
};

/// EVAL-1 / EVAL-2: static-method invocation and object arguments, through the real handlers.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
// A linear script against one probe: arrange, then a dozen evaluations that only mean something
// against the state the ones before them established. Split, each half would need its own probe and
// neither could claim the other's setup. Over 100 lines only since the rustfmt adoption (#44) split
// the lines that ran past 110.
#[allow(clippy::too_many_lines)]
fn evaluate_static_methods_and_object_arguments() {
    let Some(jdk) = jdk_or_skip("evaluate_static_methods_and_object_arguments") else { return };
    let probe = Probe::launch(&jdk, "EvalProbe").expect("launch EvalProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let source = probe_source("EvalProbe");
    let line = probe_line(&source, "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "EvalProbe", "line": line}));
    let hit = server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in EvalProbe.work never fired");
    assert_contains_all("breakpoint hit", &hit, &["\"method\":\"work\"", "\"event\":\"breakpoint\""]);

    // --- EVAL-1: static methods off a class head ---
    assert_contains_all("static int method", &server.evaluate("EvalProbe.twice(21)"), &["(int) 42"]);
    assert_contains_all("two int args", &server.evaluate("EvalProbe.sum(1, 2)"), &["(int) 3"]);
    assert_contains_all("String arg", &server.evaluate("EvalProbe.greet(\"world\")"), &["hello world"]);
    // A static field read must keep working alongside the call path.
    assert_contains_all("static field", &server.evaluate("EvalProbe.infra"), &["PROD"]);
    // A static call result is an ordinary head, so it keeps chaining.
    assert_contains_all(
        "chained off a static call",
        &server.evaluate("EvalProbe.infraName().length()"),
        &["(int) 4"],
    );
    assert_contains_all(
        "field then instance call",
        &server.evaluate("EvalProbe.holder.label()"),
        &["holder#3"],
    );
    assert_contains_all(
        "unknown static method",
        &server.evaluate("EvalProbe.noSuchMethod(1)"),
        &["has no static method"],
    );

    // --- EVAL-2: expressions as arguments, passed by reference ---
    assert_contains_all(
        "local as arg to instance method",
        &server.evaluate("a.matches(b)"),
        &["(boolean) true"],
    );
    assert_contains_all(
        "local as arg to static method",
        &server.evaluate("EvalProbe.describe(a)"),
        &["item:alpha/1"],
    );
    assert_contains_all("two object args", &server.evaluate("EvalProbe.sameName(a, b)"), &["(boolean) true"]);
    assert_contains_all("static field as arg", &server.evaluate("a.plus(EvalProbe.base)"), &["(int) 8"]);
    assert_contains_all("nested call as arg", &server.evaluate("EvalProbe.twice(a.plus(1))"), &["(int) 4"]);
    assert_contains_all(
        "field path as arg",
        &server.evaluate("EvalProbe.describe(EvalProbe.holder)"),
        &["item:holder/3"],
    );
    assert_contains_all(
        "unresolvable arg names itself",
        &server.evaluate("a.matches(nosuchlocal)"),
        &["argument 'nosuchlocal'"],
    );

    // --- Overload resolution by runtime type (every candidate parameter is tag 'L') ---
    assert_contains_all("Item overload", &server.evaluate("EvalProbe.pick(a)"), &["Item:alpha"]);
    assert_contains_all("String overload", &server.evaluate("EvalProbe.pick(\"x\")"), &["String:x"]);
    // pick(int) exists but is an instance method, and the static overloads all take a reference.
    // Handing the JVM an int for a reference parameter SIGSEGVs it, so this must be refused.
    assert_contains_all(
        "int for a reference parameter is refused",
        &server.evaluate("EvalProbe.pick(9)"),
        &["has no static method"],
    );

    // --- EVAL-3: parameters the superclass chain can't settle ---
    // An interface never appears in an argument's superclass chain, so these need the JVM's own answer.
    assert_contains_all(
        "directly implemented interface",
        &server.evaluate("EvalProbe.takesRunnable(EvalProbe.task)"),
        &["Runnable"],
    );
    // Subtask implements Runnable only through its superclass — the case a direct-superinterface query
    // misses, and the reason the walk has to be transitive.
    assert_contains_all(
        "interface inherited via superclass",
        &server.evaluate("EvalProbe.takesRunnable(EvalProbe.subtask)"),
        &["Runnable"],
    );
    // And on a JDK class we don't own.
    assert_contains_all(
        "interface on a library type",
        &server.evaluate("EvalProbe.takesComparable(\"x\")"),
        &["Comparable"],
    );
    // The negative case, which is the whole point: an argument that does NOT implement the interface
    // must be refused, not passed anyway by a blind arity/kind fallback.
    assert_contains_all(
        "an object that doesn't implement the interface is refused",
        &server.evaluate("EvalProbe.takesRunnable(a)"),
        &["has no static method"],
    );
    assert_contains_all(
        "an unrelated class parameter is refused",
        &server.evaluate("EvalProbe.takesThread(a)"),
        &["has no static method"],
    );
    // Autoboxing: an int argument selects f(Integer), and a real Integer reaches the method.
    assert_contains_all(
        "int boxes into Integer",
        &server.evaluate("EvalProbe.takesInteger(5)"),
        &["Integer:5"],
    );
    // Array covariance — a String[] is an Object[], which no signature comparison can tell you.
    assert_contains_all(
        "array covariance",
        &server.evaluate("EvalProbe.takesObjects(EvalProbe.words)"),
        &["Object[]:2"],
    );
    // The cheap path must be unchanged: an exact match still wins without asking the JVM anything.
    assert_contains_all(
        "exact overload still preferred",
        &server.evaluate("EvalProbe.pick(a)"),
        &["Item:alpha"],
    );

    // --- Conditions whose right-hand side is an expression, not a literal ---
    // Each gets its own line so its hit is distinguishable from every earlier one by line number.
    // Both hold on every iteration, so a miss means the condition failed to evaluate.
    for (marker, condition) in [("// BP2", "a.name == b.name"), ("// BP3", "check == EvalProbe.twice(local)")]
    {
        let cond_line = probe_line(&source, marker);
        server.panic_reset();
        server.call(
            "debug.set_line_stop",
            serde_json::json!({"class_pattern": "EvalProbe", "line": cond_line, "condition": condition}),
        );
        assert!(
            server.wait_for_event(&format!("\"line\":{cond_line}"), EVENT_TIMEOUT).is_some(),
            "condition `{condition}` never fired at line {cond_line}"
        );
    }

    server.panic_reset();
}

/// WATCH-1: field watchpoints, including the old → new pair on a write.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
// Writes and reads are asserted against the SAME armed watchpoint and the same probe run — the point
// is that one stop point reports both, which two tests could not show. Crossed 100 lines when the
// rustfmt adoption (#44) split its over-110 lines.
#[allow(clippy::too_many_lines)]
fn watchpoints_report_field_writes_and_reads() {
    let Some(jdk) = jdk_or_skip("watchpoints_report_field_writes_and_reads") else { return };
    // Running, not merely listening: a watchpoint cannot be deferred, so arming one against a class the
    // JVM has not loaded is refused outright. This test passed for months on borrowed time — readiness
    // used to poll the port on a 100ms tick, which handed every test ~50ms of slack it never asked for,
    // and TEST-20 (#55) removed that when it stopped dialling the port. The dependency was always here;
    // only the accident that hid it is gone. Saying it out loud is #49's fix, one test further on.
    let probe =
        Probe::launch_running(&jdk, "WatchProbe", |l| tick_index(l).is_some()).expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // --- Static field, modification. bumpCounter() does `counter = counter + 1`, so the pair must be
    // one apart — which only holds if the old value is read before the pending store commits.
    server.panic_reset();
    let set = server.call(
        "debug.set_field_stop",
        serde_json::json!({"class_name": "WatchProbe", "field_name": "counter"}),
    );
    assert_contains_all("static watch set", &set, &["watch_modify_", "static int"]);
    let hit =
        server.wait_for_event("field_modification", EVENT_TIMEOUT).expect("counter write never reported");
    assert_contains_all(
        "static write hit",
        &hit,
        &["\"method\":\"bumpCounter\"", "\"field\":\"WatchProbe.counter\"", "\"static\":true"],
    );
    let (old, new) = parse_old_new(&hit).unwrap_or_else(|| panic!("no old/new ints in: {hit}"));
    assert_eq!(new, old + 1, "expected a single increment, got {old} → {new}");

    // --- Instance field, modification. relabel() alternates "even"/"odd", so old != new.
    server.panic_reset();
    let set = server.call(
        "debug.set_field_stop",
        serde_json::json!({"class_name": "WatchProbe$Holder", "field_name": "label"}),
    );
    assert_contains_all("instance watch set", &set, &["watch_modify_", "instance java.lang.String"]);
    let hit = server.wait_for_event("field_modification", EVENT_TIMEOUT).expect("label write never reported");
    assert_contains_all(
        "instance write hit",
        &hit,
        &[
            "\"method\":\"relabel\"",
            "\"field\":\"WatchProbe$Holder.label\"",
            "\"static\":false",
            "\"instance\":\"0x",
        ],
    );
    let distinct = ["even", "odd", "start"].iter().filter(|w| hit.contains(*w)).count();
    assert!(distinct >= 2, "expected two distinct label strings in: {hit}");

    // --- Access watch. readOnly is never written, so only an access watch fires on it, and it
    // reports a single value rather than a pair.
    server.panic_reset();
    let set = server.call(
        "debug.set_field_stop",
        serde_json::json!({"class_name": "WatchProbe", "field_name": "readOnly", "modify": false, "access": true}),
    );
    assert_contains_all("access watch set", &set, &["watch_access_"]);
    let hit = server.wait_for_event("field_access", EVENT_TIMEOUT).expect("readOnly read never reported");
    assert_contains_all(
        "access hit",
        &hit,
        &["\"method\":\"readConfig\"", "\"field\":\"WatchProbe.readOnly\"", "\"value\":\"(int) 41\""],
    );
    assert!(!hit.contains("\"old\""), "an access hit must not report old/new: {hit}");

    // --- Bookkeeping: a watch survives ClearAllBreakpoints, so list/clear/panic must know about it.
    server.panic_reset();
    let set = server.call(
        "debug.set_field_stop",
        serde_json::json!({"class_name": "WatchProbe", "field_name": "counter", "modify": true, "access": true}),
    );
    assert_contains_all("modify+access makes two requests", &set, &["watch_modify_", "watch_access_"]);
    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_contains_all("listed", &listed, &["2 watchpoint(s)", "WatchProbe.counter"]);

    let one = set
        .split_whitespace()
        .find(|w| w.starts_with("watch_modify_"))
        .map(|w| w.trim_end_matches(',').to_string())
        .expect("no watch_modify_ id in set output");
    let cleared = server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": one}));
    assert_contains_all("cleared one", &cleared, &["Watchpoint cleared", "WatchProbe.counter"]);
    assert_contains_all(
        "the other survives",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &["1 watchpoint(s)"],
    );
    assert_contains_all("panic reports watchpoints", &server.panic_reset(), &["watchpoint"]);
    assert_contains_all(
        "nothing left",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &["No breakpoints set"],
    );

    // --- Argument validation.
    assert_contains_all(
        "unknown field",
        &server.call(
            "debug.set_field_stop",
            serde_json::json!({"class_name": "WatchProbe", "field_name": "nope"}),
        ),
        &["has no field 'nope'"],
    );
    assert_contains_all(
        "unloaded class",
        &server.call(
            "debug.set_field_stop",
            serde_json::json!({"class_name": "NoSuchClass", "field_name": "x"}),
        ),
        &["not loaded yet"],
    );
    assert_contains_all(
        "neither kind selected",
        &server.call(
            "debug.set_field_stop",
            serde_json::json!({"class_name": "WatchProbe", "field_name": "counter", "modify": false, "access": false}),
        ),
        &["at least one"],
    );

    server.panic_reset();
}

/// TEST-1: a breakpoint set on a class that is NOT yet loaded must arm itself when the class loads
/// and then fire — the `CLASS_PREPARE` path through the event pump.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn deferred_breakpoint_arms_when_its_class_loads() {
    let Some(jdk) = jdk_or_skip("deferred_breakpoint_arms_when_its_class_loads") else { return };
    let mut probe = Probe::launch(&jdk, "DeferredProbe").expect("launch DeferredProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // The probe idles until told to load, so LateWorker is genuinely absent right now. Setting the
    // breakpoint before the cue is the whole point of the test.
    probe.wait_for_line(EVENT_TIMEOUT, |l| l.contains("ready")).expect("probe never printed ready");

    let line = probe_line(&probe_source("DeferredProbe"), "// BP1");
    let set =
        server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "LateWorker", "line": line}));
    assert_contains_all(
        "deferred breakpoint is accepted and says it is deferred",
        &set.to_lowercase(),
        &["lateworker"],
    );
    assert!(
        set.contains("defer") || set.contains("not loaded") || set.contains("when the class"),
        "expected the reply to say the breakpoint is deferred, got: {set}"
    );
    // It should be listed as deferred, not armed.
    assert_contains_all(
        "listed as deferred",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &["1 deferred"],
    );

    // Now let the class load. ClassPrepare → the pump arms the real breakpoint → it hits.
    probe.send_line("load").expect("cue the probe");
    let hit = server.wait_for_event("\"class\":\"LateWorker\"", EVENT_TIMEOUT).unwrap_or_else(|| {
        // The probe's own output (stdout + stderr) is the difference between "the debugger is
        // broken" and "the probe threw before it ever reached the method".
        panic!(
            "deferred breakpoint never armed or never fired after LateWorker loaded.\n  \
             probe output: {:?}\n  breakpoints: {}",
            probe.output(),
            server.call("debug.list_stop_points", serde_json::json!({}))
        )
    });
    assert_contains_all(
        "deferred hit",
        &hit,
        &["\"event\":\"breakpoint\"", "\"method\":\"work\"", &format!("\"line\":{line}")],
    );

    // The frame must be real and inspectable, not just a location — this is what proves the
    // breakpoint was armed properly rather than the event being an artefact of class loading.
    assert_contains_all("locals are readable at the deferred hit", &server.evaluate("n"), &["(int)"]);
    assert_contains_all(
        "static field on the late class",
        &server.evaluate("LateWorker.label"),
        &["late-worker"],
    );

    // Once armed it is a normal breakpoint, so it must now appear as armed rather than deferred.
    assert_contains_all(
        "promoted from deferred to armed",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &["0 deferred"],
    );

    server.panic_reset();
}

/// FILT-3/FILT-4: one wildcard pattern must arm a breakpoint on every matching LOADED class, keep arming
/// matching classes that load later, and be droppable — all of it — with the one `bpset_` id it returned.
///
/// The three properties are one test on purpose, because they are one promise: a wildcard is only usable if
/// the caller can see what it armed, trust that it keeps up with class loading, and take it all back. Any
/// one of them alone is a stop point you cannot reason about on a shared JVM.
///
/// `trace:true` throughout, for two reasons: the probe keeps calling these methods in a loop, so a
/// suspending family would freeze on the first hit and the rest of the test would be about resuming; and
/// snapshots are what let the test prove each member actually FIRES rather than merely appearing in a reply.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_wildcard_family_arms_every_match_grows_with_class_loading_and_clears_as_one() {
    let Some(jdk) =
        jdk_or_skip("a_wildcard_family_arms_every_match_grows_with_class_loading_and_clears_as_one")
    else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "FamilyProbe").expect("launch FamilyProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);
    // FamilyAlpha/Beta/NoMethod are constructed before this line is printed, so they are genuinely loaded
    // when we arm — and FamilyGamma genuinely is not.
    probe.wait_for_line(EVENT_TIMEOUT, |l| l.contains("ready")).expect("probe never printed ready");

    // A line number is refused rather than armed on four classes with four unrelated meanings.
    assert_contains_all(
        "a wildcard refuses a line number",
        &server.call(
            "debug.set_line_stop",
            serde_json::json!({"class_pattern": "Family*", "line": 42, "method": "handle"}),
        ),
        &["wildcard", "line 42"],
    );

    let set = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "Family*", "method": "handle", "trace": true}),
    );
    assert_contains_all(
        "one call arms every matching loaded class, under one family id",
        &set,
        &["bpset_", "FamilyAlpha", "FamilyBeta"],
    );
    assert!(
        !set.contains("FamilyGamma"),
        "FamilyGamma is not loaded yet, so nothing can be armed on it — the watch is what must catch it \
         later.\n  arm reply: {set}"
    );
    // FamilyProbe and FamilyNoMethod match `Family*` and declare no `handle`. That is the majority case for
    // a broad pattern and must be counted rather than reported as failures.
    assert_contains_all(
        "classes that are not targets are counted, not failed",
        &set,
        &["no method 'handle'"],
    );
    let bpset = find_id(&set, "bpset_").unwrap_or_else(|| panic!("no bpset_ id in the reply: {set}"));

    // Both armed members must actually fire, or "armed" was a claim about our own bookkeeping.
    let traces = server
        .wait_for_traces("FamilyAlpha", EVENT_TIMEOUT)
        .unwrap_or_else(|| panic!("no snapshot from FamilyAlpha.\n  probe output: {:?}", probe.output()));
    assert!(
        traces.contains("FamilyAlpha") && server.wait_for_traces("FamilyBeta", EVENT_TIMEOUT).is_some(),
        "both members of the family must fire, not just the first one armed.\n  traces: {traces}"
    );

    // The part no arming reply could have reported: a class that loads minutes later.
    probe.send_line("load").expect("cue the probe to load FamilyGamma");
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("gamma loaded"))
        .expect("probe never loaded FamilyGamma");
    let late = server.wait_for_traces("FamilyGamma", EVENT_TIMEOUT).unwrap_or_else(|| {
        panic!(
            "the family never armed FamilyGamma after it loaded — its class-prepare watch is what should \
             have done it.\n  probe output: {:?}\n  stop points: {}",
            probe.output(),
            server.call("debug.list_stop_points", serde_json::json!({}))
        )
    });
    assert!(late.contains("FamilyGamma"), "traces: {late}");

    let list = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_contains_all(
        "the listing reports the family and that it grew after the arming reply",
        &list,
        &[&bpset, "armed since"],
    );

    // One id takes all of it: three breakpoints and the watch. A watch left behind would keep arming
    // classes for a family the caller believes is gone.
    let cleared = server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": bpset}));
    assert_contains_all("one call drops the whole family", &cleared, &["family cleared", "watch"]);
    assert_contains_all(
        "nothing is left armed",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &["No breakpoints set"],
    );

    // FILT-4: several patterns in one call, whose normal outcome is PARTIAL — two armed, one deferred —
    // and which must report all three rather than failing on the entry that could not arm now.
    let batch = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": ["FamilyAlpha", "FamilyBeta", "NoSuchClassAtAll"],
            "method": "handle",
            "trace": true
        }),
    );
    assert_contains_all(
        "a batch reports every entry's outcome",
        &batch,
        &["3 pattern(s)", "2 trace breakpoint(s) armed", "1 deferred", "FamilyAlpha", "NoSuchClassAtAll"],
    );

    server.panic_reset();
}

/// FILT-5: a family that is FULL must stop watching for classes it cannot arm, and start again when a slot
/// frees.
///
/// The cost this proves is gone cannot be observed from outside — a `CLASS_PREPARE` event, a suspension of the
/// loading thread and a resume, on every class load — so the test asserts on the two places the state is
/// reported and then on the BEHAVIOUR that state implies: after unparking, a class loading later is armed
/// again. That last step is what separates a real fix from a listing that merely claims one, and it is why
/// this uses `FamilyGamma`, which loads only on a cue, long after the arming reply was written.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_full_family_parks_its_class_load_watch_and_takes_it_back_when_a_member_is_cleared() {
    let Some(jdk) =
        jdk_or_skip("a_full_family_parks_its_class_load_watch_and_takes_it_back_when_a_member_is_cleared")
    else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "FamilyProbe").expect("launch FamilyProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);
    probe.wait_for_line(EVENT_TIMEOUT, |l| l.contains("ready")).expect("probe never printed ready");

    // `max_classes: 1` against a pattern with two loaded targets: the family fills up as it arms, so it is
    // full before the reply is even written.
    let set = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "Family*", "method": "handle", "trace": true, "max_classes": 1}),
    );
    assert_contains_all(
        "a family that filled up says it armed one, refused the rest, and is NOT watching for more",
        &set,
        &["bpset_", "max_classes: 1", "NOT watching for classes that load later"],
    );
    let bpset = find_id(&set, "bpset_").unwrap_or_else(|| panic!("no bpset_ id in the reply: {set}"));
    let member = find_id(&set, "bp_").unwrap_or_else(|| panic!("no member bp_ id in the reply: {set}"));

    // The listing has to distinguish "parked because full" from the two other ways a family can be not
    // watching — a caller reading "not watching" alone cannot tell whether clearing a member would help.
    assert_contains_all(
        "the listing says the watch is parked, not that it failed or that the family is off",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &[&bpset, "FULL at max_classes: 1", "watch is parked"],
    );

    // Clearing the member frees the slot, so the family can grow again and must therefore be listening
    // again — and the reply says so, because it changes what happens to the next class the JVM loads.
    let cleared = server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": member}));
    assert_contains_all(
        "clearing a member reports that the family it belonged to is watching again",
        &cleared,
        &["freed a slot", &bpset, "watching again"],
    );

    // The proof: a class that loads NOW is armed, which only an unparked watch can do.
    probe.send_line("load").expect("cue the probe to load FamilyGamma");
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("gamma loaded"))
        .expect("probe never loaded FamilyGamma");
    let late = server.wait_for_traces("FamilyGamma", EVENT_TIMEOUT).unwrap_or_else(|| {
        panic!(
            "the family never armed FamilyGamma, so its watch did not come back when the slot freed — a \
             parked watch that never unparks is worse than one that was never parked.\n  probe output: \
             {:?}\n  stop points: {}",
            probe.output(),
            server.call("debug.list_stop_points", serde_json::json!({}))
        )
    });
    assert!(late.contains("FamilyGamma"), "traces: {late}");

    // And arming it filled the family again, so the watch is parked again — the cycle, not a one-off.
    assert_contains_all(
        "a family that fills up a second time parks its watch a second time",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &["FULL at max_classes: 1", "watch is parked"],
    );

    server.panic_reset();
}

/// FILT-3/FILT-4 on the other three arming tools: a list and a wildcard must work there too, and each kind
/// must report the thing that is specific to it.
///
/// The three differ in how a pattern can even be honoured, which is the point of testing them together:
/// exception and field stops must EXPAND a wildcard over loaded classes, because those event kinds need a
/// concrete reference type; method-exit passes the pattern to the JVM and expands nothing. A field wildcard
/// also has to distinguish "matched but has no such field" (expected, a note) from a failure.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn batched_and_wildcard_arming_works_for_exception_field_and_method_exit_stops() {
    let Some(jdk) =
        jdk_or_skip("batched_and_wildcard_arming_works_for_exception_field_and_method_exit_stops")
    else {
        return;
    };
    let probe = Probe::launch(&jdk, "FamilyProbe").expect("launch FamilyProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);
    probe.wait_for_line(EVENT_TIMEOUT, |l| l.contains("ready")).expect("probe never printed ready");

    // Exception stops, as a list of two classes the JVM has certainly loaded.
    let excs = server.call(
        "debug.set_exception_stop",
        serde_json::json!({"class_pattern": ["java.lang.RuntimeException", "java.lang.Exception"], "trace": true}),
    );
    assert_contains_all(
        "one exc_ per class, and the caveat that only loaded classes can match",
        &excs,
        &["2 pattern(s)", "2 exception stop(s) armed", "exc_", "LOADED NOW"],
    );

    // A wildcard here expands over LOADED exception classes rather than being handed to the JVM.
    let wild_exc = server.call(
        "debug.set_exception_stop",
        serde_json::json!({"class_pattern": ["java.lang.*Exception"], "trace": true, "max_classes": 3}),
    );
    assert_contains_all(
        "a wildcard expands, states how many classes it matched, and stops at the cap",
        &wild_exc,
        &["loaded class(es) matched", "exception stop(s) armed"],
    );

    // Field stops: `Family*` matches four classes and only FamilyProbe declares `gamma`. The other three
    // must be reported as not-targets, not as errors.
    let fields = server.call(
        "debug.set_field_stop",
        serde_json::json!({"class_name": "Family*", "field_name": "gamma", "trace": true}),
    );
    assert_contains_all(
        "the one class with the field is armed; the rest are notes",
        &fields,
        &["1 watchpoint(s) armed", "FamilyProbe.gamma", "has no field 'gamma'"],
    );
    assert!(!fields.contains("❌"), "a class without the field is not a failure: {fields}");

    // Method exits: a list of two patterns, one request each, no expansion — the JVM does the matching.
    let mexits = server.call(
        "debug.set_method_exit_stop",
        serde_json::json!({"class_pattern": ["FamilyAlpha", "FamilyBeta"], "method": "handle"}),
    );
    assert_contains_all(
        "one mexit_ per pattern",
        &mexits,
        &["2 pattern(s)", "2 method-exit request(s) armed", "mexit_"],
    );
    assert!(
        !mexits.contains("loaded class(es) matched"),
        "method-exit does not expand, so it must not claim to have counted matches: {mexits}"
    );

    // All of it is listed, and the traced ones actually fire.
    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_contains_all("every kind is listed", &listed, &["exception", "watchpoint(s)", "method-exit"]);
    assert!(
        server.wait_for_traces("FamilyAlpha", EVENT_TIMEOUT).is_some(),
        "a batched method-exit request must actually report returns.\n  stop points: {listed}"
    );

    server.panic_reset();
}

/// LAUNCH-1: `debug.launch` must start a JVM that is suspended BEFORE its first instruction, be identifiable
/// as ours, and be terminated by `debug.disconnect`.
///
/// The suspend claim is tested the only way that actually proves it: a breakpoint inside a **static
/// initialiser**. Attaching can never hit one — by the time a connection is possible the class has long since
/// initialised — so if this fires, the debugger genuinely got there first. It also demonstrates the state
/// itself: at the moment of launch the main class is not even LOADED, so the breakpoint has to defer.
///
/// Termination is proven against the OS rather than against our own reply text, because a reply saying
/// "terminated" is exactly what a leaked JVM would also say.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn launch_suspends_before_the_first_instruction_and_disconnect_terminates_it() {
    let Some(jdk) = jdk_or_skip("launch_suspends_before_the_first_instruction_and_disconnect_terminates_it")
    else {
        return;
    };
    let out = tempfile::tempdir().expect("tempdir for the compiled probe");
    jdk.compile_probe("StartupProbe", out.path()).expect("compile StartupProbe");
    let mut server = Server::start().expect("start server");

    // java_home is passed explicitly: the server must run the SAME JDK that compiled the class, or the
    // failure would be a class-file version mismatch rather than anything this test is about.
    let launched = server.call(
        "debug.launch",
        serde_json::json!({
            "main_class": "StartupProbe",
            "classpath": [out.path().to_string_lossy()],
            "java_home": jdk.home().to_string_lossy(),
        }),
    );
    assert_contains_all(
        "the launch reply states what it started and the three things only a launched JVM has",
        &launched,
        &["Launched StartupProbe", "SUSPENDED BEFORE ITS FIRST INSTRUCTION", "TERMINATES", "pid "],
    );
    let pid: u32 = launched
        .split("pid ")
        .nth(1)
        .and_then(|rest| rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok())
        .unwrap_or_else(|| panic!("no pid in the launch reply: {launched}"));

    assert_contains_all(
        "the session says whose JVM this is, and what it was started with",
        &server.call("debug.list_sessions", serde_json::json!({})),
        &["LAUNCHED by us", "dies with this session", "Launched with:", "StartupProbe"],
    );

    // Nothing has run, so the main class is not loaded yet — the breakpoint must defer rather than resolve.
    let line = probe_line(&probe_source("StartupProbe"), "// BP_CLINIT");
    let set = server
        .call("debug.set_line_stop", serde_json::json!({"class_pattern": "StartupProbe", "line": line}));
    assert!(
        set.contains("Deferred") || set.contains("not loaded"),
        "at suspend=y the main class cannot be loaded yet, so this must defer: {set}"
    );

    // Releasing the VM is what runs the static initialiser, and the deferred breakpoint has to arm in time
    // to catch it.
    server.call("debug.continue", serde_json::json!({}));
    let hit = server.wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT).unwrap_or_else(|| {
        panic!(
            "the breakpoint inside the static initialiser never fired — suspend=y did not hold the JVM \
             before class initialisation.\n  launch reply: {launched}\n  stop points: {}",
            server.call("debug.list_stop_points", serde_json::json!({}))
        )
    });
    assert_contains_all(
        "the hit is inside <clinit>, which attaching could never have reached",
        &hit,
        &["\"method\":\"<clinit>\"", "StartupProbe"],
    );

    let bye = server.call("debug.disconnect", serde_json::json!({}));
    assert_contains_all(
        "disconnect says it ended the JVM it started",
        &bye,
        &["TERMINATED", &pid.to_string()],
    );

    // The claim, checked against the OS. `/proc/<pid>` is Linux-only, which is where this suite runs; on
    // anything else the directory is absent and this passes without proving anything, so the assertion is
    // deliberately about the process being GONE rather than about the reply.
    let proc_entry = std::path::PathBuf::from(format!("/proc/{pid}"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while proc_entry.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        !proc_entry.exists(),
        "the JVM this server launched (pid {pid}) is still alive after debug.disconnect said it terminated \
         it — a launched JVM that outlives its session is the leak LAUNCH-1's triage was worried about"
    );
}

/// LAUNCH-1: a JVM that dies during startup must come back as the JVM's OWN words, not as a timeout.
///
/// This is the failure the launch path is most careful about, because a JVM that died and a JVM that is
/// merely slow are the same observation from the socket's side — a connect that has not succeeded yet — and
/// reporting "could not connect" after 30s would throw away the one thing that explains it.
///
/// **An unrecognised VM option, not a missing main class**, and the difference is worth recording: with
/// `suspend=y` the JVM is held *before* it resolves the main class, so a bogus `main_class` LAUNCHES FINE and
/// only dies on the first `debug.continue`. That surprised this test into existence, and it is why the launch
/// reply now says a successful launch is not evidence the program can run, and why `debug.list_sessions`
/// carries a dead launched JVM's last output. A bad VM option is rejected before the agent ever listens, which
/// is what makes it the deterministic case for the startup path.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_launch_that_dies_at_startup_reports_the_jvms_own_words() {
    let Some(jdk) = jdk_or_skip("a_launch_that_dies_at_startup_reports_the_jvms_own_words") else { return };
    let mut server = Server::start().expect("start server");

    let started = std::time::Instant::now();
    let failed = server.call(
        "debug.launch",
        serde_json::json!({
            "main_class": "Whatever",
            "jvm_args": ["-XX:+DefinitelyNotARealVmOption"],
            "java_home": jdk.home().to_string_lossy(),
        }),
    );
    assert_contains_all(
        "the reply is the JVM's own diagnosis, plus the command that produced it",
        &failed,
        &["exited before the debugger could attach", "DefinitelyNotARealVmOption", "Command:"],
    );
    // And it must not have waited out the connect timeout to say so: the child was polled alongside the port.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "a dead child should be noticed as soon as it exits, not after the 30s connect timeout (took {:?})",
        started.elapsed()
    );

    // A failed launch must leave no session behind — there is nothing to talk to.
    assert_contains_all(
        "no session was opened",
        &server.call("debug.list_sessions", serde_json::json!({})),
        &["No debug sessions"],
    );

    // Nor a usable java_home: naming one that is not a JDK is an error, never a silent fallback to another
    // JVM (the TEST-18 lesson, applied to the launch path).
    assert_contains_all(
        "an unusable java_home is refused by name",
        &server.call(
            "debug.launch",
            serde_json::json!({"main_class": "Whatever", "java_home": "/definitely/not/a/jdk"}),
        ),
        &["/definitely/not/a/jdk", "bin/java"],
    );
}

/// The first `prefix<digits>` id in a tool reply — enough to follow one up with `clear`/`toggle` without
/// pulling a regex crate into the test suite.
fn find_id(reply: &str, prefix: &str) -> Option<String> {
    let at = reply.find(prefix)?;
    let rest = &reply[at + prefix.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    Some(format!("{prefix}{digits}"))
}

/// TEST-1: `force_return` must change what the CALLER receives, not merely report success. Proven by
/// reading the probe's own stdout.
///
/// **Why the failure text is so loud, and why nothing else here changed (TEST-16, #45).** This test was
/// reported failing about 1 run in 24 with the output never captured, so the first job was to reproduce
/// it. It would not. 25 full-suite runs and 1,166 solo runs — 3,573 `force_return` cycles — on JDK 11 and
/// JDK 25, at 32 and 64 synthetic CPU hogs on a 16-core box, and with two whole suites racing each other
/// for JVMs, produced not one failure of this test. That is the load that reproduces TEST-19 (#54) at
/// will, and the same runs did knock over two *other* tests, so the sampling was not too gentle — it was
/// aimed at the wrong property. Nothing was tuned, because tuning a test nobody has watched fail is how
/// a test stops being able to fail at all.
///
/// What was worth doing instead is making the next occurrence pay for itself, and the three ways this
/// goes red each used to throw away the one fact that decides between their causes:
///
///   - *"never fired"* said nothing about whether the breakpoint had **armed** — an armed-but-unhit
///     breakpoint, one still `⏳ Deferred` because `ForceProbe` had not loaded, and a probe that is not
///     running at all are three different bugs with one message. It now prints the arm reply, the stop
///     point listing, the newest event and the probe's tail.
///   - *"caller never observed the forced value"* reads as an accusation against `force_return`, but a VM
///     that was never resumed is silent in exactly the same way. The probe's line count is now taken
///     across the resume, so the message says which of the two happened and names the panic's own reply
///     (`resume_and_verify` reports a suspend depth it could not clear, and that report used to be
///     discarded on the floor).
///   - the control assertion now quotes the line that broke it.
///
/// If it fires in CI, that output is the bug report this issue never got.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn force_return_changes_what_the_caller_receives() {
    let Some(jdk) = jdk_or_skip("force_return_changes_what_the_caller_receives") else { return };
    let probe = Probe::launch(&jdk, "ForceProbe").expect("launch ForceProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let source = probe_source("ForceProbe");

    // (marker, forced value, the line main() prints only if the caller got the forced value)
    let cases = [
        ("// BP1", "true", "check=true"),
        ("// BP2", "\"forced\"", "name=forced"),
        ("// BP3", "99", "count=99"),
    ];

    for (marker, forced, expected_output) in cases {
        let line = probe_line(&source, marker);
        server.panic_reset();
        // The probe never prints this by itself, so anything found before we force is a bug in the
        // test, not a pass. Name the line that broke the control: the useful question then is whether
        // the probe changed or whether an earlier case leaked into this one.
        let leaked = probe.output().into_iter().find(|l| l.contains(expected_output));
        assert!(
            leaked.is_none(),
            "probe printed {expected_output} before force_return — the probe is not a valid control \
             (the offending line was {leaked:?})"
        );

        let armed = server
            .call("debug.set_line_stop", serde_json::json!({"class_pattern": "ForceProbe", "line": line}));
        assert!(
            server.wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT).is_some(),
            "breakpoint at ForceProbe:{line} ({marker}) never fired.\n  \
             set_line_stop said: {armed}\n  \
             stop points now: {}\n  \
             newest event: {}\n  \
             probe's last 8 lines (empty or frozen means it is not running): {:?}",
            server.call("debug.list_stop_points", serde_json::json!({})),
            server.last_event(),
            probe.output().iter().rev().take(8).collect::<Vec<_>>(),
        );

        let forced_reply = server.call("debug.force_return", serde_json::json!({"value": forced}));
        assert!(
            !forced_reply.contains("Failed") && !forced_reply.contains("error"),
            "force_return({forced}) was rejected: {forced_reply}"
        );

        // Clear first: leaving the breakpoint armed would re-suspend on the next iteration before
        // main() gets to print, and the test would time out waiting for its own effect.
        //
        // Count the probe's lines across the resume. Absence of `expected_output` alone cannot tell
        // "force_return reported success and lied" from "we never let the debuggee run again", and
        // those are a debugger bug and a harness bug respectively — TEST-19 (#54)'s lesson that a
        // panic message which cannot separate its own two causes sends the next reader the wrong way.
        let printed_before = probe.output().len();
        let resumed = server.panic_reset();
        let seen = probe.wait_for_line(EVENT_TIMEOUT, |l| l.contains(expected_output));
        let printed_after = probe.output().len();
        assert!(
            seen.is_some(),
            "caller never observed the forced value: expected a line containing {expected_output}, \
             force_return said: {forced_reply}\n  \
             the panic that had to resume the VM said: {resumed}\n  \
             the probe printed {delta} line(s) in the {EVENT_TIMEOUT:?} after that resume, so {verdict}\n  \
             recent output: {:?}",
            probe.output().iter().rev().take(8).collect::<Vec<_>>(),
            delta = printed_after - printed_before,
            verdict = resume_verdict(printed_before, printed_after),
        );
    }

    server.panic_reset();
}

/// OBJ-1: recursive expansion — nested objects, collections, cycles, and the bounds.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn deep_expansion_walks_objects_collections_and_survives_cycles() {
    let Some(jdk) = jdk_or_skip("deep_expansion_walks_objects_collections_and_survives_cycles") else {
        return;
    };
    let probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let line = probe_line(&probe_source("DeepProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in DeepProbe.inspect never fired");

    // Shallow stays shallow: the default must not start invoking methods or walking graphs.
    let shallow = server.evaluate("order");
    assert!(
        !shallow.contains("status") && !shallow.contains('\n'),
        "default evaluate must stay a one-liner, got: {shallow}"
    );

    // Order has more fields than the default child limit of 16, and inherited fields come last, so
    // ask for enough to see them all — the truncation behaviour itself is asserted separately below.
    let deep = |server: &mut Server, expr: &str, depth: usize| {
        server.call(
            "debug.evaluate",
            serde_json::json!({
                "expression": expr, "expand_objects": true, "max_depth": depth, "max_children": 30
            }),
        )
    };

    // --- Fields, including an inherited one, and nesting to the requested depth ---
    let d3 = deep(&mut server, "order", 3);
    assert_contains_all(
        "primitive and String fields expand",
        &d3,
        &["id = (int) 42", "status = \"OPEN\"", "total = (double) 19.5", "paid = (boolean) false"],
    );
    assert_contains_all("inherited field is included", &d3, &["recordId = (int) 7"]);
    assert_contains_all(
        "nesting reaches a grandchild object",
        &d3,
        &["customer = ", "name = \"Ana\"", "address = ", "city = \"Lisbon\""],
    );

    // --- Cycles must be reported, not recursed ---
    assert!(d3.contains("cycle"), "expected a cycle marker for customer.self / lastOrder in:\n{d3}");

    // --- Collections, element-level ---
    assert_contains_all("List elements", &d3, &["tags = ", "[0] = \"urgent\"", "[1] = \"fragile\""]);
    assert_contains_all("Map entries render as key → value", &d3, &["counts = ", "\"a\" → (int) 1"]);
    assert_contains_all("Set elements", &d3, &["labels = ", "\"x\""]);
    assert_contains_all(
        "Optional present and empty",
        &d3,
        &["note = Optional[\"gift\"]", "missing = Optional.empty"],
    );
    assert_contains_all("empty List", &d3, &["empty = "]);

    // --- Arrays take a different path from collections ---
    assert_contains_all(
        "primitive and object arrays",
        &d3,
        &["numbers = int[3]", "(int) 1", "words = java.lang.String[2]", "\"alpha\""],
    );

    // --- Breadth bound: fields truncate and say so, not just elements ---
    let few_fields = server.call(
        "debug.evaluate",
        serde_json::json!({"expression": "order", "expand_objects": true, "max_depth": 1, "max_children": 3}),
    );
    assert_contains_all("field truncation is reported", &few_fields, &["id = (int) 42", "more field(s)"]);
    assert!(!few_fields.contains("recordId"), "max_children=3 must not reach the 17th field:\n{few_fields}");

    // --- Breadth bound ---
    let capped = server.call(
        "debug.evaluate",
        serde_json::json!({"expression": "order.many", "expand_objects": true, "max_depth": 2, "max_children": 5}),
    );
    assert_contains_all("child limit truncates and says so", &capped, &["[0] = ", "… +15 more"]);
    assert!(!capped.contains("[5] ="), "max_children=5 must not render a 6th element:\n{capped}");

    // --- Depth bound: depth 1 expands `order`'s own fields but must not expand `customer`'s ---
    let d1 = deep(&mut server, "order", 1);
    assert_contains_all("depth 1 still shows own fields", &d1, &["id = (int) 42", "customer = "]);
    assert!(!d1.contains("city = "), "max_depth=1 must not reach order.customer.address.city:\n{d1}");

    // --- get_stack expansion is the same machinery on a frame's locals ---
    let stack = server.call(
        "debug.get_stack",
        serde_json::json!({"expand_objects": true, "max_depth": 2, "max_frames": 1, "package_filter": "DeepProbe"}),
    );
    assert_contains_all("get_stack expands locals", &stack, &["order = ", "id = (int) 42", "name = \"Ana\""]);

    // A deep render must never leave the VM wedged: the probe keeps printing after we resume.
    server.panic_reset();
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| l.contains("inspect ")).is_some(),
        "probe stopped running after deep expansion — an invocation likely left a thread suspended"
    );
}

/// OBJ-2: collection subscripts — indexing, slicing, and predicate filters.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
// Index, slice and filter over the same live collections: the assertions are a comparison, and a
// comparison cannot be split across test functions. Crossed 100 lines when the rustfmt adoption
// (#44) split its over-110 lines.
#[allow(clippy::too_many_lines)]
fn collection_subscripts_index_slice_and_filter() {
    let Some(jdk) = jdk_or_skip("collection_subscripts_index_slice_and_filter") else { return };
    let probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let line = probe_line(&probe_source("DeepProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in DeepProbe.inspect never fired");

    // --- [i]: index narrows to one value, so it keeps chaining ---
    assert_contains_all("List index", &server.evaluate("order.lines[0]"), &["Line(aa,1,true)"]);
    assert_contains_all("index then field", &server.evaluate("order.lines[1].sku"), &["\"bb\""]);
    assert_contains_all("index then method", &server.evaluate("order.lines[3].getQty()"), &["(int) 9"]);
    assert_contains_all("array index", &server.evaluate("order.numbers[2]"), &["(int) 3"]);
    assert_contains_all("String array index", &server.evaluate("order.words[0]"), &["\"alpha\""]);
    // A Map subscript is a key lookup, not a position — and an int key must be boxed for get(Object).
    assert_contains_all("Map string key", &server.evaluate("order.counts[\"b\"]"), &["(int) 2"]);
    assert_contains_all(
        "Optional via index is refused clearly",
        &server.evaluate("order.note[0]"),
        &["not indexable"],
    );

    // Bounds and type errors say which is which.
    assert_contains_all("array out of bounds", &server.evaluate("order.numbers[9]"), &["out of bounds"]);
    assert_contains_all(
        "non-int list index",
        &server.evaluate("order.lines[\"x\"]"),
        &["list index must be an int"],
    );

    // --- [a..b]: half-open slice ---
    let sliced = server.evaluate("order.lines[1..3]");
    assert_contains_all(
        "slice reports selection and count",
        &sliced,
        &["2 of 5", "[0] = ", "Line(bb,5,false)", "Line(cc,2,true)"],
    );
    assert!(!sliced.contains("Line(aa"), "slice [1..3] must exclude element 0:\n{sliced}");
    assert!(!sliced.contains("Line(dd"), "slice [1..3] must be half-open:\n{sliced}");
    // An over-long range clamps rather than erroring — asking for "up to 100" is normal.
    assert_contains_all("over-long range clamps", &server.evaluate("order.lines[0..100]"), &["5 of 5"]);
    assert_contains_all("empty range", &server.evaluate("order.lines[2..2]"), &["0 of 5"]);
    assert_contains_all("array slice", &server.evaluate("order.numbers[0..2]"), &["2 of 3", "(int) 1"]);
    assert_contains_all(
        "reversed range is rejected",
        &server.evaluate("order.lines[3..1]"),
        &["ends before it starts"],
    );

    // --- [?predicate]: left side resolves against each element ---
    let paid = server.evaluate("order.lines[?paid == true]");
    assert_contains_all(
        "boolean field predicate",
        &paid,
        &["2 of 5 matched", "Line(aa,1,true)", "Line(cc,2,true)"],
    );
    assert!(!paid.contains("Line(bb"), "unpaid lines must not match:\n{paid}");

    // qty > 3 matches bb(5), dd(9) AND ee(4) — three, not two.
    assert_contains_all(
        "numeric comparison predicate",
        &server.evaluate("order.lines[?qty > 3]"),
        &["3 of 5 matched", "Line(bb,5,false)", "Line(dd,9,false)", "Line(ee,4,false)"],
    );
    assert_contains_all(
        "method-call predicate",
        &server.evaluate("order.lines[?getQty() == 2]"),
        &["1 of 5 matched", "Line(cc,2,true)"],
    );
    assert_contains_all(
        "String predicate",
        &server.evaluate("order.lines[?sku == \"ee\"]"),
        &["1 of 5 matched", "Line(ee,4,false)"],
    );
    // The right-hand side may be an ordinary expression, so a predicate can reference the frame.
    // threshold is 3, so this must agree with the literal `qty > 3` above.
    assert_contains_all(
        "predicate right side reads an enclosing field",
        &server.evaluate("order.lines[?qty > order.threshold]"),
        &["3 of 5 matched"],
    );
    // A match-nothing predicate is a real answer, and must not look like a broken one.
    assert_contains_all(
        "no matches still reports the scan",
        &server.evaluate("order.lines[?qty > 999]"),
        &["0 of 5 matched"],
    );
    // A predicate that can't resolve on any element is an error, not "0 matched".
    assert_contains_all(
        "broken predicate is an error, not an empty result",
        &server.evaluate("order.lines[?nosuchfield == 1]"),
        &["failed on every element"],
    );
    assert_contains_all(
        "string filter on a String list",
        &server.evaluate("order.tags[?length() == 7]"),
        &["1 of 2 matched"],
    );

    // --- Multi-value results end the expression, explicitly ---
    assert_contains_all(
        "chaining after a filter is refused with a reason",
        &server.evaluate("order.lines[?paid == true].sku"),
        &["selects several values"],
    );
    // A subscript in a write target used to be parsed and then dropped, writing the whole field. An
    // *indexed* write is now supported (OBJ-4, see `subscript_writes_and_map_entry_filters`); a slice or
    // filter target still has nothing single to write, and says so.
    assert_contains_all(
        "a multi-value set_value target is refused, not silently widened",
        &server.call("debug.set_value", serde_json::json!({"target": "order.lines[0..2]", "value": "1"})),
        &["selects several elements"],
    );
    // A Map has no positional order, so slicing one points at the alternatives.
    assert_contains_all(
        "slicing a Map explains itself",
        &server.evaluate("order.counts[0..1]"),
        &["no order to slice"],
    );

    // --- Subscripts compose with deep expansion ---
    let deep_filter = server.call(
        "debug.evaluate",
        serde_json::json!({"expression": "order.lines[?paid == true]", "expand_objects": true, "max_depth": 2}),
    );
    assert_contains_all(
        "filter results expand",
        &deep_filter,
        &["2 of 5 matched", "sku = \"aa\"", "qty = (int) 1", "paid = (boolean) true"],
    );

    // Filters invoke methods in the debuggee; the VM must still be usable afterwards.
    server.panic_reset();
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| l.contains("inspect ")).is_some(),
        "probe stopped running after subscript evaluation"
    );
}

/// The original roadmap's success criteria (`docs/VARIABLE_INSPECTION_PLAN.md`, appendix items 10/14),
/// checked one by one against a stand-in for Spring Boot + Micrometer.
///
/// The criteria were written against a `HelloController` with a Micrometer `meterRegistry`. Spring
/// can't be a test dependency here, so `MetricsProbe` reproduces the object *shape* — a registry
/// holding a `Map<String, Counter>`, a `Counter` with a nested `id.name`/`id.tags`, and a real
/// `AtomicInteger`. That verifies the tool against the real structure; what it cannot verify is
/// Spring's own class names and bean lifecycle.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn roadmap_metrics_inspection_criteria() {
    let Some(jdk) = jdk_or_skip("roadmap_metrics_inspection_criteria") else { return };
    let probe = Probe::launch(&jdk, "MetricsProbe").expect("launch MetricsProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let line = probe_line(&probe_source("MetricsProbe"), "// BP1");
    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "MetricsProbe$HelloController", "line": line}),
    );
    let hit = server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in HelloController.hello never fired");

    // "Set a breakpoint in HelloController" + the Week-1 BLOCKER: know WHICH thread hit it, rather
    // than guessing among dozens.
    assert_contains_all(
        "the hit names its thread and location",
        &hit,
        &["\"method\":\"hello\"", "\"thread\":\"0x"],
    );

    // "See that meterRegistry is a SimpleMeterRegistry"
    assert_contains_all(
        "meterRegistry's concrete type",
        &server.evaluate("this.meterRegistry"),
        &["SimpleMeterRegistry"],
    );

    // "See fields of meterRegistry (including the meters collection)"
    let registry = server.call(
        "debug.evaluate",
        serde_json::json!({"expression": "this.meterRegistry", "expand_objects": true, "max_depth": 2}),
    );
    assert_contains_all(
        "meterRegistry fields, with the meters map expanded",
        &registry,
        &["meters = ", "3 entries", "\"hello_requests_total\"", "\"http.server.requests\""],
    );

    // "See that helloCounter exists with count=42.0"
    assert_contains_all(
        "helloCounter's count",
        &server.evaluate("this.helloCounter.count"),
        &["(double) 42"],
    );

    // "See string values directly (not object IDs)" — and the Week-4 headline, field-path navigation.
    assert_contains_all(
        "field path through a nested object to a String",
        &server.evaluate("this.helloCounter.id.name"),
        &["\"hello_requests_total\""],
    );
    assert_contains_all(
        "and into a collection on that nested object",
        &server.evaluate("this.helloCounter.id.tags[0]"),
        &["uri=/hello"],
    );

    // Stretch goal: "show me this.meterRegistry.meters and get the map contents".
    let meters = server.call(
        "debug.evaluate",
        serde_json::json!({"expression": "this.meterRegistry.meters", "expand_objects": true, "max_depth": 3}),
    );
    assert_contains_all(
        "the map renders as key → value with drillable entries",
        &meters,
        &["→", "hello_requests_total", "count = (double)"],
    );

    // Stretch goal: "find metrics with name containing 'hello'" — the filter, keyed on the element.
    let hello_only =
        server.evaluate("this.meterRegistry.meters.values()[?id.name != \"http.server.requests\"]");
    assert_contains_all(
        "filter the registry down to the hello meters",
        &hello_only,
        &["2 of 3 matched", "hello_requests_total", "hello_errors_total"],
    );

    // A real JDK library object, and a boxed-style wrapper: AtomicInteger holds an int `value`.
    assert_contains_all(
        "a library object expands too",
        &server.call(
            "debug.evaluate",
            serde_json::json!({"expression": "this.requestCount", "expand_objects": true, "max_depth": 1}),
        ),
        &["AtomicInteger", "value = (int)"],
    );

    server.panic_reset();
}

/// TRACE-2: an exception breakpoint in trace mode records each throw and leaves nothing suspended.
///
/// The suspension check is the load-bearing assertion, and it can only be made against the
/// *debuggee's* own output: the debugger reports success either way, so "no complaint" proves nothing.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn traced_exception_breakpoints_record_throws_without_suspending() {
    let Some(jdk) = jdk_or_skip("traced_exception_breakpoints_record_throws_without_suspending") else {
        return;
    };
    let probe = Probe::launch(&jdk, "ExcProbe").expect("launch ExcProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // An exception request needs a concrete ref type, so the class must already be loaded — one tick
    // means integrate() has thrown at least once, which is what loads it.
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let base = highest_tick(&probe).expect("no tick to count from");

    let set = server.call(
        "debug.set_exception_stop",
        serde_json::json!({
            "class_pattern": "ExcProbe$SwallowedException", "trace": true, "trace_expr": "i",
        }),
    );
    assert_contains_all("traced exception breakpoint is armed", &set, &["exc_", "trace (non-suspending)"]);

    // The whole point: the probe keeps printing. A suspending exception breakpoint stops the ticks
    // dead on the first throw — and every throw here is caught, so this would freeze on a code path
    // the application itself considers a non-event.
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2))
            .is_some(),
        "probe stopped ticking after a traced exception breakpoint — a throw left it suspended\n  output: {:?}",
        probe.output(),
    );

    let traces = server.call("debug.get_traces", serde_json::json!({}));
    assert_contains_all(
        "each throw is recorded with its exception detail",
        &traces,
        &[
            "exc_",
            "ExcProbe.integrate",
            "exception=ExcProbe$SwallowedException",
            "caught=true",
            "caught_at=ExcProbe.integrate",
            // EXC-2 (#67): the message travels with the type. Read off `Throwable.detailMessage` as a
            // field, which is the only mechanism available here — trace mode resumes the hit thread
            // immediately, so `getMessage()` could never have been called.
            "message=integration failed on ",
        ],
    );
    // Locals and the trace expression are captured in the throwing frame, exactly as for a logpoint.
    assert_contains_all("locals and trace expr", &traces, &["{i=(int) ", "i =>"]);

    // A traced hit must never surface as an event — that is what would suggest a suspended VM.
    let ev = server.last_event();
    assert!(
        !ev.contains("\"event\":\"exception\""),
        "a traced throw must not be reported as a suspending event: {ev}"
    );

    // Trace mode is visible in the bookkeeping, so a later reader can tell why nothing is frozen.
    assert_contains_all(
        "listed as traced",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &["exception ExcProbe$SwallowedException", "(trace)"],
    );

    server.panic_reset();
}

/// EXC-2 (#67): an exception snapshot carries the message the JVM already computed, and says nothing
/// when there is none.
///
/// The two halves are one test on purpose, because each is what makes the other readable. A `message=`
/// that appeared unconditionally could be an empty string dressed up as an answer; an absent one proves
/// only that nothing was read unless a sibling throw in the same run shows a message arriving.
///
/// **The NPE half is deliberately version-gated rather than version-locked.** JEP 358's helpful message
/// is on by default from JDK 15, so on CI's JDK 11 leg the very same throw carries no message at all —
/// which is the JVM behaving correctly, not the flake it would otherwise be diagnosed as (#36).
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn an_exception_snapshot_carries_the_jvms_own_message() {
    let Some(jdk) = jdk_or_skip("an_exception_snapshot_carries_the_jvms_own_message") else {
        return;
    };
    let probe = Probe::launch(&jdk, "ExcMsgProbe").expect("launch ExcMsgProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // An exception request needs a concrete ref type, so both throwing paths must have run once.
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let base = highest_tick(&probe).expect("no tick to count from");

    // Traced, not suspending: reading the message off a field is the *only* mechanism available on this
    // path, so proving it here proves the constraint #67 was filed under.
    for pattern in ["java.lang.NullPointerException", "ExcMsgProbe$Bare"] {
        let set = server
            .call("debug.set_exception_stop", serde_json::json!({"class_pattern": pattern, "trace": true}));
        assert_contains_all(&format!("{pattern} traced"), &set, &["exc_", "trace (non-suspending)"]);
    }

    // Both throws are caught, so the probe must keep ticking — the usual proof nothing is frozen.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2)).is_some(),
        "probe stopped ticking after two traced exception stops\n  output: {:?}",
        probe.output(),
    );

    let traces = server.call("debug.get_traces", serde_json::json!({}));

    // The messageless throw: `exception=` present, `message=` absent. Asserted on the Bare line rather
    // than on the whole reply, because the NPE line in the same reply may legitimately carry one.
    let bare_line = traces
        .lines()
        .find(|l| l.contains("exception=ExcMsgProbe$Bare"))
        .unwrap_or_else(|| panic!("no snapshot for the messageless throw\n  got: {traces}"));
    assert!(
        !bare_line.contains("message="),
        "an exception with no message must omit the key rather than report an empty one — a caller \
         cannot tell those apart, and one of them is a lie: {bare_line}"
    );

    let npe_line = traces
        .lines()
        .find(|l| l.contains("exception=java.lang.NullPointerException"))
        .unwrap_or_else(|| panic!("no snapshot for the NPE\n  got: {traces}"));
    match jdk.feature_version() {
        Some(v) if v >= 15 => assert_contains_all(
            "a JDK 15+ helpful NPE names the failing subexpression, which is the diagnosis itself",
            npe_line,
            &["message=", "getCount()", "is null"],
        ),
        // Pre-15 the JVM computes nothing, so the correct snapshot is the same one the Bare throw gets.
        Some(_) => assert!(
            !npe_line.contains("message="),
            "before JDK 15 the JVM computes no NPE message, so reporting one means it came from \
             somewhere else: {npe_line}"
        ),
        // An unparseable version gates nothing: the type and catch site are version-independent.
        None => assert_contains_all("NPE snapshot", npe_line, &["caught=true", "ExcMsgProbe.helpfulNpe"]),
    }

    server.panic_reset();
}

/// EXC-3 (#68): a rethrown instance keeps both ends of its chain, collapses the middle, and does not
/// spend the trace budget on the plumbing.
///
/// `trace_max_hits: 3` is the whole experiment. `RethrowProbe` throws once and rethrows three times per
/// iteration, so before the fix a budget of 3 was gone inside the *first* iteration — spent on
/// `origin`, `pooled`, `tx`, disarming before the exception even escaped. Every assertion below is a
/// different way of saying the budget now counts failures rather than layers.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_rethrow_chain_collapses_instead_of_spending_the_trace_budget() {
    let Some(jdk) = jdk_or_skip("a_rethrow_chain_collapses_instead_of_spending_the_trace_budget") else {
        return;
    };
    let probe = Probe::launch(&jdk, "RethrowProbe").expect("launch RethrowProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // The exception request needs a concrete ref type, so one full throw/rethrow cycle must have run.
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let base = highest_tick(&probe).expect("no tick to count from");

    let set = server.call(
        "debug.set_exception_stop",
        serde_json::json!({
            "class_pattern": "RethrowProbe$LayerException", "trace": true, "trace_max_hits": 3,
        }),
    );
    assert_contains_all("armed with a budget of 3", &set, &["exc_", "trace (non-suspending)"]);

    // Wait for the budget to run out. Three charged hits means three *iterations*, so the probe has to
    // get that far — and it can only tick if nothing was left suspended.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 4)).is_some(),
        "probe stopped ticking during a traced rethrow chain\n  output: {:?}",
        probe.output(),
    );

    let traces = server.call("debug.get_traces", serde_json::json!({}));
    // Count by the snapshot's own HIT location — `#3 [exc_1] Class.method:line …` — never by a substring
    // of the whole line. `trace_frames` puts `← RethrowProbe.pooled:40` in every origin record's caller
    // chain, so a `contains` here would count the collapsed layers it is supposed to prove absent.
    let at = |m: &str| {
        let want = format!("RethrowProbe.{m}:");
        traces
            .lines()
            .filter(|l| l.starts_with('#'))
            .filter_map(|l| l.split_whitespace().nth(2))
            .filter(|hit| hit.starts_with(&want))
            .count()
    };

    // The budget bound, so this test is not passing merely because nothing ran out.
    assert!(
        traces.contains("trace-hit budget"),
        "the stop point never hit its budget, so this proves nothing about what the budget was spent \
         on:\n{traces}"
    );

    // THE assertion: three charged hits bought three distinct failures, not one failure's four layers.
    assert_eq!(
        at("origin"),
        3,
        "a budget of 3 should capture 3 separate throws; before EXC-3 the first instance's rethrows ate \
         it and only one `origin` was ever recorded:\n{traces}"
    );

    // Both ends survive: the original throw above, and the point where it escaped.
    assert!(
        at("security") > 0,
        "the escaping end of the chain is missing — that is the record saying where the exception left, \
         and a chain trimmed to its start would never show it:\n{traces}"
    );

    // The middle is a count, not a pile of records. `pooled` and `tx` are pure plumbing here.
    assert_eq!(
        (at("pooled"), at("tx")),
        (0, 0),
        "the middle of the chain was kept as records instead of being collapsed:\n{traces}"
    );
    assert_contains_all(
        "a collapsed chain says how much it folded, and points at the original throw",
        &traces,
        &["↻ rethrow of #", "more rethrow(s) collapsed"],
    );

    server.panic_reset();
}

/// TRACE-8 (#72): a traced stop point that disarms on its budget must not freeze the VM with the hits it
/// already generated.
///
/// Found while implementing EXC-3 (#68), and it is the worst failure shape this crate has: trace mode's
/// single promise is that it never suspends anything, and this broke it *at the moment the budget ran
/// out* — the point where a caller has stopped watching. `try_record_trace` recognises a traced hit by
/// looking its request id up among the enabled requests, so a disarm made every in-flight hit
/// unrecognisable, and each one fell through to the suspending path and stayed there.
///
/// It takes a **rethrow** to see, which is why it went unnoticed: with `trace_max_hits: 1` a fresh-throw
/// probe disarms and the next throw is a whole iteration away, by which time the request is simply gone.
/// `RethrowProbe` has three more throws of the same instance already unwinding.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_traced_stop_point_that_disarms_mid_chain_leaves_nothing_suspended() {
    let Some(jdk) = jdk_or_skip("a_traced_stop_point_that_disarms_mid_chain_leaves_nothing_suspended") else {
        return;
    };
    let probe = Probe::launch(&jdk, "RethrowProbe").expect("launch RethrowProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let base = highest_tick(&probe).expect("no tick to count from");

    // A budget of 1 disarms on the very first throw, with three rethrows of that instance still to come.
    server.call(
        "debug.set_exception_stop",
        serde_json::json!({
            "class_pattern": "RethrowProbe$LayerException", "trace": true, "trace_max_hits": 1,
        }),
    );

    // The debuggee's own output is the only thing that can prove this: the debugger reports success
    // whether or not it left a thread frozen.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 3)).is_some(),
        "the probe stopped ticking after a traced exception stop disarmed itself mid-chain — an in-flight \
         traced hit was surfaced as a suspending event and the thread was never resumed\n  output: {:?}",
        probe.output(),
    );

    // The other half of the same bug: a traced hit must never become an event, disarmed or not.
    let ev = server.last_event();
    assert!(
        !ev.contains("\"event\":\"exception\""),
        "an in-flight traced throw was surfaced as a suspending event after the disarm: {ev}"
    );

    server.panic_reset();
}

/// TRACE-2: a watchpoint in trace mode records the mutating location and the old → new pair without
/// suspending — "who mutates this?" answered on a JVM you are not allowed to freeze.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn traced_watchpoints_record_writes_without_suspending() {
    let Some(jdk) = jdk_or_skip("traced_watchpoints_record_writes_without_suspending") else { return };
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    // `probe.attach(&mut server)` rather than `server.attach(probe.port)`: this is one of the two tests
    // that has been seen failing at attach with `Connection refused` (TEST-21, #56), and the difference
    // is what the failure says — the probe's own log and whether anything is listening on the port.
    probe.attach(&mut server);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let base = highest_tick(&probe).expect("no tick to count from");

    let set = server.call(
        "debug.set_field_stop",
        serde_json::json!({"class_name": "WatchProbe", "field_name": "counter", "trace": true}),
    );
    assert_contains_all("traced watchpoint is armed", &set, &["watch_modify_", "trace (non-suspending)"]);

    // WatchProbe's tick number IS `counter`, so a rising tick proves both that the probe is running
    // and that the writes the watchpoint is reporting are really committing.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2)).is_some(),
        "probe stopped ticking after a traced watchpoint — a write left it suspended\n  output: {:?}",
        probe.output(),
    );

    let traces = server.call("debug.get_traces", serde_json::json!({}));
    assert_contains_all(
        "each write is recorded with its field detail",
        &traces,
        &["watch_modify_", "WatchProbe.bumpCounter", "field=WatchProbe.counter", "static=true"],
    );
    // The old → new pair is the whole value of the hit, and it is only readable while the pending
    // store has not committed — i.e. it has to be captured at trace time, not at read time.
    let hit = traces
        .lines()
        .rev()
        .find(|l| l.contains("field=WatchProbe.counter"))
        .unwrap_or_else(|| panic!("no counter trace line in:\n{traces}"));
    let (old, new) = (
        trace_int(hit, "old").unwrap_or_else(|| panic!("no old= in: {hit}")),
        trace_int(hit, "new").unwrap_or_else(|| panic!("no new= in: {hit}")),
    );
    assert_eq!(new, old + 1, "bumpCounter does counter+1, so the pair must be one apart: {hit}");

    let ev = server.last_event();
    assert!(
        !ev.contains("\"event\":\"field_modification\""),
        "a traced write must not be reported as a suspending event: {ev}"
    );
    assert_contains_all(
        "listed as traced",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &["watch WatchProbe.counter", "(trace)"],
    );

    server.panic_reset();
}

/// TRACE-5: a traced hit records the calling chain, so a logpoint can say WHICH path reached it.
///
/// `CallerProbe.record` is reached from three different paths, and the assertion pairs each hit with
/// its own chain via the `v` argument. That pairing is the whole test: a snapshot that captured one
/// frame and reported a hardcoded or last-seen caller would satisfy any single-caller probe.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn traced_hits_record_which_caller_reached_them() {
    let Some(jdk) = jdk_or_skip("traced_hits_record_which_caller_reached_them") else { return };
    let probe = Probe::launch(&jdk, "CallerProbe").expect("launch CallerProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let base = highest_tick(&probe).expect("no tick to count from");

    let src = probe_source("CallerProbe");
    let line = probe_line(&src, "// BP1");
    // Depth 3 is DELIBERATELY deeper than the shallow paths reach: `record` under `alpha` sits on a
    // 3-frame stack, so only 2 callers exist. JDWP fails a `Frames` request whose length exceeds the
    // frames a thread has, and asking for the exact count lost the whole snapshot — locals included —
    // on exactly those hits. A depth that every path could satisfy would not have caught it.
    let set = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "CallerProbe", "line": line, "trace": true, "trace_frames": 3,
        }),
    );
    assert_contains_all("traced logpoint with caller frames is armed", &set, &["bp_", "Caller frames: 3"]);

    // The TRACE-2 discipline: reading caller frames must not leave the hit thread suspended. Only the
    // probe's own output can prove that — the debugger reports success either way.
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2))
            .is_some(),
        "probe stopped ticking after a traced logpoint with caller frames — a hit left it suspended\n  output: {:?}",
        probe.output(),
    );

    // Wait for the deepest path (record(3), reached via nested) so all three are certainly recorded.
    let traces = server
        .wait_for_traces("CallerProbe.nested:", EVENT_TIMEOUT)
        .expect("no traced hit ever reported a caller chain");

    // Each of the three paths must report ITS OWN caller chain, matched by the `v` it was called with.
    // record(1) came via alpha, record(2) directly via beta, record(3) via nested (itself under beta).
    // `depth` is how many callers that path really has, which for the first two is LESS than the 3
    // requested — the shallow-stack case.
    for (v, want_chain, depth) in [
        (1, &["CallerProbe.alpha:", "CallerProbe.main:"][..], 2),
        (2, &["CallerProbe.beta:", "CallerProbe.main:"][..], 2),
        (3, &["CallerProbe.nested:", "CallerProbe.beta:", "CallerProbe.main:"][..], 3),
    ] {
        let hit = traces
            .lines()
            .find(|l| trace_int(l, "v") == Some(v))
            .unwrap_or_else(|| panic!("no trace line for record({v}) in:\n{traces}"));
        assert!(hit.contains("CallerProbe.record:"), "the hit frame is still record() itself: {hit}");
        for want in want_chain {
            assert!(hit.contains(want), "record({v}) should report `{want}` in its chain: {hit}");
        }
        // Exactly the callers that exist — not the requested 3 padded out, and not a truncation.
        assert_eq!(hit.matches(" ← ").count(), depth, "record({v}) has {depth} caller(s) above it: {hit}");
        // The locals must survive alongside the chain. Losing them was the real cost of the
        // over-long `Frames` request, and it was silent: the line still looked like a valid hit.
        assert!(
            hit.contains(&format!("{{v=(int) {v}}}")),
            "record({v}) must still capture the hit frame's locals: {hit}"
        );
        // Caller frames carry LOCATIONS ONLY — one `{…}` group on the line, the hit frame's. A caller
        // rendered with its variable table would add another.
        assert_eq!(
            hit.matches('{').count(),
            1,
            "only the hit frame's locals should be captured, not each caller's: {hit}"
        );
    }

    // The depth is visible in the bookkeeping, so a slowed-down debuggee is explainable from a listing.
    assert_contains_all(
        "caller depth is listed",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &["(trace)", "[+3 caller frame(s)]"],
    );

    server.panic_reset();
}

/// TRACE-5: `trace_frames: 0` is the pre-TRACE-5 one-frame snapshot, and the cap is enforced and
/// reported rather than silently applied.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn trace_frames_zero_keeps_the_one_frame_snapshot_and_the_cap_is_reported() {
    let Some(jdk) = jdk_or_skip("trace_frames_zero_keeps_the_one_frame_snapshot_and_the_cap_is_reported")
    else {
        return;
    };
    let probe = Probe::launch(&jdk, "CallerProbe").expect("launch CallerProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let src = probe_source("CallerProbe");
    let line = probe_line(&src, "// BP1");

    let zero = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "CallerProbe", "line": line, "trace": true, "trace_frames": 0,
        }),
    );
    assert!(zero.contains("Caller frames: 0"), "depth 0 should say so: {zero}");

    let traces = server.wait_for_traces("CallerProbe.record", EVENT_TIMEOUT).unwrap_or_default();
    assert!(traces.contains("CallerProbe.record"), "the logpoint never fired: {traces}");
    assert!(!traces.contains(" ← "), "trace_frames:0 must record no callers at all: {traces}");
    // ...and with no callers there is nothing extra on the listing either.
    assert!(
        !server.call("debug.list_stop_points", serde_json::json!({})).contains("caller frame(s)"),
        "depth 0 should not advertise a caller depth"
    );

    // A request past the cap is clamped AND says so: a silently ignored argument would leave the
    // caller believing they had a 99-frame chain.
    let over = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "CallerProbe", "line": line, "trace": true, "trace_frames": 99,
        }),
    );
    assert_contains_all("the cap is reported, not silent", &over, &["clamped to 20", "Caller frames: 20"]);

    server.panic_reset();
}

/// TRACE-7: a traced stop point reports what it has ACTUALLY cost, not what #22 measured elsewhere.
///
/// The figures are asserted against the probe's known firing rate rather than merely checked for
/// presence, because "present" is satisfied by any number — including the constants from #22's one-off
/// measurement, which is exactly what this replaces. `CallerProbe` reaches the traced line three times
/// per ~150ms iteration, so ~20 hits/s is the arrival rate the debugger has to arrive at on its own.
///
/// The zero-hit case is asserted on a stop point that CANNOT fire — the same line, pinned to a JVM
/// housekeeping thread that never runs probe code — rather than by racing the first hit. A traced stop
/// point with nothing captured must read as unmeasured, and a race that lost would have it read as
/// measured-at-zero, which is precisely the failure being guarded against.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_traced_stop_point_reports_its_observed_capture_cost() {
    let Some(jdk) = jdk_or_skip("a_traced_stop_point_reports_its_observed_capture_cost") else { return };
    let probe = Probe::launch(&jdk, "CallerProbe").expect("launch CallerProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");

    let src = probe_source("CallerProbe");
    let line = probe_line(&src, "// BP1");

    // A live thread that cannot reach probe code: HotSpot's own Reference Handler. Pinning a stop point
    // to it makes "never fires" a property of the JVM rather than of the test's timing. It must be alive,
    // or FILT-2 refuses the filter — which would fail loudly here rather than pass quietly.
    let idle_thread = thread_hex_for(&mut server, "Reference Handler")
        .expect("no Reference Handler thread — expected in every HotSpot");

    // Traced, and unable to fire: the zero-capture case.
    let never = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "CallerProbe", "line": line, "trace": true, "thread_id": idle_thread,
        }),
    );
    let quiet_id = stop_id(&never, "bp_").expect("no bp_ id in the filtered arm reply");

    // Suspending, and also unable to fire — so arming it cannot freeze the probe. It performs no capture,
    // so it must report no capture cost at all: an absence, not a zero.
    let suspending = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "CallerProbe", "line": line, "trace": false, "thread_id": idle_thread,
        }),
    );
    let suspending_id = stop_id(&suspending, "bp_").expect("no bp_ id in the suspending arm reply");
    assert_ne!(quiet_id, suspending_id, "two arms must be two stop points");

    // A budget high enough that it cannot disarm mid-test: a self-disarm is correct behaviour but would
    // stop the clock partway and make the rate a function of the test's own timing.
    let armed = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "CallerProbe", "line": line, "trace": true,
            "trace_frames": 3, "trace_max_hits": 5000,
        }),
    );
    let hot_id = stop_id(&armed, "bp_").expect("no bp_ id in the arm reply");

    // Before any hit lands anywhere, the stop point that cannot fire must say it is unmeasured.
    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    let quiet = trace_cost_line(&listed, &quiet_id)
        .unwrap_or_else(|| panic!("no cost line for the never-firing trace {quiet_id} in:\n{listed}"));
    assert!(quiet.contains("nothing captured yet"), "a trace with no hits must say so: {quiet}");
    assert!(quiet.contains("UNMEASURED"), "unmeasured must not read as free: {quiet}");
    assert!(
        trace_cost_line(&listed, &suspending_id).is_none(),
        "a suspending stop point has no capture cost to report:\n{listed}"
    );

    let base = highest_tick(&probe).expect("no tick to count from");

    // Let it run long enough for the arrival rate to mean something: ~20 hits/s, so 30 captures spans
    // several iterations rather than one burst of three.
    let deadline = std::time::Instant::now() + EVENT_TIMEOUT;
    let hot = loop {
        let listed = server.call("debug.list_stop_points", serde_json::json!({}));
        let cost = trace_cost_line(&listed, &hot_id).unwrap_or_default();
        if number_before(&cost, " capture(s)").is_some_and(|n| n >= 30.0) {
            break cost;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the traced logpoint never reported 30 captures\n  last: {cost:?}\n  probe: {:?}",
            probe.output()
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
    };

    // TRACE-2 discipline: the probe must still be advancing, since only its own output can show that no
    // hit was left suspended — and a cost measurement is worth nothing if taking it froze the debuggee.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 1)).is_some(),
        "probe stopped ticking while its trace cost was being measured\n  output: {:?}",
        probe.output()
    );

    let mean_ms = number_before(&hot, "ms mean").unwrap_or_else(|| panic!("no mean capture in: {hot}"));
    let rate = number_before(&hot, "/s (").unwrap_or_else(|| panic!("no arrival rate in: {hot}"));
    let share = number_before(&hot, "% of the window").unwrap_or_else(|| panic!("no share in: {hot}"));

    // Order of magnitude, not a hardcoded figure. A capture reads a frame, a variable table and three
    // caller locations over loopback: sub-millisecond to a few ms. Below 1µs would mean nothing was
    // timed; above 100ms would mean this is not the capture being measured.
    assert!(
        (0.001..100.0).contains(&mean_ms),
        "implausible mean capture of {mean_ms}ms — is the capture window really what is timed? {hot}"
    );
    // A `sustains ~N/s` figure used to sit here and was removed as a re-expression of the mean, so the
    // line must no longer carry one: two differently-scoped "rates" is what made #26's own acceptance
    // criteria contradict each other.
    assert!(
        !hot.contains("sustains"),
        "the derived ceiling was removed; the mean is the primitive and 1/mean recovers it: {hot}"
    );
    // Three hits per ~150ms iteration ⇒ ~20/s. Wide bounds, because the JVM's own scheduling and the
    // capture cost itself both stretch the iteration — but not wide enough to accept "one per loop"
    // (~7/s) or a rate derived from the capture window instead of the arrivals (hundreds/s).
    assert!(
        (8.0..80.0).contains(&rate),
        "expected ~20 hits/s from CallerProbe's three calls per 150ms iteration, got {rate}/s: {hot}"
    );
    // And the share must be the product of the two, which is the number that answers "is this hurting?".
    let expected_share = rate * mean_ms / 10.0; // (hits/s × s/hit) as a percentage
    assert!(
        (share - expected_share).abs() < expected_share * 0.05 + 0.1,
        "the reported {share}% does not match {rate}/s × {mean_ms}ms: {hot}"
    );
    assert!(share < 100.0, "a serialised capture cannot occupy more than the window itself: {hot}");

    server.panic_reset();
}

/// The `bp_`/`exc_`/`mexit_` id out of an arm reply.
fn stop_id(reply: &str, prefix: &str) -> Option<String> {
    let at = reply.find(prefix)?;
    let rest = reply.get(at + prefix.len()..)?;
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    let digits = rest.get(..end)?;
    (!digits.is_empty()).then(|| format!("{prefix}{digits}"))
}

/// The `⏱  Trace cost:` line belonging to one stop point in a `debug.list_stop_points` listing.
///
/// Rows start at two spaces plus a glyph and their detail lines are indented further, so a row's details
/// are exactly the following lines that begin with three spaces. Matching on the whole listing instead
/// would credit one stop point with another's cost.
fn trace_cost_line(listing: &str, id: &str) -> Option<String> {
    let header = format!("[{id}]");
    let mut lines = listing.lines().skip_while(|l| !l.contains(&header));
    lines.next()?; // the row itself
    lines.take_while(|l| l.starts_with("   ")).find(|l| l.contains("Trace cost:")).map(str::to_string)
}

/// The number immediately before `suffix`, e.g. `number_before("1.23ms mean", "ms mean") == Some(1.23)`.
fn number_before(line: &str, suffix: &str) -> Option<f64> {
    let head = line.split(suffix).next()?;
    let start = head.rfind(|c: char| !c.is_ascii_digit() && c != '.').map_or(0, |i| i + 1);
    head.get(start..)?.parse().ok()
}

/// DUMP-1: one call returns every thread's stack plus who holds what, and a real two-lock deadlock is
/// readable off the output.
///
/// The cross-pairing is the assertion that matters: each thread must be shown holding ITS lock and
/// waiting for the OTHER thread's, with the holder named. Reporting monitors per thread without
/// correlating them would satisfy a laxer test and still leave a deadlock invisible.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn thread_dump_shows_stacks_and_the_deadlock_cycle() {
    let Some(jdk) = jdk_or_skip("thread_dump_shows_stacks_and_the_deadlock_cycle") else { return };
    let probe = Probe::launch(&jdk, "DeadlockProbe").expect("launch DeadlockProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // `armed=2` means both threads hold their first lock and are reaching for the second, so the cycle
    // has formed. Dumping before that would race the deadlock into existence.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("armed=2"))
        .unwrap_or_else(|| panic!("the probe never armed both locks\n  output: {:?}", probe.output()));
    let base = highest_tick(&probe).expect("no tick to count from");

    // Without suspend:true the running threads are unreadable — and the reply must SAY that rather than
    // return an empty-looking dump that reads as "nothing is contended".
    let running =
        server.call("debug.thread_dump", serde_json::json!({"name_filter": "deadlock", "monitors": true}));
    assert_contains_all(
        "a running VM explains why it can't be read",
        &running,
        &["deadlock-one", "JDWP can only read a suspended thread", "suspend:true"],
    );
    assert!(
        !running.contains("waiting to enter"),
        "nothing should be claimed about locks it could not read: {running}"
    );

    // The real dump: freeze briefly, read, resume, verify.
    let dump = server.call(
        "debug.thread_dump",
        serde_json::json!({"name_filter": "deadlock", "suspend": true, "max_frames": 6}),
    );
    assert_contains_all(
        "the dump resumed the VM and says so",
        &dump,
        &["verified running", "Cost:", "JDWP packet(s)"],
    );

    // Each thread's own section, holding one lock and blocked on the other — with the holder named.
    // Split on "\n0x" (a thread header, always at the start of a line): a bare "0x" also appears
    // mid-line in the `← held by 0x…` annotation and inside JVM lambda class names.
    let one =
        dump_section(&dump, "deadlock-one").unwrap_or_else(|| panic!("no deadlock-one section in:\n{dump}"));
    let two =
        dump_section(&dump, "deadlock-two").unwrap_or_else(|| panic!("no deadlock-two section in:\n{dump}"));
    let (one, two) = (one.as_str(), two.as_str());

    assert!(one.contains("holds: DeadlockProbe$LockA@"), "deadlock-one must hold LockA:\n{one}");
    assert!(
        one.contains("waiting to enter: DeadlockProbe$LockB@"),
        "deadlock-one must be blocked on LockB:\n{one}"
    );
    assert!(one.contains("held by"), "the blocking lock's holder must be named:\n{one}");
    assert!(one.contains("deadlock-two"), "LockB is held by deadlock-two:\n{one}");

    assert!(two.contains("holds: DeadlockProbe$LockB@"), "deadlock-two must hold LockB:\n{two}");
    assert!(
        two.contains("waiting to enter: DeadlockProbe$LockA@"),
        "deadlock-two must be blocked on LockA:\n{two}"
    );
    assert!(two.contains("deadlock-one"), "LockA is held by deadlock-one:\n{two}");

    // Stacks came back too — a dump without frames would be a monitor report, not a thread dump.
    assert!(one.contains("DeadlockProbe.grab:"), "deadlock-one's stack must show grab():\n{one}");

    // The load-bearing check, as everywhere else in this suite: the debuggee is still running. Only
    // main can report that — the deadlocked pair never will.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2)).is_some(),
        "the probe stopped ticking after a suspending thread dump — it was not resumed\n  output: {:?}",
        probe.output(),
    );

    server.panic_reset();
}

/// #17 item 3: monitors-only answers the deadlock question without the stacks, for a fraction of the
/// suspension.
///
/// The cost comparison is the assertion that matters. "It returned lock lines" would pass on a mode that
/// read the frames anyway and merely hid them — which would be the same freeze on a shared instance for
/// a tidier reply, i.e. the opposite of the point. So the same dump is taken both ways against the same
/// probe and the packet counts are compared directly.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_monitors_only_dump_finds_the_cycle_for_a_fraction_of_the_packets() {
    let Some(jdk) = jdk_or_skip("a_monitors_only_dump_finds_the_cycle_for_a_fraction_of_the_packets") else {
        return;
    };
    let probe = Probe::launch(&jdk, "DeadlockProbe").expect("launch DeadlockProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("armed=2"))
        .unwrap_or_else(|| panic!("the probe never armed both locks\n  output: {:?}", probe.output()));
    let base = highest_tick(&probe).expect("no tick to count from");

    // Neither locks nor stacks is not a cheaper dump, it is an empty one — refused rather than answered.
    let refused =
        server.call("debug.thread_dump", serde_json::json!({"monitors_only": true, "monitors": false}));
    assert_contains_all(
        "asking for nothing at all is refused, with the reason",
        &refused,
        &["neither locks nor stacks", "Drop one of the two"],
    );

    let full = server.call(
        "debug.thread_dump",
        serde_json::json!({"name_filter": "deadlock", "suspend": true, "max_frames": 6}),
    );
    let cheap = server.call(
        "debug.thread_dump",
        serde_json::json!({"name_filter": "deadlock", "suspend": true, "monitors_only": true}),
    );

    // The cycle is still fully readable: both halves, with the holder named.
    let one = dump_section(&cheap, "deadlock-one")
        .unwrap_or_else(|| panic!("no deadlock-one section in:\n{cheap}"));
    assert!(one.contains("holds: DeadlockProbe$LockA@"), "the held lock is still reported:\n{one}");
    assert!(
        one.contains("waiting to enter: DeadlockProbe$LockB@"),
        "the contended lock is still reported:\n{one}"
    );
    assert!(one.contains("held by"), "and its holder is still named:\n{one}");
    assert!(one.contains("deadlock-two"), "by name:\n{one}");

    // No frames were read, and the reply says so rather than leaving their absence to be interpreted.
    assert_contains_all(
        "the omission is attributed to the request",
        &cheap,
        &["monitors-only", "not requested"],
    );
    assert!(!one.contains("#0 "), "monitors-only must not render frames:\n{one}");
    assert!(!cheap.contains("(no frames)"), "nor claim the threads have none:\n{cheap}");

    // The actual saving, in the units the tool reports. DeadlockProbe's threads are only five frames
    // deep, so this is the *shallow* end of the benefit — see the 60-thread measurement in TODO.md.
    let (full_cost, cheap_cost) = (dump_cost(&full), dump_cost(&cheap));
    assert!(
        cheap_cost * 3 < full_cost * 2,
        "monitors-only must cost materially less than a dump with frames, \
         but spent {cheap_cost} packets against {full_cost}\n--- full ---\n{full}\n--- cheap ---\n{cheap}"
    );

    // The decisive one: a cost that does not move with `max_frames` is proof the frames were never
    // read. An implementation that read them and merely hid them would pass every assertion above while
    // holding a shared VM for exactly as long as before — which is the failure this mode exists to
    // avoid, so "it looks cheaper" is not enough.
    let deep = server.call(
        "debug.thread_dump",
        serde_json::json!({
            "name_filter": "deadlock", "suspend": true, "monitors_only": true, "max_frames": 200
        }),
    );
    assert_eq!(
        dump_cost(&deep),
        cheap_cost,
        "monitors-only cost must be independent of max_frames, or frames are still being read\
         \n--- max_frames 200 ---\n{deep}"
    );

    // TRACE-2 discipline: only the debuggee's own output proves the cheap path resumed it too.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2)).is_some(),
        "the probe stopped ticking after a monitors-only dump — it was not resumed\n  output: {:?}",
        probe.output(),
    );

    server.panic_reset();
}

/// EVAL-5: a `toString()` that will not return must be abandoned within its budget and **reported**, not
/// waited out and then rendered as though it had cost nothing.
///
/// Measured against a real `WildFly` before the fix: 30–40 seconds of frozen VM (the event loop's generic
/// reply timeout, swept every 10s) followed by a reply byte-identical to the free shallow render. The whole
/// bug was the indistinguishability, so that is what this asserts.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_tostring_that_never_returns_is_bounded_and_reported() {
    let Some(jdk) = jdk_or_skip("a_tostring_that_never_returns_is_bounded_and_reported") else { return };
    let probe = Probe::launch(&jdk, "SlowToStringProbe").expect("launch SlowToStringProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let line = probe_line(&probe_source("SlowToStringProbe"), "// BP1");
    server
        .call("debug.set_line_stop", serde_json::json!({"class_pattern": "SlowToStringProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in SlowToStringProbe.inspect never fired");

    // Order matters here, and it is the fix's own consequence that dictates it: JDWP cannot cancel an
    // invocation, so a timed-out render leaves that thread still executing toString() and its frames
    // unreadable. Everything needing the frame therefore runs BEFORE the pathological value.

    // 1. An ordinary value is unaffected — the budget must not tax values that answer promptly.
    let fast = server.evaluate("fast");
    assert!(fast.contains("Quick(42)"), "a prompt toString() must still render normally: {fast}");
    assert!(
        !fast.contains("did not return"),
        "a prompt toString() must not be reported as timed out: {fast}"
    );

    // 2. The documented escape hatch, on the pathological value: expansion reads fields and invokes
    // nothing, so it answers where toString() cannot. This is the inversion EVAL-5 recorded — the
    // "expensive" opt-in mode is the cheap one here.
    let expanded = server.call(
        "debug.evaluate",
        serde_json::json!({"expression": "slow", "expand_objects": true, "max_depth": 1}),
    );
    assert!(
        expanded.contains("id") && !expanded.contains("did not return"),
        "expand_objects must read fields rather than invoking toString(): {expanded}"
    );

    // 3. The pathological render itself: bounded, and the reply says why it is shallow.
    let started = std::time::Instant::now();
    let slow = server.evaluate("slow");
    let elapsed = started.elapsed();
    assert_contains_all(
        "a budget expiry is reported, not hidden behind a shallow render",
        &slow,
        &["SlowToStringProbe$Blocker", "toString() did not return", "expand_objects"],
    );
    // The load-bearing assertion: bounded. The pre-fix behaviour was 30-40s of frozen VM.
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "rendering must be bounded by the invocation budget, took {elapsed:?}"
    );
    // The reply must also warn that the thread is still busy, since that is why the frame stops working.
    assert!(
        slow.contains("STILL executing"),
        "the reply must say the invocation is still running, or the next confusing error is unexplained: {slow}"
    );

    server.panic_reset();
}

/// FILT-2: when the pool retires the thread a `ThreadOnly` filter is pinned to, the stop point must say so
/// rather than presenting itself as armed.
///
/// Before the fix, `list_stop_points` showed a healthy `⚡` for a request pinned to a thread that no longer
/// existed, and `get_traces` returned an empty buffer — so silence read as "the bug did not reproduce".
/// `PoolProbe`'s `quiesce` / `resume` cues exist to reproduce it: the pool retires every idle worker, then
/// rebuilds with fresh ids.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_filter_pinned_to_a_retired_thread_reports_itself_as_dead() {
    let Some(jdk) = jdk_or_skip("a_filter_pinned_to_a_retired_thread_reports_itself_as_dead") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "PoolProbe").expect("launch PoolProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");

    let threads =
        server.call("debug.list_threads", serde_json::json!({"name_filter": "pool-worker", "limit": 400}));
    let target = threads
        .lines()
        .find(|l| l.trim_start().starts_with("0x") && l.contains("pool-worker"))
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or_else(|| panic!("no pool worker id in:\n{threads}"))
        .to_string();

    let armed = server.call(
        "debug.set_exception_stop",
        serde_json::json!({
            "class_pattern": "PoolProbe$PoolException",
            "trace": true, "trace_max_hits": 0, "thread_id": target,
        }),
    );
    assert!(armed.contains("exc_"), "filtered arm failed: {armed}");
    // It works to begin with — otherwise the assertions below would pass for the wrong reason.
    server
        .wait_for_traces("PoolProbe.doWork", EVENT_TIMEOUT)
        .unwrap_or_else(|| panic!("the filtered stop point never fired while its thread was alive"));

    // Retire the whole pool, so the filter's thread stops existing.
    probe.send_line("quiesce").expect("send quiesce");
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("quiesced"))
        .unwrap_or_else(|| panic!("the pool never quiesced\n  output: {:?}", probe.output()));
    probe.send_line("resume").expect("send resume");
    probe.wait_for_line(EVENT_TIMEOUT, |l| l.contains("resumed")).expect("the pool never resumed");

    // The pool is serving again on brand-new threads, so the filter can never match.
    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_contains_all(
        "a dead filter must be reported, not shown as armed",
        &listed,
        &["FILTER THREAD", "IS GONE", "no longer exists"],
    );

    // And the place a caller looks when nothing is arriving must say why it is quiet.
    server.call("debug.get_traces", serde_json::json!({"clear": true}));
    let traces = server.call("debug.get_traces", serde_json::json!({}));
    assert!(
        traces.contains("cannot record anything") || traces.contains("no longer exists"),
        "an empty trace buffer must explain a dead filter rather than reading as \"no hits\":\n{traces}"
    );

    // Arming afresh with the same stale id is refused, naming the real cause.
    let refused = server.call(
        "debug.set_exception_stop",
        serde_json::json!({
            "class_pattern": "PoolProbe$PoolException", "trace": true, "thread_id": target,
        }),
    );
    assert_contains_all(
        "a stale thread id is refused at arm time, not as INVALID_OBJECT",
        &refused,
        &["not a live thread", "per-connection"],
    );
    assert!(
        !refused.contains("INVALID_OBJECT"),
        "the bare JDWP code must not be what the caller sees: {refused}"
    );

    server.panic_reset();
}

/// TEST-6 assumption 1: the `ThreadOnly` filter holds against a **real thread pool** — 200 busy workers
/// reused across thousands of tasks, all running the same throw site.
///
/// `thread_filter_reports_only_the_chosen_thread` verifies the same property against two dedicated,
/// immortal threads. That leaves the interesting cases untested: a filter competing with hundreds of
/// siblings rather than one, and a thread id that outlives many units of work. This closes the local half
/// of #13's first assumption; what it still cannot reproduce is `WildFly`'s own pool under real traffic.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_thread_filter_holds_against_a_real_pool_of_reused_threads() {
    let Some(jdk) = jdk_or_skip("a_thread_filter_holds_against_a_real_pool_of_reused_threads") else {
        return;
    };
    let probe = Probe::launch(&jdk, "PoolProbe").expect("launch PoolProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // The pool pre-starts every core thread, so waiting for one heartbeat is enough for all 200 to exist.
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let base = highest_tick(&probe).expect("no tick to count from");

    let threads =
        server.call("debug.list_threads", serde_json::json!({"name_filter": "pool-worker", "limit": 400}));
    // The premise of the test: if the pool were not saturated this would be a handful of threads, and
    // "the filter excluded the others" would prove almost nothing.
    let pool_size = threads.lines().filter(|l| l.contains("pool-worker")).count();
    assert!(
        pool_size >= 100,
        "the pool must be saturated for this test to mean anything, saw {pool_size} worker(s):\n{threads}"
    );
    let target = threads
        .lines()
        .find_map(|l| l.strip_prefix("0x").map(|_| l.split_whitespace().next().unwrap_or("")))
        .filter(|t| t.starts_with("0x"))
        .unwrap_or_else(|| panic!("no pool worker id in:\n{threads}"))
        .to_string();

    let armed = server.call(
        "debug.set_exception_stop",
        serde_json::json!({
            "class_pattern": "PoolProbe$PoolException",
            "trace": true, "trace_max_hits": 0, "thread_id": target,
        }),
    );
    assert!(armed.contains("exc_"), "filtered exception breakpoint failed to arm: {armed}");

    let traces = server.wait_for_traces("PoolProbe.doWork", EVENT_TIMEOUT).unwrap_or_else(|| {
        panic!("{}", diagnose_missing_trace(&mut server, &probe, base, &target));
    });

    // The assertion that matters: exactly ONE thread reported, out of 200 running the same code.
    let seen: std::collections::BTreeSet<&str> = traces
        .lines()
        .filter_map(|l| l.split("thread=").nth(1))
        .map(|r| r.split_whitespace().next().unwrap_or(""))
        .collect();
    assert_eq!(
        seen.len(),
        1,
        "a thread-filtered stop point must report exactly one thread; saw {seen:?} among {pool_size} workers"
    );
    assert!(
        seen.contains(target.as_str()),
        "the one thread reported must be the one asked for ({target}), saw {seen:?}"
    );

    // And the other 199 kept working — the filter must not have suspended anything.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2)).is_some(),
        "the pool stopped making progress under a filtered trace\n  output: {:?}",
        probe.output().len(),
    );

    server.panic_reset();
}

/// #17: the dump reports how long it held the VM, and a suspension budget bounds that window —
/// truncating loudly rather than silently.
///
/// The held duration is the number that matters on a shared instance and the one the first version did
/// not report. The budget is proven by making it impossible to meet: 1ms against 60 parked workers cannot
/// finish, so the early exit is exercised deterministically rather than hoped for.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_dump_reports_how_long_it_held_the_vm_and_a_budget_bounds_it() {
    let Some(jdk) = jdk_or_skip("a_dump_reports_how_long_it_held_the_vm_and_a_budget_bounds_it") else {
        return;
    };
    let probe = Probe::launch(&jdk, "ManyThreadsProbe").expect("launch ManyThreadsProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let base = highest_tick(&probe).expect("no tick to count from");

    // A budget that cannot be met: 60 workers, three frames each, in 1ms.
    let truncated = server
        .call("debug.thread_dump", serde_json::json!({"suspend": true, "max_suspend_ms": 1, "limit": 60}));
    assert_contains_all(
        "an exhausted budget says so, and says the dump is incomplete",
        &truncated,
        &["Stopped early", "suspension budget ran out", "INCOMPLETE", "max_suspend_ms"],
    );
    assert!(truncated.contains("Held the VM suspended for"), "the held duration is reported: {truncated}");
    // Truncation and the resume are separate facts — stopping early must not read as failing to resume.
    assert!(
        truncated.contains("verified running"),
        "a truncated dump must still resume and verify (ADR-0003): {truncated}"
    );

    // The VM really was released, which only the probe's own output can show.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2)).is_some(),
        "the probe stopped ticking after a budget-truncated dump — it was not resumed\n  output: {:?}",
        probe.output(),
    );

    // A generous budget on a narrow dump completes, so the budget bounds rather than merely caps.
    let complete = server.call(
        "debug.thread_dump",
        serde_json::json!({"suspend": true, "max_suspend_ms": 0, "limit": 3, "max_frames": 2}),
    );
    assert!(complete.contains("Held the VM suspended for"), "duration is reported here too: {complete}");
    assert!(
        !complete.contains("Stopped early"),
        "an unbounded budget on 3 threads must not truncate: {complete}"
    );

    // A dump that never suspends owns no freeze, so it must claim none.
    let running = server.call("debug.thread_dump", serde_json::json!({"limit": 3}));
    assert!(
        !running.contains("Held the VM suspended"),
        "a non-suspending dump must not report a held duration: {running}"
    );

    server.panic_reset();
}

/// TEST-8 (#24): a dump of a PRODUCTION-SHAPED pool costs a bounded number of packets per thread.
///
/// #24 said the shared-instance defaults could only be calibrated against the real 8180. Two of the three
/// things that make the real instance different are properties of the debuggee — hundreds of threads, and
/// stacks far deeper than 8 frames — so `PoolShapeProbe` presents them here: 300 workers, 60 distinct
/// frames each, parked.
///
/// Measured against it, a whole-pool dump cost **21,364 packets and 4.7s**, and at the default 2000ms
/// budget it TRUNCATED at 40% of the pool. Nearly all of it was `Method.LineTable`, asked once per frame
/// per thread while covering ~60 distinct methods, because a request pool's threads stand in the same
/// code. With those cached per dump it is **1,625 packets / ~0.7s**, and the same dump now completes
/// inside the default budget.
///
/// **The assertion is a per-thread packet budget, not a duration.** Packet counts are deterministic and
/// independent of what else the machine is doing, so this cannot flake — and it fails loudly at ~70/thread
/// if the cache is removed or a new per-frame round trip is added. A timing assertion would be the flaky
/// restatement of the same fact.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_production_shaped_dump_costs_a_bounded_number_of_packets_per_thread() {
    let Some(jdk) = jdk_or_skip("a_production_shaped_dump_costs_a_bounded_number_of_packets_per_thread")
    else {
        return;
    };
    let probe = Probe::launch(&jdk, "PoolShapeProbe").expect("launch PoolShapeProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);
    // The workers descend 60 frames before parking; the tick line is printed only after all 300 are down.
    probe
        .wait_for_line(std::time::Duration::from_secs(60), |l| l.starts_with("tick "))
        .expect("PoolShapeProbe never finished starting its pool");

    // The whole pool, the whole depth, with the budget out of the way so this measures packets rather than
    // the clock.
    let deep = server.call(
        "debug.thread_dump",
        serde_json::json!({"suspend": true, "limit": 400, "max_frames": 200, "max_suspend_ms": 120_000}),
    );
    let (read, total) = dump_thread_counts(&deep).expect("no thread count in the dump header");
    assert!(read >= 300, "expected the whole pool, got {read}/{total}:\n{}", head_of(&deep));
    let packets = dump_packet_cost(&deep)
        .unwrap_or_else(|| panic!("no packet cost in the dump — the reply was:\n{}", head_of(&deep)));
    let per_thread = packets / read;
    assert!(
        per_thread <= 20,
        "a dump cost {per_thread} packets per thread ({packets} for {read} threads). It was ~70 before line \
         tables were cached per dump — has that cache been removed, or has a new per-frame round trip been \
         added? On an instance 1ms away this is the difference between a 2s dump and a 26s one.\n{}",
        head_of(&deep)
    );

    // Cheap because it reads no frames at all — ~4 packets per thread, as the header claims.
    let monitors = server.call(
        "debug.thread_dump",
        serde_json::json!({"suspend": true, "limit": 400, "monitors_only": true, "max_suspend_ms": 120_000}),
    );
    let m_packets = dump_packet_cost(&monitors).unwrap_or_else(|| {
        panic!("no packet cost in the monitors-only dump — the reply was:\n{}", head_of(&monitors))
    });
    let (m_read, _) = dump_thread_counts(&monitors).expect("no thread count");
    assert!(
        m_packets / m_read <= 6,
        "monitors_only should cost ~4 packets per thread, got {}\n{}",
        m_packets / m_read,
        head_of(&monitors)
    );
    // It stays the cheaper of the two — the frame read is what it skips — but the gap is no longer the ~18x
    // it was before the cache, which is the more useful fact: a deep dump is now affordable.
    assert!(m_packets < packets, "monitors_only must remain the cheaper mode: {m_packets} vs {packets}");

    // The probe must be running again afterwards; a dump that measured well and left the VM frozen is the
    // ADR-0003 failure.
    let base = highest_tick(&probe).unwrap_or(0);
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base)).is_some(),
        "probe stopped ticking after the dumps — it was left suspended\n  output: {:?}",
        probe.output()
    );

    server.panic_reset();
}

/// TEST-8 (#24): the cache's win survives a pool whose threads are in DIFFERENT code.
///
/// The headline measurement uses `PoolShapeProbe`, where all 300 workers sit in the same 60 frames — the
/// cache's best case, and the obvious objection: if the win depended on that uniformity it would not
/// survive contact with a real app server, whose workers are spread across handlers.
///
/// Both ends of the bracket are already known. Uniform costs **1,625 packets**; a pool sharing *no* frames
/// costs **21,364**, because that is the pre-cache measurement — with nothing shared the cache never hits.
/// `MixedPoolProbe` measures the realistic middle: 300 workers across 10 handlers over a shared 40-frame
/// framework prefix, so 240 distinct `(class, method)` pairs rather than 60.
///
/// Measured at **1,812** — +187 over uniform for +180 distinct pairs, one packet each, which is the cost
/// model stated exactly: `threads × fixed + distinct pairs`. Diversity is paid for per distinct frame, not
/// per thread, and the shared prefix is what carries the cache. That is why this holds on a real server.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_heterogeneous_pool_pays_only_for_its_distinct_frames() {
    let Some(jdk) = jdk_or_skip("a_heterogeneous_pool_pays_only_for_its_distinct_frames") else { return };
    let probe = Probe::launch(&jdk, "MixedPoolProbe").expect("launch MixedPoolProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);
    probe
        .wait_for_line(std::time::Duration::from_secs(60), |l| l.starts_with("tick "))
        .expect("MixedPoolProbe never finished starting its pool");

    let deep = server.call(
        "debug.thread_dump",
        serde_json::json!({"suspend": true, "limit": 400, "max_frames": 200, "max_suspend_ms": 120_000}),
    );
    let (read, total) = dump_thread_counts(&deep).expect("no thread count in the dump header");
    assert!(read >= 300, "expected the whole pool, got {read}/{total}:\n{}", head_of(&deep));
    let packets = dump_packet_cost(&deep)
        .unwrap_or_else(|| panic!("no packet cost in the dump — the reply was:\n{}", head_of(&deep)));

    // The same per-thread bound the uniform pool has to meet. Heterogeneity must not quietly restore the
    // per-frame-per-thread cost the cache exists to remove.
    let per_thread = packets / read;
    assert!(
        per_thread <= 20,
        "a heterogeneous dump cost {per_thread} packets per thread ({packets} for {read}). Uniform stacks \
         cost ~5; no sharing at all costs ~70, which is the pre-cache number. This landing near 70 would \
         mean the cache only ever worked because every thread was in identical code.\n{}",
        head_of(&deep)
    );

    // And the frames must actually be diverse, or the test is measuring the uniform case by accident: the
    // dump should show workers in several different handler classes.
    let handlers = (0..10).filter(|k| deep.contains(&format!("Handler{k}."))).count();
    assert!(
        handlers >= 5,
        "expected workers spread across handlers, found {handlers} of 10 in the dump — is the probe routing?"
    );

    server.panic_reset();
}

/// TEST-8 (#24): the line numbers in a deep dump are the RIGHT ones, per frame.
///
/// The per-dump line-table cache is keyed by (class, method) and the line is resolved per frame from the
/// cached table — so a cache keyed too coarsely, or one that stored the resolved *line* instead of the
/// table, would still produce a plausible dump with every frame showing the same number. `PoolShapeProbe`'s
/// 60 frames are 60 distinct one-line methods, so the correct answer is 60 *different* lines, each
/// checkable against the probe's own source.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_deep_dump_resolves_each_frames_own_source_line() {
    let Some(jdk) = jdk_or_skip("a_deep_dump_resolves_each_frames_own_source_line") else { return };
    let probe = Probe::launch(&jdk, "PoolShapeProbe").expect("launch PoolShapeProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);
    probe
        .wait_for_line(std::time::Duration::from_secs(60), |l| l.starts_with("tick "))
        .expect("PoolShapeProbe never finished starting its pool");

    // One worker is enough: the cache is shared across threads, so if it were wrong the first thread would
    // show it. `name_filter` keeps this to one stack rather than 300.
    let dump = server.call(
        "debug.thread_dump",
        serde_json::json!({
            "suspend": true, "limit": 1, "max_frames": 200, "max_suspend_ms": 120_000,
            "name_filter": "http-nio-8180-exec-0",
        }),
    );

    let src = probe_source("PoolShapeProbe");
    let mut checked = 0;
    // Frames render as `#<idx> PoolShapeProbe.f12:<line>`; each fN is declared on exactly one source line.
    for n in (1..60).rev() {
        let needle = format!("PoolShapeProbe.f{n}:");
        let Some(at) = dump.find(&needle) else { continue };
        let got: i32 = dump
            .get(at + needle.len()..)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_else(|| panic!("f{n} has no line number in:\n{dump}"));
        let want = probe_line(&src, &format!("static void f{n}() {{ f{}(); }}", n - 1));
        assert_eq!(got, want, "f{n} reported line {got}, its body is on line {want}");
        checked += 1;
    }
    assert!(
        checked >= 50,
        "expected ~59 chain frames to check, only found {checked} — is max_frames being applied?\n{}",
        head_of(&dump)
    );

    server.panic_reset();
}

/// TEST-8 (#24): the harness can present an instance that is NOT on loopback, and the cost model holds.
///
/// This is the capability the issue was blocked on. `LatencyRelay` puts a measured round trip in front of
/// the probe's JDWP port in userspace (`tc netem` needs `NET_ADMIN`, which a container does not have), so
/// "how does this behave against an instance 4ms away" stops needing that instance.
///
/// The model it confirms is `held ≈ packets × (our per-packet cost + RTT)`. Measured with the sweep this
/// test's assertion is drawn from: 0/1/2/4ms nominal RTT over the same workload gave ~1.0ms of extra held
/// time per ms of RTT per packet. That linearity is why **packet count is the lever** and why the fix for a
/// slow remote dump was caching, not a bigger budget.
///
/// **How the two readings are taken is part of the test** (TEST-13,
/// [#38](https://github.com/YgorPerez/java-debugging-mcp/issues/38)). This is a wall-clock comparison on a
/// box running 55 other tests, each with its own JVM, and it used to take its two readings from two
/// separate attaches — so a scheduler hiccup landing on one and not the other was indistinguishable from
/// the wire, and the test failed about one full run in three. Both readings now come off **one**
/// connection whose round trip is turned up and down between dumps, taken alternately, each arm scored on
/// its fastest sample. See the loop.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn latency_added_to_the_wire_shows_up_as_held_time_per_packet() {
    /// Samples per arm. Three is enough for one spike to be outvoted and cheap enough that the whole
    /// test still runs in seconds; the reasoning for taking the fastest of them is at the loop.
    const ROUNDS: usize = 3;

    let Some(jdk) = jdk_or_skip("latency_added_to_the_wire_shows_up_as_held_time_per_packet") else {
        return;
    };
    let probe = Probe::launch(&jdk, "PoolShapeProbe").expect("launch PoolShapeProbe");
    probe
        .wait_for_line(std::time::Duration::from_secs(60), |l| l.starts_with("tick "))
        .expect("PoolShapeProbe never finished starting its pool");

    // A small slice, so the far end of the sweep stays in seconds. Same workload at both latencies, so the
    // packet count is the same and the ONLY difference is the wire.
    let workload = serde_json::json!({
        "suspend": true, "limit": 20, "max_frames": 200, "max_suspend_ms": 120_000,
    });
    let rtt = std::time::Duration::from_millis(4);

    // One relay and one attach for both arms, with the round trip dialled between dumps. Two relays
    // behind two attaches put a JVM handshake and several seconds between the readings, which is long
    // enough for the machine to be doing something else by the second one (TEST-13).
    let relay = LatencyRelay::start(probe.port, std::time::Duration::ZERO).expect("start relay");
    let mut server = Server::start().expect("start server");
    // Attaching THROUGH the relay is the whole point: the debugger is told nothing about it.
    server.attach(relay.port);

    let mut sample = |delay: std::time::Duration| -> (u64, f64) {
        relay.set_rtt(delay);
        let dump = server.call("debug.thread_dump", workload.clone());
        let packets = dump_packet_cost(&dump).unwrap_or_else(|| {
            // TEST-25 (#71): a missing cost line and a reply that is not a dump at all produce the same
            // words, and the second is what a contended runner actually yields. Print the reply.
            panic!("no packet cost — the reply was:\n{}", head_of(&dump))
        });
        // The figure the dump reports about ITSELF, which is what a caller on a real instance reads
        // instead of doing this arithmetic (TEST-8). Asserting on the reported number rather than a
        // recomputed one is the point: it is the reading #24 wanted from the 8180.
        let reported = dump_per_packet_ms(&dump).unwrap_or_else(|| {
            panic!("the dump must report its own per-packet cost — the reply was:\n{}", head_of(&dump))
        });
        (packets, reported)
    };

    // The first dump on a fresh connection also fills the connection-lifetime method-list cache every
    // dump after it reuses (ADR-0011 caches line tables per call, `TypeCache` caches method lists per
    // connection), so it does strictly more work than the ones being compared. Thrown away rather than
    // averaged in.
    sample(std::time::Duration::ZERO);

    // Alternate the two arms instead of running one and then the other, and score each on its *lowest*
    // reading. Both halves of that are about the same hazard, which is that this is a clock.
    //
    // A busy machine can only ever make a dump slower — never faster — so the floor of a handful of
    // samples is the closest thing to the cost with the noise taken out, and it is a floor for both arms
    // alike, so it cannot invent a difference the wire did not put there. Alternating is what makes the
    // two floors comparable: a slow stretch outlasting one dump lands on both arms rather than on
    // whichever one happened to be running.
    let mut near = Vec::with_capacity(ROUNDS);
    let mut far = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        near.push(sample(std::time::Duration::ZERO));
        far.push(sample(rtt));
    }
    let fastest = |arm: &[(u64, f64)]| -> (u64, f64) {
        arm.iter().copied().min_by(|a, b| a.1.total_cmp(&b.1)).expect("an arm with no samples")
    };
    let (near_packets, near_per_packet) = fastest(&near);
    let (far_packets, far_per_packet) = fastest(&far);

    // Same work either way — if the delay changed what was read, the comparison would be meaningless.
    assert!(
        near_packets > 100 && far_packets.abs_diff(near_packets) * 10 < near_packets,
        "the wire must not change the work done: {near_packets} packets with the relay passing traffic \
         straight through vs {far_packets} with it holding each chunk"
    );

    // Each packet crosses the wire once, so the round trip shows up in the per-packet figure the dump
    // reports: at least ~half of it even allowing for coalescing, and at most a few times it allowing for
    // sleep granularity. This is the linearity that makes packet count the lever.
    let rtt_ms = rtt.as_secs_f64() * 1000.0;
    let added = far_per_packet - near_per_packet;
    assert!(
        (rtt_ms * 0.4..rtt_ms * 3.0).contains(&added),
        "a {rtt_ms}ms round trip should show up as roughly that much per packet: the fastest of {ROUNDS} \
         dumps reported {near_per_packet:.2}ms/packet with the relay passing traffic straight through and \
         {far_per_packet:.2}ms/packet with it holding each chunk, a difference of {added:.2}ms. If this \
         is ~0, either the relay is not delaying anything or the dump is not measuring itself — and every \
         measurement taken through it is worthless.\n  straight through: {near:?}\n  {rtt_ms}ms away: \
         {far:?}"
    );

    server.panic_reset();
}

/// `(threads read, threads total)` from a dump's `🧵 Thread dump — 40/306 thread(s)` header.
fn dump_thread_counts(dump: &str) -> Option<(u64, u64)> {
    let at = dump.find("dump — ")? + "dump — ".len();
    let rest = dump.get(at..)?;
    let (read, rest) = rest.split_once('/')?;
    let total: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((read.trim().parse().ok()?, total.parse().ok()?))
}

/// `(threads shown, threads total)` from a `debug.list_threads` header — `40/103 thread(s):`.
///
/// Its own parser rather than the dump's, because the two headers are deliberately different sentences
/// about the same arithmetic, and a test that could not tell them apart would let one tool's reply pass
/// for the other's.
fn list_thread_counts(listed: &str) -> Option<(u64, u64)> {
    let head = listed.lines().next()?;
    let (shown, rest) = head.split_once('/')?;
    let total: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((shown.trim().parse().ok()?, total.parse().ok()?))
}

/// The `Cost: N JDWP packet(s).` figure a dump reports.
fn dump_packet_cost(dump: &str) -> Option<u64> {
    let at = dump.find("Cost: ")? + "Cost: ".len();
    dump.get(at..)?.split_whitespace().next()?.parse().ok()
}

/// The `, 0.42ms each` figure the cost line reports — this connection's observed per-packet price.
fn dump_per_packet_ms(dump: &str) -> Option<f64> {
    let at = dump.find("packet(s), ")? + "packet(s), ".len();
    dump.get(at..)?.split("ms each").next()?.trim().parse().ok()
}

/// A dump's header lines only — the whole thing is thousands of frames, which no assertion message wants.
fn head_of(dump: &str) -> String {
    dump.lines().take(6).collect::<Vec<_>>().join("\n")
}

/// DUMP-1 + SAFE-6: a thread dump reads only, so it must work in a read-only session — and it must not
/// suspend anything unless asked, since silently pausing a shared VM is the SAFE-4 mistake.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn thread_dump_works_read_only_and_never_suspends_on_its_own() {
    let Some(jdk) = jdk_or_skip("thread_dump_works_read_only_and_never_suspends_on_its_own") else {
        return;
    };
    let probe = Probe::launch(&jdk, "DeadlockProbe").expect("launch DeadlockProbe");
    let mut server = Server::start_with_env(&[("JDWP_READONLY", "1")]).expect("start server");
    server.attach(probe.port);
    probe.wait_for_line(EVENT_TIMEOUT, |l| l.contains("armed=2")).expect("probe never armed");
    let base = highest_tick(&probe).expect("no tick to count from");

    // Reading frames and monitors invokes nothing, so read-only must not refuse it — the guard is about
    // executing code in the debuggee, and a wedged production JVM is exactly where a dump is needed.
    let dump =
        server.call("debug.thread_dump", serde_json::json!({"name_filter": "deadlock", "suspend": true}));
    assert!(
        !dump.contains("Read-only session"),
        "a dump invokes nothing, so read-only must allow it: {dump}"
    );
    assert_contains_all(
        "the read-only dump still correlates the locks",
        &dump,
        &["holds: DeadlockProbe$Lock", "held by"],
    );

    // A dump WITHOUT suspend:true must leave the VM running: no pause, so the ticks never stop.
    server.call("debug.thread_dump", serde_json::json!({"limit": 5}));
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2)).is_some(),
        "a default thread dump must not suspend the VM\n  output: {:?}",
        probe.output(),
    );
    // ...and the session must not think it is suspended either, or the watchdog would try to rescue it.
    assert!(
        !server.call("debug.list_sessions", serde_json::json!({})).contains("SUSPENDED"),
        "a default thread dump must leave no suspension behind"
    );

    server.panic_reset();
}

/// METH-1: a method-exit request reports which `return` was taken AND what it returned.
///
/// `ReturnProbe.classify` has two returns and alternates between them, so the test can pair each hit's
/// value with its return site. A probe with one return would pass even if the value were read from the
/// wrong place, and a non-null-only probe would let a missing `null` look like a missing value.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn method_exit_reports_the_value_each_return_produced() {
    let Some(jdk) = jdk_or_skip("method_exit_reports_the_value_each_return_produced") else { return };
    let probe = Probe::launch(&jdk, "ReturnProbe").expect("launch ReturnProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let base = highest_tick(&probe).expect("no tick to count from");

    // No `trace` given: this kind defaults to trace mode, unlike every other stop point.
    let set = server.call(
        "debug.set_method_exit_stop",
        serde_json::json!({"class_pattern": "ReturnProbe", "method": "classify"}),
    );
    assert_contains_all(
        "armed in trace mode by default",
        &set,
        &["mexit_", "trace (non-suspending)", "Method filter: classify"],
    );

    // The TRACE-2 discipline: the probe must keep printing.
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2))
            .is_some(),
        "probe stopped ticking after a traced method-exit request — a return left it suspended\n  output: {:?}",
        probe.output(),
    );

    let traces = server
        .wait_for_traces("returned=null", EVENT_TIMEOUT)
        .unwrap_or_else(|| panic!("the null-returning path was never reported"));

    // Both values, from the two different `return` statements.
    let ok = traces
        .lines()
        .find(|l| l.contains("returned=\"OK\""))
        .unwrap_or_else(|| panic!("no \"OK\" return reported in:\n{traces}"));
    let null = traces
        .lines()
        .find(|l| l.contains("returned=null"))
        .unwrap_or_else(|| panic!("no null return reported in:\n{traces}"));

    // Which `return` was taken: the two paths must report DIFFERENT lines, or the location is not
    // actually the return site and "which path did it take" is unanswered.
    let line_of = |s: &str| {
        s.split("ReturnProbe.classify:").nth(1).and_then(|r| {
            let end = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
            r.get(..end).and_then(|d| d.parse::<i32>().ok())
        })
    };
    let (ok_line, null_line) = (
        line_of(ok).unwrap_or_else(|| panic!("no return site on: {ok}")),
        line_of(null).unwrap_or_else(|| panic!("no return site on: {null}")),
    );
    assert_ne!(
        ok_line, null_line,
        "the two returns are different statements, so their return sites must differ:\n  {ok}\n  {null}"
    );

    // The method filter is real: `other()` also returns on every iteration, and JDWP's ClassMatch
    // reported it to us — it must have been dropped rather than recorded.
    assert!(
        !traces.contains("ReturnProbe.other"),
        "the method filter must drop other()'s returns:\n{traces}"
    );

    // The whole set of bookkeeping tools must know about this kind — a stop point that can be created
    // but not listed or cleared would be a SAFE-class bug.
    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_contains_all(
        "listed as a method-exit request",
        &listed,
        &["method-exit ReturnProbe.classify", "with return value", "(trace)"],
    );
    let mexit_id = grab_token(&listed, "mexit_").expect("no mexit id in the listing");

    let off = server
        .call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": mexit_id, "enabled": false}));
    assert_contains_all("toggle disables it", &off, &["Disabled", "method-exit"]);
    let on = server
        .call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": mexit_id, "enabled": true}));
    assert_contains_all("toggle re-arms it under the same id", &on, &["Re-armed", &mexit_id]);
    let cleared = server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": mexit_id}));
    assert_contains_all("clear removes it", &cleared, &["cleared", &mexit_id]);
    assert!(
        !server.call("debug.list_stop_points", serde_json::json!({})).contains(&mexit_id),
        "a cleared method-exit request must be gone from the listing"
    );

    server.panic_reset();
}

/// METH-1: the safety rule for the noisiest event kind in JDWP — a broad SUSPENDING method-exit request
/// is refused outright, with the reason and the fix, and `panic` drops the ones that do get armed.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_broad_suspending_method_exit_is_refused_and_panic_clears_the_rest() {
    let Some(jdk) = jdk_or_skip("a_broad_suspending_method_exit_is_refused_and_panic_clears_the_rest") else {
        return;
    };
    let probe = Probe::launch(&jdk, "ReturnProbe").expect("launch ReturnProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let base = highest_tick(&probe).expect("no tick to count from");

    // No method filter + suspending: this would freeze the VM on the first return of any method.
    let no_method = server.call(
        "debug.set_method_exit_stop",
        serde_json::json!({"class_pattern": "ReturnProbe", "trace": false}),
    );
    assert_contains_all(
        "a suspending request with no method filter is refused",
        &no_method,
        &["Refused", "no method filter", "trace:true"],
    );

    // A wildcard class is refused for the same reason even WITH a method name.
    let wildcard = server.call(
        "debug.set_method_exit_stop",
        serde_json::json!({"class_pattern": "Return*", "method": "classify", "trace": false}),
    );
    assert_contains_all("a wildcard class is refused too", &wildcard, &["Refused", "Return*"]);

    // Neither refusal may have armed anything — a refusal that half-armed would be worse than allowing it.
    assert!(
        !server.call("debug.list_stop_points", serde_json::json!({})).contains("mexit_"),
        "a refused request must not be armed"
    );
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 1)).is_some(),
        "the probe must still be running after two refusals\n  output: {:?}",
        probe.output(),
    );

    // The narrow form IS allowed: one concrete class, one method, explicitly suspending.
    let narrow = server.call(
        "debug.set_method_exit_stop",
        serde_json::json!({"class_pattern": "ReturnProbe", "method": "classify", "trace": false}),
    );
    assert_contains_all("the narrow suspending form is allowed", &narrow, &["mexit_", "SUSPENDING"]);

    // It suspends on a real return, and reports the value through the event path (not get_traces).
    let ev = server
        .wait_for_event("\"event\":\"method_exit\"", EVENT_TIMEOUT)
        .unwrap_or_else(|| panic!("a suspending method exit never fired"));
    assert_contains_all("the event reports the return site and value", &ev, &["ReturnProbe", "returned"]);

    // panic must drop it: resuming without clearing would re-freeze on the very next return, which for
    // this kind is immediate.
    let panicked = server.panic_reset();
    assert!(
        panicked.contains("method-exit"),
        "panic must say it cleared the method-exit request: {panicked}"
    );
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 3))
            .is_some(),
        "panic must leave the probe running — a method-exit request left armed re-freezes it at once\n  output: {:?}",
        probe.output(),
    );
}

/// EVT-1: a second hit must not erase the first. Before the event ring buffer, `last_event` was one
/// slot, so the breakpoint below was silently gone by the time the step landed.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn events_are_buffered_so_a_second_hit_doesnt_erase_the_first() {
    let Some(jdk) = jdk_or_skip("events_are_buffered_so_a_second_hit_doesnt_erase_the_first") else { return };
    let probe = Probe::launch(&jdk, "ExcProbe").expect("launch ExcProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // A breakpoint then a step: two suspending events in a row, the second arriving before anything
    // has read the first. Single-threaded and deterministic, unlike racing two threads at one line.
    let line = probe_line(&probe_source("ExcProbe"), "// BP2");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "ExcProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in ExcProbe.main never fired");
    server.call("debug.step_over", serde_json::json!({}));
    server.wait_for_event("\"event\":\"step\"", EVENT_TIMEOUT).expect("step never reported");

    // A bare call still means "the newest event", as it always did — plus a note that there is more.
    let latest = server.last_event();
    // TEST-23 (#64): this assertion failed in CI reporting `[pending] 2 older event(s)` where it staged
    // one, and a *count* is not a diagnosis — three events existed and nothing said what the third was.
    // The whole buffer is read here (no `drain`, so it changes nothing) purely so the failure names it.
    let buffered = server.call("debug.get_last_event", serde_json::json!({"limit": 10}));
    assert_contains_all(
        &format!("newest event, and the backlog is announced\nthe whole buffer was:\n{buffered}"),
        &latest,
        &["\"event\":\"step\"", "[pending] 1 older event"],
    );
    assert!(
        !latest.contains("\"event\":\"breakpoint\""),
        "the default limit must return only the newest event: {latest}"
    );

    // Both are still there. This is the assertion that fails against a single-slot `last_event`.
    let both = server.call("debug.get_last_event", serde_json::json!({"limit": 5}));
    assert_contains_all(
        "both hits are retrievable",
        &both,
        &["\"event\":\"breakpoint\"", "\"event\":\"step\""],
    );
    assert!(
        both.find("\"event\":\"breakpoint\"") < both.find("\"event\":\"step\""),
        "buffered events read oldest-first, so the breakpoint must precede the step:\n{both}"
    );
    assert!(!both.contains("[pending]"), "nothing is pending once all of it is shown:\n{both}");

    // Draining discards what was read, so the next call doesn't re-report old hits.
    server.panic_reset();
    server.call("debug.get_last_event", serde_json::json!({"limit": 10, "drain": true}));
    let after = server.last_event();
    assert!(
        !after.contains("\"event\":\"breakpoint\"") && !after.contains("\"event\":\"step\""),
        "drain:true must discard the events it returned, got: {after}"
    );
}

/// OBJ-3: `get_stack {expand_objects:true}` must spend ONE node budget across the whole call. It used
/// to allocate a fresh 400 per local, so a 20-local frame × 20 frames could walk ~160k nodes against a
/// possibly-shared JVM — a documented cap that bounded nothing.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn get_stack_node_budget_bounds_the_whole_call() {
    let Some(jdk) = jdk_or_skip("get_stack_node_budget_bounds_the_whole_call") else { return };
    let probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let line = probe_line(&probe_source("DeepProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in DeepProbe.inspect never fired");

    // `inspect` holds `order` and `batch` (the same order four times over), and `main` below it holds
    // `order` again — several expandable locals across two frames, which is exactly the shape a
    // per-local budget failed to bound.
    let stack = server.call(
        "debug.get_stack",
        serde_json::json!({"expand_objects": true, "max_depth": 5, "max_children": 30}),
    );
    assert_contains_all(
        "the cap is reported once, naming where it stopped",
        &stack,
        &["node budget (1000) exhausted at #", "remaining frames not expanded"],
    );
    assert!(
        stack.matches("node budget").count() == 1,
        "the exhaustion notice belongs once per call, not per local:\n{stack}"
    );
    assert!(
        stack.trim_end().ends_with("debug.evaluate."),
        "expansion must stop at the cap, not carry on into later frames:\n{stack}"
    );
    // Sanity: it did real work before giving up, rather than bailing on the first local.
    assert_contains_all("it expanded what it could", &stack, &["order = ", "id = (int) 42"]);

    // `debug.evaluate` is unchanged: its own, smaller budget, and its own message.
    let one = server.call(
        "debug.evaluate",
        serde_json::json!({"expression": "batch", "expand_objects": true, "max_depth": 5, "max_children": 30}),
    );
    assert_contains_all(
        "evaluate keeps its documented per-expression budget",
        &one,
        &["node budget (400) exhausted"],
    );

    // Every frame keeps its locals under expansion. Expanding frame #0 invokes toArray/toString in the
    // debuggee, which invalidates the thread's frame ids — so frame #1's id, read before that, is
    // stale. It used to fail silently, printing `main` with no locals as though it had none.
    let both_frames = server.call(
        "debug.get_stack",
        serde_json::json!({"expand_objects": true, "max_depth": 1, "max_children": 2}),
    );
    let main_frame = both_frames
        .split_once("DeepProbe.main")
        .map_or_else(|| panic!("no main frame in:\n{both_frames}"), |(_, rest)| rest.to_string());
    assert_contains_all(
        "a frame below an expanded one still shows its locals",
        &main_frame,
        &["i = (int)", "order = DeepProbe$Order"],
    );

    server.panic_reset();
}

/// OBJ-4: the two things OBJ-2 deliberately left out — writing through a subscript, and filtering a
/// `Map` without losing the keys.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn subscript_writes_and_map_entry_filters() {
    let Some(jdk) = jdk_or_skip("subscript_writes_and_map_entry_filters") else { return };
    let probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let line = probe_line(&probe_source("DeepProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in DeepProbe.inspect never fired");

    let write = |server: &mut Server, target: &str, value: &str| {
        server.call("debug.set_value", serde_json::json!({"target": target, "value": value}))
    };

    // --- Array element: ArrayReference.SetValues, no invocation in the debuggee ---
    let set = write(&mut server, "order.numbers[1]", "42");
    assert_contains_all(
        "array element written, old value reported",
        &set,
        &["numbers[1] = 42", "was (int) 2"],
    );
    assert_contains_all("and it stuck", &server.evaluate("order.numbers[1]"), &["(int) 42"]);
    // The neighbours must be untouched — an untagged write of the wrong width would corrupt them.
    assert_contains_all("neighbours intact", &server.evaluate("order.numbers[0]"), &["(int) 1"]);
    assert_contains_all("neighbours intact", &server.evaluate("order.numbers[2]"), &["(int) 3"]);

    // --- List element: List.set(index, value), which hands back what it displaced ---
    let set = write(&mut server, "order.tags[0]", "\"replaced\"");
    assert_contains_all("List element written via set()", &set, &["tags[0] = ", "urgent", "set()"]);
    assert_contains_all("and it stuck", &server.evaluate("order.tags[0]"), &["\"replaced\""]);

    // --- Map value: Map.put(key, value), with the int boxed into the Integer the map holds ---
    let set = write(&mut server, "order.counts[\"a\"]", "9");
    assert_contains_all("Map value written via put()", &set, &["counts[\"a\"] = 9", "put()"]);
    assert_contains_all("and it stuck, boxed", &server.evaluate("order.counts[\"a\"]"), &["(int) 9"]);

    // --- The refusals ---
    assert_contains_all(
        "out of bounds",
        &write(&mut server, "order.numbers[9]", "1"),
        &["out of bounds", "length 3"],
    );
    assert_contains_all(
        "type mismatch against the component type",
        &write(&mut server, "order.numbers[0]", "\"text\""),
        &["int"],
    );
    assert_contains_all(
        "a slice names several elements, so there is nothing single to write",
        &write(&mut server, "order.numbers[0..2]", "1"),
        &["selects several elements"],
    );
    assert_contains_all(
        "so does a filter",
        &write(&mut server, "order.lines[?paid == true]", "1"),
        &["selects several elements"],
    );
    // The array write must not have been corrupted by any of the refused attempts.
    assert_contains_all("refusals changed nothing", &server.evaluate("order.numbers[0]"), &["(int) 1"]);

    // --- Map entry filtering: predicate against each VALUE, keys preserved in the output ---
    // qty > 3 keeps bb(5), dd(9) and ee(4) — three of five.
    let matched = server.evaluate("order.byId[?qty > 3]");
    assert_contains_all(
        "filtered entries render as key → value",
        &matched,
        &["3 of 5 entr(ies)", "\"bb\" → ", "Line(bb,5,false)", "\"dd\" → ", "Line(dd,9,false)", "\"ee\" → "],
    );
    assert!(!matched.contains("Line(aa"), "qty 1 must not match qty > 3:\n{matched}");
    assert!(!matched.contains("Line(cc"), "qty 2 must not match qty > 3:\n{matched}");
    assert!(!matched.contains("[0] = "), "map results are keyed, not positional:\n{matched}");
    // Zero matches is still a keyed, counted answer rather than an error.
    assert_contains_all("no matches", &server.evaluate("order.byId[?qty > 99]"), &["0 of 5 entr(ies)"]);
    // A slice still has no meaning on a Map, and says why.
    assert_contains_all(
        "slicing a Map is refused with the alternative",
        &server.evaluate("order.byId[0..2]"),
        &["no order to slice"],
    );

    server.panic_reset();
}

/// Why a trace that was expected never arrived (TEST-22, #57).
///
/// `the filtered stop point never recorded a throw` was the whole of this failure's message, and it
/// covers at least four different worlds: the request was never armed, it armed and then disarmed
/// itself, the debuggee stopped running, or the filter is pinned to a thread that has died — which is
/// the ordinary fate of a pool worker and the thing a *filtered* stop point is most exposed to. Each has
/// a different next step, and picking between them cost a soak run every time it fired.
///
/// So the evidence is gathered at the moment of failure, from the three places that can distinguish
/// them: the stop-point listing (armed? disabled? filtered to a dead thread?), the unfiltered trace
/// buffer (did the site fire at all, for anyone?), and the probe's own tick counter (is the debuggee
/// still working?). Nothing here fixes the flake; it makes its next sighting worth having.
fn diagnose_missing_trace(server: &mut Server, probe: &Probe, base_tick: i64, target: &str) -> String {
    let stop_points = server.call("debug.list_stop_points", serde_json::json!({}));
    let all_traces = server.call("debug.get_traces", serde_json::json!({}));
    let threads =
        server.call("debug.list_threads", serde_json::json!({"name_filter": "pool-worker", "limit": 400}));
    let target_alive = threads.lines().any(|l| l.starts_with(target));
    let now = highest_tick(probe);
    let advanced = now.is_some_and(|n| n > base_tick);

    format!(
        "the filtered stop point never recorded a throw from PoolProbe.doWork within {EVENT_TIMEOUT:?}. \
         Which of these is true decides what to look at next:\n  \
         (1) the debuggee: tick was {base_tick}, is now {now:?} — {}\n  \
         (2) the filter's thread {target}: {} in the pool listing. A pool that retires idle workers \
         invalidates the id, and the stop point then reports nothing at all (FILT-2)\n  \
         (3) the request: does the listing below show it armed and enabled, or disarmed?\n{stop_points}\n  \
         (4) the throw site: the UNFILTERED trace buffer holds {} record(s) — if it is empty the site \
         never fired for anyone (a probe problem), if it has records from other threads the filter is \
         what dropped them (a filtering problem)\n{}",
        if advanced { "still running" } else { "NOT advancing, so nothing could have thrown" },
        if target_alive { "still alive" } else { "GONE" },
        all_traces.lines().filter(|l| l.contains("thread=")).count(),
        head_of(&all_traces),
    )
}

/// How many `stable-worker-*` threads the debuggee has alive right now, read straight from the JVM.
///
/// Used to tell a selection-rule failure apart from a probe that never reached its steady state
/// (TEST-22, #57). Deliberately asks with a wide `limit` and a name filter, so the answer cannot itself
/// be truncated by the very default the caller is testing.
fn stable_workers_in_debuggee(server: &mut Server) -> usize {
    let all =
        server.call("debug.list_threads", serde_json::json!({"name_filter": "stable-worker", "limit": 400}));
    all.lines().filter(|l| l.contains("stable-worker-")).count()
}

/// The `<n>` of `ExcProbe`'s / `WatchProbe`'s `tick <n> …` line. Both count something that only advances
/// while the JVM is running, which is how a test proves nothing was left suspended.
fn tick_index(line: &str) -> Option<i64> {
    line.strip_prefix("tick ")?.split_whitespace().next()?.parse().ok()
}

/// The highest tick a probe has printed so far.
fn highest_tick(probe: &Probe) -> Option<i64> {
    probe.output().iter().filter_map(|l| tick_index(l)).max()
}

/// Pull `<key>=(int) N` out of one `debug.get_traces` line.
fn trace_int(line: &str, key: &str) -> Option<i64> {
    let needle = format!("{key}=(int) ");
    let at = line.find(&needle)? + needle.len();
    let rest = &line[at..];
    let end = rest.find(|c: char| !c.is_ascii_digit() && c != '-').unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The integer in a rendered `debug.evaluate` result like `n = (int) 3`.
fn int_value(rendered: &str) -> Option<i64> {
    let start = rendered.find("(int) ")? + "(int) ".len();
    let rest = &rendered[start..];
    let end = rest.find(|c: char| !c.is_ascii_digit() && c != '-').unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Pull the ints out of `"old":"(int) 5"` / `"new":"(int) 6"` in an event line.
fn parse_old_new(ev: &str) -> Option<(i64, i64)> {
    let grab = |key: &str| -> Option<i64> {
        let at = ev.find(&format!("\"{key}\":\""))?;
        let rest = &ev[at..];
        let start = rest.find("(int) ")? + "(int) ".len();
        let end = rest[start..].find('"')? + start;
        rest[start..end].trim().parse().ok()
    };
    Some((grab("old")?, grab("new")?))
}

/// SESS-1: `debug.list_sessions` — concurrent sessions are addressable, and one whose JVM has gone is
/// reported dead rather than listed as healthy.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn list_sessions_names_every_attachment_and_flags_a_dead_one() {
    let Some(jdk) = jdk_or_skip("list_sessions_names_every_attachment_and_flags_a_dead_one") else { return };
    let mut server = Server::start().expect("start server");

    assert_contains_all(
        "no sessions yet",
        &server.call("debug.list_sessions", serde_json::json!({})),
        &["No debug sessions"],
    );

    // Two probes, so the listing has to distinguish them — and the second attach becomes current.
    // `first` must be *running*: the stop point below is a watchpoint, which cannot be deferred, and a
    // refused arm would leave this session's count at zero and fail the listing assertion for a reason
    // that has nothing to do with sessions. Same accidental-slack story as
    // `watchpoints_report_field_writes_and_reads` (TEST-20, #55).
    let first =
        Probe::launch_running(&jdk, "WatchProbe", |l| tick_index(l).is_some()).expect("launch WatchProbe");
    let attach_first = server.attach(first.port);
    let first_id = session_id_from(&attach_first).expect("no session id in attach reply");
    let second = Probe::launch(&jdk, "ExcProbe").expect("launch ExcProbe");
    let attach_second = server.attach(second.port);
    let second_id = session_id_from(&attach_second).expect("no session id in attach reply");
    assert_ne!(first_id, second_id, "each attach must get its own session");

    // Give the older session a stop point, so the counts are visibly per-session rather than global.
    server.call(
        "debug.set_field_stop",
        serde_json::json!({
            "session_id": first_id, "class_name": "WatchProbe", "field_name": "counter", "trace": true,
        }),
    );

    let listed = server.call("debug.list_sessions", serde_json::json!({}));
    assert_contains_all(
        "both sessions, by endpoint",
        &listed,
        &["2 session(s)", &first.port.to_string(), &second.port.to_string(), &first_id, &second_id],
    );
    assert_contains_all("the newest attach is current", &listed, &["← current"]);
    assert_eq!(listed.matches("← current").count(), 1, "exactly one session is current:\n{listed}");
    let current_line = listed.lines().find(|l| l.contains("← current")).expect("a current line");
    assert!(current_line.contains(&second_id), "the last attach should be current, got: {current_line}");
    let first_line = listed.lines().find(|l| l.contains(&first_id)).expect("a line for the first session");
    assert_contains_all("per-session stop-point count", first_line, &["1 stop point(s)"]);

    // Kill the older probe's JVM. The event pump ends with the connection, which is what marks the
    // session dead — no round trip, so this can't hang on a half-closed socket.
    drop(first);
    let mut dead_seen = String::new();
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        dead_seen = server.call("debug.list_sessions", serde_json::json!({}));
        if dead_seen.contains("DEAD") {
            break;
        }
    }
    let dead_line = dead_seen
        .lines()
        .find(|l| l.contains(&first_id))
        .unwrap_or_else(|| panic!("no line for the dead session in:\n{dead_seen}"));
    assert_contains_all("a gone JVM is reported dead", dead_line, &["DEAD"]);
    // The surviving session must not be collateral damage.
    let live_line = dead_seen.lines().find(|l| l.contains(&second_id)).expect("a line for the live session");
    assert!(!live_line.contains("DEAD"), "the other session is still attached: {live_line}");

    // And it can be removed by id, which is the escape hatch the listing points at.
    server.call("debug.disconnect", serde_json::json!({"session_id": first_id}));
    let after = server.call("debug.list_sessions", serde_json::json!({}));
    assert_contains_all("one left", &after, &["1 session(s)", &second_id]);
    assert!(!after.contains(&first_id), "the disconnected session must be gone:\n{after}");
}

/// Pull `session_id` out of an attach reply — `… (session: session_abc123)`.
fn session_id_from(attach_reply: &str) -> Option<String> {
    let at = attach_reply.find("session: ")? + "session: ".len();
    let rest = &attach_reply[at..];
    let end = rest.find(')')?;
    Some(rest[..end].to_string())
}

/// The packet-count instrument behind every round-trip claim in
/// `docs/VARIABLE_INSPECTION_PLAN.md`, as a regression guard: a deep expansion of the `DeepProbe` graph
/// must stay in the low hundreds of JDWP commands.
///
/// Wall-clock is the wrong tool here — over loopback a round trip is sub-millisecond, and noise swamps
/// the signal (measuring the type cache that way first suggested it did nothing). The bound is generous
/// on purpose: it exists to catch a return to per-object refetching, which measured 421 packets against
/// the cache's 218, not to pin an exact number that a different JDK would shift.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn deep_expansion_stays_within_its_packet_budget() {
    let Some(jdk) = jdk_or_skip("deep_expansion_stays_within_its_packet_budget") else { return };
    let probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let line = probe_line(&probe_source("DeepProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in DeepProbe.inspect never fired");

    let expand = |server: &mut Server| {
        server.call(
            "debug.evaluate",
            serde_json::json!({
                "expression": "order", "expand_objects": true, "max_depth": 3, "max_children": 30,
            }),
        )
    };

    let before = packets_sent(&mut server);
    expand(&mut server);
    let cold = packets_sent(&mut server) - before;

    // A second identical expansion pays only for values, since every type's shape is already cached.
    expand(&mut server);
    let warm = packets_sent(&mut server) - before - cold;

    println!("deep expansion: {cold} packets cold, {warm} warm");
    assert!(cold > 0 && warm > 0, "the instrument reported nothing: {cold} cold, {warm} warm");
    assert!(
        cold < 600,
        "a cold deep expansion cost {cold} packets — the type cache measured 218; has shape caching regressed?"
    );
    assert!(
        warm < cold,
        "a warm expansion ({warm}) should cost less than a cold one ({cold}) — that is what the cache buys"
    );

    server.panic_reset();
}

/// The current session's JDWP command count, from `debug.list_sessions`.
fn packets_sent(server: &mut Server) -> u32 {
    let listed = server.call("debug.list_sessions", serde_json::json!({}));
    let line = listed
        .lines()
        .find(|l| l.contains("← current"))
        .unwrap_or_else(|| panic!("no current session in:\n{listed}"));
    let (head, _) =
        line.split_once(" JDWP packet(s)").unwrap_or_else(|| panic!("no packet count in: {line}"));
    head.rsplit(' ')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("unparseable packet count in: {line}"))
}

/// Grab the first token starting with `prefix` (an id like `bp_5`, `watch_modify_2`, `exc_7`) out of a
/// tool reply — the token runs until the first character that isn't alphanumeric or `_`.
fn grab_token(text: &str, prefix: &str) -> Option<String> {
    let at = text.find(prefix)?;
    let rest = &text[at..];
    let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// The hex thread id (`0x…`) of the first thread whose name contains `name_substr`, via `list_threads`.
fn thread_hex_for(server: &mut Server, name_substr: &str) -> Option<String> {
    let listed = server.call("debug.list_threads", serde_json::json!({"name_filter": name_substr}));
    listed.lines().find_map(|l| {
        let t = l.trim();
        t.starts_with("0x").then(|| t.split_whitespace().next().map(str::to_string)).flatten()
    })
}

/// One thread's block from a `debug.thread_dump` reply, found by a substring of its header line.
///
/// Sections are delimited by a thread header at the start of a line (`0x<id> "<name>" […]`). Splitting
/// on a bare `0x` would cut a section short, because `0x` also appears mid-line in the `← held by 0x…`
/// annotation and inside JVM lambda class names (`DeadlockProbe$$Lambda.0x…`) — which is exactly how
/// this helper's absence first showed up as a missing `holds:` line.
fn dump_section(dump: &str, header_substr: &str) -> Option<String> {
    let mut sections: Vec<String> = Vec::new();
    for line in dump.lines() {
        if line.starts_with("0x") {
            sections.push(String::new());
        }
        if let Some(cur) = sections.last_mut() {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    sections.into_iter().find(|s| s.lines().next().is_some_and(|h| h.contains(header_substr)))
}

/// The packet count off a `debug.thread_dump` reply's trailing `Cost: N JDWP packet(s).` line.
///
/// Panics rather than returning an `Option`: a dump that did not report its cost is a failure of the
/// reply contract, and swallowing it would let a cost comparison silently pass on two zeroes.
fn dump_cost(dump: &str) -> u32 {
    dump.lines()
        .find_map(|l| l.strip_prefix("Cost: ")?.split_whitespace().next()?.parse().ok())
        .unwrap_or_else(|| panic!("no `Cost: N JDWP packet(s)` line in:\n{dump}"))
}

/// Count the trace records (lines beginning with `#`) in a `debug.get_traces` reply.
fn count_trace_records(traces: &str) -> usize {
    traces.lines().filter(|l| l.trim_start().starts_with('#')).count()
}

/// TEST-4 + SAFE-2: the watchdog is the project's primary safety mechanism and had no coverage. With
/// `JDWP_WATCHDOG_SECS=1`, a breakpoint that leaves the VM suspended must be auto-resumed — proven by
/// the probe's OWN output starting to advance again — and the offending breakpoint disarmed, so it
/// doesn't just re-freeze on the next hit.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn watchdog_auto_resumes_and_disarms_the_offending_breakpoint() {
    let Some(jdk) = jdk_or_skip("watchdog_auto_resumes_and_disarms_the_offending_breakpoint") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "1")]).expect("start server");
    server.attach(probe.port);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");

    // A plain (suspending) breakpoint on the only writer of `counter`: on hit it freezes the whole VM,
    // so the probe stops ticking — exactly the forgotten-breakpoint hazard the watchdog exists for.
    let line = probe_line(&probe_source("WatchProbe"), "counter = counter + 1;");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in bumpCounter never fired");
    let frozen_at = highest_tick(&probe).expect("no tick before suspension");

    // The watchdog must resume the VM AND disarm the breakpoint. The debuggee's own tick line is the
    // only thing that proves it really resumed (the debugger would report success either way), and a
    // tick well past the freeze point proves it didn't just re-freeze on the next bump.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at + 3)).is_some(),
        "probe never resumed ticking after the watchdog window — it was left frozen\n  output: {:?}",
        probe.output(),
    );

    // The disarm is discoverable, in list_stop_points and in the next get_last_event — and per BP-2 the
    // breakpoint is *disabled*, not deleted, so its definition survived and it can be re-armed.
    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_contains_all(
        "watchdog action is surfaced, and the stop point is kept as disabled",
        &listed,
        &["watchdog auto-resumed", "disabled", "DISABLED"],
    );
    assert!(
        server.last_event().contains("[watchdog]"),
        "get_last_event should carry the watchdog note so a returning caller sees the VM was rescued"
    );

    server.panic_reset();
}

/// SAFE-10 (#69): a watchdog note belongs to the suspension it rescued, so a *newer* hit must not be
/// rendered underneath it.
///
/// The bug this pins down produced a `get_last_event` whose two lines were each correct and jointly
/// false — `[suspended] true` for a live breakpoint hit, over a `[watchdog] auto-resumed the VM` about a
/// suspension that had already ended — which reads as "the hit you are looking at is stale".
///
/// **Asserted on the note's *identity*, not its presence, and that is what keeps it off the flake list.**
/// `JDWP_WATCHDOG_SECS=1` means the second freeze will be rescued too, a second or two later, and its
/// own note is then legitimate. So racing to read before that happens would be the flaky version of this
/// test. Instead: whatever `get_last_event` says after the second hit, it must not be about the *first*
/// breakpoint — which is precisely what the old code replayed forever, and is true no matter which side
/// of the second rescue the read lands on.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_watchdog_note_is_not_replayed_next_to_a_newer_hit() {
    let Some(jdk) = jdk_or_skip("a_watchdog_note_is_not_replayed_next_to_a_newer_hit") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "1")]).expect("start server");
    server.attach(probe.port);
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");

    let src = probe_source("WatchProbe");
    // Two different methods, each reached every iteration, so the second breakpoint is certain to fire.
    let (line_a, line_b) =
        (probe_line(&src, "counter = counter + 1;"), probe_line(&src, "holder.label = (i % 2 == 0)"));
    assert_ne!(line_a, line_b, "the two freezes must be distinguishable stop points");

    // First freeze, then its rescue — the state SAFE-2 exists to report, and the note is correct here.
    let set_a = server
        .call("debug.set_line_stop", serde_json::json!({"class_pattern": "WatchProbe", "line": line_a}));
    let id_a = grab_token(&set_a, "bp_").expect("no bp id in the first arm reply");
    server
        .wait_for_event(&format!("\"line\":{line_a}"), EVENT_TIMEOUT)
        .expect("first breakpoint never fired");
    let frozen_at = highest_tick(&probe).expect("no tick before the first suspension");
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at + 3)).is_some(),
        "the watchdog never rescued the first freeze\n  output: {:?}",
        probe.output(),
    );
    let rescued = server.last_event();
    let note = rescued
        .lines()
        .find(|l| l.starts_with("[watchdog]"))
        .unwrap_or_else(|| panic!("SAFE-2's own case regressed — no note after a rescue:\n{rescued}"));
    assert!(
        note.contains(&id_a),
        "the note should name the stop point it disarmed ({id_a}), or the assertion below proves nothing: {note}"
    );

    // A fresh hit on a DIFFERENT stop point. The first breakpoint was disabled by the rescue, so this
    // event can only be the second one.
    let set_b = server
        .call("debug.set_line_stop", serde_json::json!({"class_pattern": "WatchProbe", "line": line_b}));
    let id_b = grab_token(&set_b, "bp_").expect("no bp id in the second arm reply");
    assert_ne!(id_a, id_b, "the second arm reused the first id, so the two notes are indistinguishable");
    server
        .wait_for_event(&format!("\"line\":{line_b}"), EVENT_TIMEOUT)
        .expect("second breakpoint never fired");

    let now = server.last_event();
    let replayed = now.lines().any(|l| l.starts_with("[watchdog]") && l.contains(&id_a));
    assert!(
        !replayed,
        "a rescue of {id_a} was replayed next to a newer hit on {id_b}, so a live suspension reads as \
         one that is already over:\n{now}"
    );

    server.panic_reset();
}

/// TEST-4: `JDWP_WATCHDOG_SECS=0` disables the watchdog — documented behaviour that was unverified.
/// A suspended VM must stay suspended (the probe stops ticking and stays stopped).
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn watchdog_zero_disables_the_auto_resume() {
    let Some(jdk) = jdk_or_skip("watchdog_zero_disables_the_auto_resume") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    server.attach(probe.port);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let line = probe_line(&probe_source("WatchProbe"), "counter = counter + 1;");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in bumpCounter never fired");

    let frozen_at = highest_tick(&probe).expect("no tick before suspension");
    // Far longer than the disabled watchdog's old default poll — if it were going to resume, it would
    // have by now. The VM must still be frozen.
    std::thread::sleep(std::time::Duration::from_secs(5));
    let still = highest_tick(&probe).expect("no tick reading");
    assert_eq!(
        still, frozen_at,
        "watchdog=0 must not auto-resume, but the probe advanced from {frozen_at} to {still}"
    );

    server.panic_reset(); // unfreeze the VM ourselves, since the watchdog won't
}

/// TEST-4: the watchdog's resume must clear a pending single-step, or the resume re-fires the step and
/// re-suspends. With only a step armed (the breakpoint cleared), the probe running freely afterwards
/// proves the step was cleared.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn watchdog_clears_a_pending_single_step() {
    let Some(jdk) = jdk_or_skip("watchdog_clears_a_pending_single_step") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "1")]).expect("start server");
    server.attach(probe.port);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let line = probe_line(&probe_source("WatchProbe"), "counter = counter + 1;");
    let set =
        server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
    let bp_id = grab_token(&set, "bp_").expect("no bp id in set reply");
    server.wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT).expect("breakpoint never fired");

    // Arm a step (sets pending_step), then clear the breakpoint so ONLY the step remains as the reason
    // the VM is suspended. Now the watchdog's step-clearing is the only thing that can free the probe.
    server.call("debug.step_over", serde_json::json!({}));
    server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": bp_id}));
    let frozen_at = highest_tick(&probe).unwrap_or(0);

    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at + 2)).is_some(),
        "probe never resumed — the watchdog did not clear the pending step\n  output: {:?}",
        probe.output(),
    );

    server.panic_reset();
}

/// SAFE-1: `debug.disconnect` must leave the JVM RUNNING with nothing armed. A bare disconnect used to
/// drop the session (and its watchdog) without resuming, freezing a VM suspended at a breakpoint
/// forever. Proven, per the TRACE-2 pattern, by the probe's own output resuming after the disconnect.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn disconnect_resumes_and_clears_instead_of_freezing() {
    let Some(jdk) = jdk_or_skip("disconnect_resumes_and_clears_instead_of_freezing") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    // Watchdog disabled, so ONLY the disconnect can rescue the VM — otherwise the watchdog could be
    // what resumes it and the test would pass without disconnect doing its job.
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    let attach = server.attach(probe.port);
    let sid = session_id_from(&attach).expect("no session id");

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let line = probe_line(&probe_source("WatchProbe"), "counter = counter + 1;");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
    server.wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT).expect("breakpoint never fired");
    let frozen_at = highest_tick(&probe).expect("no tick before suspension");

    let bye = server.call("debug.disconnect", serde_json::json!({"session_id": sid}));
    assert_contains_all(
        "disconnect reports it left the VM safe",
        &bye,
        &["Disconnected", "resumed all threads"],
    );

    // The debuggee's own ticks resuming is the only proof the VM was actually left running.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at + 3)).is_some(),
        "probe never resumed after disconnect — the VM was left frozen\n  output: {:?}",
        probe.output(),
    );
}

/// TRACE-3 + TRACE-4: a traced stop point disarms itself after its hit budget, so a hot field can't
/// flood the debuggee; `get_traces` says it stopped and why; and its output can be filtered by id,
/// `since`, and class.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn trace_budget_disarms_and_get_traces_filters() {
    let Some(jdk) = jdk_or_skip("trace_budget_disarms_and_get_traces_filters") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let base = highest_tick(&probe).expect("no tick to count from");

    let set = server.call(
        "debug.set_field_stop",
        serde_json::json!({"class_name": "WatchProbe", "field_name": "counter", "trace": true, "trace_max_hits": 3}),
    );
    assert_contains_all("budget is reported", &set, &["Auto-disarms after 3"]);
    let watch_id = grab_token(&set, "watch_modify_").expect("no watch id");

    // Poll until it disarms itself. `counter` is bumped every tick, so it reaches 3 quickly.
    let mut traces = String::new();
    for _ in 0..50 {
        traces = server.call("debug.get_traces", serde_json::json!({}));
        if traces.contains("stopped recording") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    assert_contains_all(
        "the disarm is announced, so silence isn't read as 'no hits'",
        &traces,
        &["stopped recording", "trace-hit budget"],
    );

    // Exactly the budget was recorded — not more (it kept flooding), not fewer (it stopped early).
    let by_id = server.call("debug.get_traces", serde_json::json!({"bp_id": watch_id}));
    assert_eq!(
        count_trace_records(&by_id),
        3,
        "a budget of 3 must record exactly 3, filtered to this stop point:\n{by_id}"
    );

    // `since` returns only newer records — a poller asking "what's new since seq 2" gets just #3.
    let since2 = server.call("debug.get_traces", serde_json::json!({"since": 2}));
    assert!(since2.contains("#3") && !since2.contains("#1"), "since:2 should show only #3:\n{since2}");
    // class_filter narrows by class substring.
    let by_class = server.call("debug.get_traces", serde_json::json!({"class_filter": "WatchProbe"}));
    assert_eq!(count_trace_records(&by_class), 3, "all 3 records are in WatchProbe:\n{by_class}");
    let none = server.call("debug.get_traces", serde_json::json!({"class_filter": "NoSuchClass"}));
    assert_eq!(count_trace_records(&none), 0, "a non-matching class_filter shows nothing:\n{none}");

    // The watch disarmed itself but is still LISTED as disabled (BP-2) — an automatic disarm must not
    // destroy a definition the user typed, so it stays recoverable in one call.
    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_contains_all("a self-disarmed watch stays listed as disabled", &listed, &[&watch_id, "DISABLED"]);
    // …and the probe keeps running (it never suspended, and now records nothing either).
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 4)).is_some(),
        "probe stopped ticking after a budgeted trace watch"
    );

    // BP-2: re-arming the self-disarmed watch works, keeps the same id, and records again — which is
    // only possible because the definition survived the auto-disarm.
    let on = server
        .call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": watch_id, "enabled": true}));
    assert_contains_all("a self-disarmed watch can be re-armed", &on, &["Re-armed", &watch_id]);
    server.call("debug.get_traces", serde_json::json!({"clear": true}));
    assert!(
        (0..40).any(|_| {
            let got = count_trace_records(&server.call("debug.get_traces", serde_json::json!({}))) > 0;
            if !got {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            got
        }),
        "the re-armed watch never recorded again — its budget was not restored"
    );

    server.panic_reset();
}

/// FILT-1: a thread-filtered exception breakpoint reports throws from only the chosen thread, and the
/// other thread keeps running — proving the filter, and that it composes with trace mode.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn thread_filter_reports_only_the_chosen_thread() {
    let Some(jdk) = jdk_or_skip("thread_filter_reports_only_the_chosen_thread") else { return };
    let probe = Probe::launch(&jdk, "ThreadProbe").expect("launch ThreadProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // Both threads must be running (so both are throwing, and the exception class is loaded) before we
    // can read their ids or arm a filter.
    probe.wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("alpha throw")).expect("alpha never threw");
    probe.wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("beta throw")).expect("beta never threw");

    let alpha = thread_hex_for(&mut server, "alpha-worker").expect("no alpha-worker thread");
    let beta = thread_hex_for(&mut server, "beta-worker").expect("no beta-worker thread");
    assert_ne!(alpha, beta, "the two workers must have distinct ids");

    let set = server.call(
        "debug.set_exception_stop",
        serde_json::json!({
            "class_pattern": "ThreadProbe$FilterException", "caught": true, "uncaught": false,
            "trace": true, "thread_id": alpha,
        }),
    );
    assert_contains_all("filter + trace are reported", &set, &["Thread filter", "trace (non-suspending)"]);

    // Collect a handful of throws.
    let mut traces = String::new();
    for _ in 0..40 {
        traces = server.call("debug.get_traces", serde_json::json!({}));
        if count_trace_records(&traces) >= 3 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    assert!(count_trace_records(&traces) >= 3, "no throws were recorded at all:\n{traces}");

    // Every recorded throw is on the filtered (alpha) thread; the other thread's id never appears.
    let alpha_tag = format!("thread={alpha}");
    let beta_tag = format!("thread={beta}");
    for rec in traces.lines().filter(|l| l.trim_start().starts_with('#')) {
        assert!(rec.contains(&alpha_tag), "a throw was recorded off the filtered thread: {rec}");
        assert!(!rec.contains(&beta_tag), "the beta thread must be filtered out: {rec}");
    }
    // beta must never have been suspended by the filter — it keeps printing.
    let beta_before = probe.output().iter().filter(|l| l.starts_with("beta throw")).count();
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| {
                l.starts_with("beta throw")
                    && l.rsplit(' ')
                        .next()
                        .and_then(|n| n.parse::<usize>().ok())
                        .is_some_and(|n| n > beta_before)
            })
            .is_some(),
        "the unfiltered beta thread stopped printing — it was wrongly suspended"
    );

    server.panic_reset();
}

/// EVAL-4: `&&` / `||` in filter predicates and in breakpoint conditions, with `||` lower precedence
/// than `&&`, short-circuiting, and a failing clause surfaced rather than silently false.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn boolean_operators_in_predicates_and_conditions() {
    let Some(jdk) = jdk_or_skip("boolean_operators_in_predicates_and_conditions") else { return };
    let probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let line = probe_line(&probe_source("DeepProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in DeepProbe.inspect never fired");

    // lines: aa(1,paid) bb(5) cc(2,paid) dd(9) ee(4). `&&`: qty>3 AND unpaid -> bb, dd, ee.
    assert_contains_all(
        "AND predicate",
        &server.evaluate("order.lines[?qty > 3 && paid == false]"),
        &["3 of 5 matched", "Line(bb,5,false)", "Line(dd,9,false)", "Line(ee,4,false)"],
    );
    // `||`: paid OR qty>8 -> aa, cc (paid) + dd (qty 9).
    let or = server.evaluate("order.lines[?paid == true || qty > 8]");
    assert_contains_all(
        "OR predicate",
        &or,
        &["3 of 5 matched", "Line(aa,1,true)", "Line(cc,2,true)", "Line(dd,9,false)"],
    );
    assert!(!or.contains("Line(bb"), "bb is unpaid and qty 5 (<=8), so must not match:\n{or}");

    // Precedence: `a || b && c` is `a || (b && c)`, NOT `(a || b) && c`.
    //   paid==true || qty>3 && paid==false  ==  paid  OR  (qty>3 AND unpaid)  == all five.
    //   the wrong grouping (a||b)&&c would be (paid OR qty>3) AND unpaid == bb,dd,ee == three.
    assert_contains_all(
        "|| is lower precedence than &&",
        &server.evaluate("order.lines[?paid == true || qty > 3 && paid == false]"),
        &["5 of 5 matched"],
    );
    // Parentheses override precedence, giving the other grouping.
    assert_contains_all(
        "parentheses regroup",
        &server.evaluate("order.lines[?(paid == true || qty > 3) && paid == false]"),
        &["3 of 5 matched"],
    );

    // A clause that can't evaluate is surfaced (as errored elements), not silently treated as false.
    // For the paid lines (aa, cc) the second clause runs and errors; the unpaid ones short-circuit.
    assert_contains_all(
        "a broken clause is reported",
        &server.evaluate("order.lines[?paid == true && nosuchfield == 1]"),
        &["errored"],
    );

    // A compound breakpoint CONDITION: n and local are both the loop counter, so n>2 && local>2 first
    // holds at n == 3. Reaching the suspend at all proves both clauses were evaluated, and stopping at
    // the FIRST qualifying iteration proves neither clause is off by one. Swap the plain breakpoint
    // (still suspended from the first hit) for the conditioned one WHILE suspended, then resume so the
    // condition is evaluated afresh on later calls.
    //
    // Which iteration that is depends on how far the loop had already run when we attached: the probe
    // ticks every 150ms, so on a slow or loaded box the plain breakpoint can catch n == 3 or later and
    // the first hit the condition can possibly see is the NEXT one. Hardcoding `n == 3` made this test
    // fail for a reason that has nothing to do with `&&` — so anchor the expectation to the iteration
    // we actually resumed from instead of to the clock.
    let first_n = int_value(&server.evaluate("n")).expect("no int value for n at the plain hit");
    let expect_n = std::cmp::max(3, first_n + 1);

    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    let old_bp = grab_token(&listed, "bp_").expect("no bp to clear");
    server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": old_bp}));
    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "DeepProbe", "line": line, "condition": "n > 2 && local > 2"}),
    );
    server.call("debug.continue", serde_json::json!({}));
    server.call("debug.get_last_event", serde_json::json!({"drain": true})); // discard the first hit

    let hit = server
        .wait_for_event("\"event\":\"breakpoint\"", EVENT_TIMEOUT)
        .expect("compound condition never fired");
    assert!(hit.contains("[suspended] true"), "the compound condition should have suspended: {hit}");
    assert_contains_all(
        &format!(
            "condition held at the first qualifying iteration (n == {expect_n}, resumed from {first_n})"
        ),
        &server.evaluate("n"),
        &[&format!("(int) {expect_n}")],
    );

    server.panic_reset();
}

// ---------------------------------------------------------------------------------------------
// FILT-7 (#91): what a `condition` on a SUSPENDING stop point costs the rest of the JVM.
//
// A conditional stop point is the feature you reach for to reduce noise on a busy shared instance, and
// before this it was the most expensive thing you could arm: `SuspendPolicy::All` on every hit, the
// condition evaluated with the whole VM already stopped, and a `resume_all` when it turned out false.
// So the cost was paid on every hit regardless of the outcome, which is the opposite of what the
// argument is for.
//
// THE ONLY EVIDENCE IS THE PROBE'S OWN STDOUT. The debugger reports success either way — the condition
// works, the right hit suspends, the wrong ones do not — whether or not it froze the world in between.
// `CondProbe`'s ticker is CPU-bound so its tick RATE reads the fraction of the wall clock the VM spent
// running; see the probe for why a sleeping ticker would have been a useless witness.
// ---------------------------------------------------------------------------------------------

/// How many non-matching hits the measured window spans. Big enough that the ratio below is a rate and
/// not a coincidence, small enough that the window is under a second on an idle box.
const COND_WINDOW_HITS: i64 = 120;

/// How long the measured window may take. Deliberately larger than `EVENT_TIMEOUT`, which budgets for one
/// event: this waits for [`COND_WINDOW_HITS`] of them in a row, each costing a condition evaluation, and
/// under a whole suite on a contended box that is minutes rather than seconds.
const COND_WINDOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The floor: one tick per this many non-matching hits, over the window.
///
/// Measured on this tree rather than chosen. Five runs of each arm on JDK 17, over the same 120 hits:
/// **81–119** ticks with the fix, **10–14** with `suspend_policy_for_line` reverted to arm a conditional
/// stop point at `All`. A divisor of 4 puts the floor at 30 — 2.1x above the worst passing-when-broken
/// reading and 2.7x below the worst passing-when-fixed one, which is as much room as this measurement
/// has to give on both sides at once.
const COND_TICK_FLOOR_DIVISOR: i64 = 4;

/// The `n` in `work <n>` — how many times `CondProbe.hot` has been called.
fn work_index(line: &str) -> Option<i64> {
    line.strip_prefix("work ")?.split_whitespace().next()?.parse().ok()
}

/// The highest `work` index the probe has printed so far.
fn highest_work(probe: &Probe) -> Option<i64> {
    probe.output().iter().filter_map(|l| work_index(l)).max()
}

/// FILT-7's first acceptance criterion: non-matching hits must leave the other threads running.
///
/// Asserted as a ratio (ticks per non-matching hit) rather than as an absolute tick count, so the
/// reading is normalised against how fast this machine and this JDK happen to be — both arms measure the
/// same probe over the same number of hits, and only the debugger's behaviour differs between them.
///
/// The second half of the test is the other side of the same coin: when the condition finally DOES hold,
/// the VM must genuinely stop. A fix that simply never suspended would pass the first assertion and be a
/// worse bug than the one being fixed, so the ticker is required to go quiet at the matching hit and to
/// start again after `debug.continue` — which also exercises the suspend depth of 2 the escalation
/// builds (`EventThread` hold + VM-wide suspend), the ADR-0003 case.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_false_condition_holds_one_thread_and_a_true_one_stops_the_vm() {
    let Some(jdk) = jdk_or_skip("a_false_condition_holds_one_thread_and_a_true_one_stops_the_vm") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "CondProbe").expect("launch CondProbe");
    // The watchdog off: this test deliberately leaves the VM suspended at the matching hit, and a rescue
    // would resume it under the assertion that it is stopped.
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    probe.attach(&mut server);
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");

    let line = probe_line(&probe_source("CondProbe"), "// BP1");
    // The match is placed past the measured window on purpose, so every hit inside it is a non-matching
    // one — the case that used to cost a full VM freeze. Only just past it: every hit before the match
    // costs a round trip, so the distance between the two is time this test spends waiting, and a full
    // suite on a contended box is where that turns into a timeout rather than a slow pass.
    let matches_at = COND_WINDOW_HITS + 60;
    let armed = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "CondProbe",
            "line": line,
            "condition": format!("n == {matches_at}"),
        }),
    );
    assert_contains_all("the conditional breakpoint armed", &armed, &["bp_"]);

    probe.send_line("go").expect("cue the worker");
    // Start measuring after the first few hits: the very first one pays for the line table and the
    // variable table, which are cached afterwards, and charging that to the steady-state rate would
    // understate the fix rather than the bug.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| work_index(l).is_some_and(|n| n >= 10))
        .expect("the worker never reached its 10th hit");

    let (work_from, tick_from) = (highest_work(&probe).unwrap_or(0), highest_tick(&probe).unwrap_or(0));
    // Not `EVENT_TIMEOUT`: that budget is for ONE event to arrive, and this waits for 120 of them, each
    // costing a full condition evaluation. On a box running the whole suite that is a different order of
    // magnitude, and a timeout here would read as "the fix does not work" when it means "the box is busy".
    probe
        .wait_for_line(COND_WINDOW_TIMEOUT, |l| {
            work_index(l).is_some_and(|n| n >= work_from + COND_WINDOW_HITS)
        })
        .expect("the worker never finished the measured window");
    let (work_to, tick_to) = (highest_work(&probe).unwrap_or(0), highest_tick(&probe).unwrap_or(0));

    let (hits, ticks) = (work_to - work_from, tick_to - tick_from);
    let floor = hits / COND_TICK_FLOOR_DIVISOR;
    println!("FILT-7: {ticks} ticks across {hits} non-matching hits (floor {floor})");
    assert!(
        ticks >= floor,
        "the OTHER threads were frozen while non-matching hits were evaluated: only {ticks} ticks across \
         {hits} non-matching conditional hits, which is below the floor of {floor}. A conditional stop \
         point is being armed at SuspendPolicy::All, so every hit stops the whole VM to find out that the \
         condition is false.\n  probe tail: {:?}",
        probe.output().iter().rev().take(12).collect::<Vec<_>>(),
    );

    // --- and the matching hit must still stop the VM, exactly as before ---
    // `COND_WINDOW_TIMEOUT` for the same reason as above: the sixty hits between the window and the match
    // are sixty round trips, not one event.
    let hit = server
        .wait_for_event(&format!("\"line\":{line}"), COND_WINDOW_TIMEOUT)
        .expect("the matching hit never surfaced as an event");
    assert_contains_all("the matching hit suspended the VM", &hit, &["[suspended] true"]);
    assert_contains_all(
        "the matching hit is readable exactly as before",
        &server.evaluate("n"),
        &[&format!("(int) {matches_at}")],
    );

    // The probe's own witness that the escalation actually landed: the ticker had a core to itself a
    // moment ago and must now be going nowhere.
    let stopped_at = highest_tick(&probe).unwrap_or(0);
    std::thread::sleep(std::time::Duration::from_millis(400));
    let after = highest_tick(&probe).unwrap_or(0);
    assert!(
        after - stopped_at <= 2,
        "the condition HELD but the VM kept running: the ticker advanced {} ticks while the reply said \
         [suspended] true. An escalation that reports a freeze it did not perform is worse than the \
         freeze-everything bug it replaced.",
        after - stopped_at
    );

    // ADR-0003: the escalation leaves the hit thread suspended TWICE (its `EventThread` hold plus the
    // VM-wide suspend), so this is also a live check that `continue` clears the depth it built.
    //
    // The stop point is CLEARED first, and that is not tidying. `CondProbe`'s worker calls the conditioned
    // line in a tight loop, so a breakpoint left armed across the resume re-fires immediately and keeps
    // re-firing — which is the *disarm*-honesty case TODO.md deliberately keeps out of the resume-honesty
    // matrix, because `continue` may legitimately re-freeze there. Leaving it armed made this assertion
    // fail on a loaded box against a VM that had genuinely been resumed, which is the confound rather than
    // the bug. Resume honesty for this state is asserted properly by the matrix's `ConditionEscalated`
    // arm, which uses a one-shot breakpoint for exactly this reason.
    let bp_id = grab_token(&armed, "bp_").expect("no bp id in the arming reply");
    server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": bp_id}));
    let cont = server.call("debug.continue", serde_json::json!({}));
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > after + 4)).is_some(),
        "the VM never resumed after the escalation — `continue` said: {cont}\n  probe tail: {:?}",
        probe.output().iter().rev().take(12).collect::<Vec<_>>(),
    );

    server.panic_reset();
}

/// FILT-7's other half: `trace:true` + `condition` must be **unchanged**.
///
/// It was already the safe path — `EventThread` policy, condition evaluated in `try_record_trace`, and a
/// condition-skipped hit not charged to the trace budget (ADR-0002) — and the risk of #91 was regressing
/// it while moving the *suspending* path onto the same policy. An earlier audit claimed this path was
/// broken and was wrong; nothing here rebuilds it, so this test exists to keep it that way.
///
/// Three properties, and the first two would both look like success without the others: that the matching
/// hit is recorded, that the non-matching ones are NOT (so a condition still filters), and that the budget
/// is spent only on what was recorded — arm 3, match once, expect 2 left rather than 0.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_traced_conditional_stop_point_records_only_matches_and_never_suspends() {
    let Some(jdk) = jdk_or_skip("a_traced_conditional_stop_point_records_only_matches_and_never_suspends")
    else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "CondProbe").expect("launch CondProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    probe.attach(&mut server);
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");

    let line = probe_line(&probe_source("CondProbe"), "// BP1");
    server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "CondProbe",
            "line": line,
            "condition": "n == 7",
            "trace": true,
            "trace_max_hits": 3,
        }),
    );
    probe.send_line("go").expect("cue the worker");

    let traces = server
        .wait_for_traces("CondProbe.hot", EVENT_TIMEOUT)
        .expect("the traced conditional stop point never recorded the matching hit");
    assert_contains_all("the matching hit was recorded with its frame", &traces, &["n=(int) 7"]);
    assert_eq!(
        traces.matches("CondProbe.hot").count(),
        1,
        "a condition must still filter in trace mode — every hit was recorded, not only n == 7:\n{traces}"
    );

    // ADR-0002's contract, and the half a condition makes visible: only a RECORDED hit is charged, so a
    // budget of 3 survives dozens of skipped hits with 2 left.
    assert_contains_all(
        "a condition-skipped hit is not charged to the budget",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &["2 hit(s) left"],
    );

    // And it never suspended anything — read off the probe, which is still ticking, rather than off the
    // absence of an event.
    let before = highest_tick(&probe).unwrap_or(0);
    assert!(
        probe
            .wait_for_line(std::time::Duration::from_secs(5), |l| tick_index(l)
                .is_some_and(|n| n > before + 4))
            .is_some(),
        "a traced conditional stop point suspended the VM\n  probe tail: {:?}",
        probe.output().iter().rev().take(12).collect::<Vec<_>>(),
    );

    server.panic_reset();
}

/// How a `VirtualMachine.Suspend` can go wrong, and why both arms are needed to pin one branch each.
#[derive(Debug, Clone, Copy)]
enum SuspendFault {
    /// The debuggee never performs it and answers with its own error — the ordinary shape of a refusal,
    /// and the world in which "the application is STILL RUNNING" is the true thing to say.
    Refused,
    /// The debuggee performs it and the ANSWER is an error — a lying connection. The application really
    /// is stopped, so a reply that deduced "it failed, therefore it is running" would be wrong here, and
    /// this arm is the reason the fix measures the second half instead of deducing it.
    Misreported,
}

/// FILT-7's honest-failure path: the condition MATCHED and the escalation to a VM-wide suspend did not
/// come back clean.
///
/// The state has no precedent in this codebase — the stop point the caller armed did fire, one thread is
/// held, and the application may or may not be stopped — and each half on its own is a lie. Reported as a
/// normal suspending hit it sends the caller to `debug.get_stack` on a moving target; reported as a bare
/// failure it throws away the one hit they were waiting for. So both, or nothing.
///
/// **The two arms taught the fix, and are kept because reverting either half is invisible to the other.**
/// The first cut of this test used [`SuspendFault::Misreported`] alone and asserted the wording it
/// expected ("STILL RUNNING") — and failed against the probe, which had stopped ticking, because
/// `FaultRelay::start` rewrites the debuggee's REPLY and the suspend had landed regardless. A
/// `suspend_all` that returns an error does not prove the application is running, so the fix now measures
/// that against another thread's suspend count rather than deducing it from the error: ADR-0003's rule
/// ("ask the JVM, do not assume") pointed at a suspend instead of a resume. What both arms assert is the
/// invariant that holds whichever world the connection is in:
///
/// > the reply names the match, and whatever it says about the VM, the probe agrees.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_matched_condition_that_cannot_freeze_the_vm_reports_both_facts() {
    let Some(jdk) = jdk_or_skip("a_matched_condition_that_cannot_freeze_the_vm_reports_both_facts") else {
        return;
    };
    for fault in [SuspendFault::Refused, SuspendFault::Misreported] {
        assert_failed_escalation_is_honest(&jdk, fault);
    }
}

/// Drive one failed-escalation arm and assert the invariant, naming the arm in every failure so a break
/// says which of the two worlds it broke in.
fn assert_failed_escalation_is_honest(jdk: &Jdk, fault: SuspendFault) {
    // VirtualMachine.Suspend — set 1, command 8. Nothing else in a session issues it on the hit path, so
    // neither instrument disturbs anything but the escalation itself.
    let mut probe = Probe::launch(jdk, "CondProbe").expect("launch CondProbe");
    let relay = match fault {
        SuspendFault::Refused => FaultRelay::start_refusing(probe.port, vec![(1, 8)]),
        SuspendFault::Misreported => FaultRelay::start(probe.port, vec![(1, 8, Fault::Error(113))]),
    }
    .expect("start the fault relay");
    // The watchdog off: it would rescue the held thread mid-assertion, and what is under test is what the
    // reply says at the moment of the hit, not whether anything eventually cleans up.
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    server.attach(relay.port);
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).is_some(),
        "{fault:?}: probe never ticked, so the relay may not be passing traffic at all\n  output: {:?}",
        probe.output()
    );

    let line = probe_line(&probe_source("CondProbe"), "// BP1");
    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "CondProbe", "line": line, "condition": "n == 5"}),
    );
    probe.send_line("go").expect("cue the worker");

    let hit = server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .unwrap_or_else(|| panic!("{fault:?}: the matching hit never surfaced as an event"));
    // Half one, and the half a bare failure report would drop: the stop point the caller armed DID fire.
    assert_contains_all(
        &format!("{fault:?}: a failed escalation still reports the match"),
        &hit,
        &["[escalation]", "MATCHED"],
    );

    // Half two, checked against the debuggee rather than taken from the reply. The ticker is CPU-bound,
    // so whether it advances is the VM's own answer to the question the reply just claimed to settle.
    let said_running = hit.contains("[suspended] false");
    let before = highest_tick(&probe).unwrap_or(0);
    let advanced = probe
        .wait_for_line(std::time::Duration::from_secs(3), |l| tick_index(l).is_some_and(|n| n > before + 4))
        .is_some();
    assert_eq!(
        said_running,
        advanced,
        "INVARIANT VIOLATED — {fault:?}: the reply and the debuggee disagree about whether the \
         application is running after a failed escalation (reply said running: {said_running}, probe \
         advanced: {advanced}).\n  said: {hit}\n  probe tail: {:?}",
        probe.output().iter().rev().take(12).collect::<Vec<_>>(),
    );

    // The promise a failed escalation keeps either way: the hit thread was never released around it, so
    // the frame that satisfied the condition is the frame that is still there to read.
    assert_contains_all(
        &format!("{fault:?}: the hit thread is still held and readable"),
        &server.evaluate("n"),
        &["(int) 5"],
    );

    server.panic_reset();
}

/// BP-1: `toggle_stop_point` silences and re-arms a breakpoint without losing its definition. Tested
/// on a trace breakpoint so the probe never freezes: disabled -> no new snapshots; re-enabled -> they
/// resume.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn toggle_stop_point_disables_and_rearms() {
    let Some(jdk) = jdk_or_skip("toggle_stop_point_disables_and_rearms") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let line = probe_line(&probe_source("WatchProbe"), "counter = counter + 1;");
    let set = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "WatchProbe", "line": line, "trace": true}),
    );
    let bp_id = grab_token(&set, "bp_").expect("no bp id");

    // It fires while enabled.
    assert!(
        (0..40).any(|_| {
            let got = count_trace_records(&server.call("debug.get_traces", serde_json::json!({}))) > 0;
            if !got {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            got
        }),
        "the trace breakpoint never recorded while enabled"
    );

    // Disable: the JDWP request is cleared but the definition kept.
    let off =
        server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": bp_id, "enabled": false}));
    assert_contains_all("disable keeps the definition", &off, &["Disabled", "re-arm"]);
    assert_contains_all(
        "a disabled breakpoint stays listed",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &[&bp_id, "DISABLED"],
    );

    // Nothing new is recorded while disabled.
    server.call("debug.get_traces", serde_json::json!({"clear": true}));
    std::thread::sleep(std::time::Duration::from_millis(800));
    assert_eq!(
        count_trace_records(&server.call("debug.get_traces", serde_json::json!({}))),
        0,
        "a disabled breakpoint must not fire"
    );

    // Re-enable: re-armed at the same location (with a fresh id), and snapshots resume.
    let on =
        server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": bp_id, "enabled": true}));
    assert_contains_all("enable re-arms", &on, &["Re-armed"]);
    assert!(
        (0..40).any(|_| {
            let got = count_trace_records(&server.call("debug.get_traces", serde_json::json!({}))) > 0;
            if !got {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            got
        }),
        "re-enabling did not re-arm the breakpoint"
    );

    // BP-3: the id is STABLE across the round trip, so the id the caller holds keeps working. It used
    // to be re-keyed to `bp_<new request id>`, silently breaking any stored id.
    assert_contains_all(
        "the id survives disable → enable",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &[&bp_id],
    );
    let again =
        server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": bp_id, "enabled": false}));
    assert_contains_all("the original id still resolves after a re-arm", &again, &["Disabled", &bp_id]);

    server.panic_reset();
}

/// SAFE-4: `debug.pause` used to suspend every thread and record nothing, so the watchdog — the whole
/// reason attaching to a shared JVM is defensible — never fired and the VM stayed frozen. A manual pause
/// must be covered too, and reported as a pause rather than as a stop point the watchdog couldn't find.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn watchdog_rescues_a_manual_pause() {
    let Some(jdk) = jdk_or_skip("watchdog_rescues_a_manual_pause") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "1")]).expect("start server");
    server.attach(probe.port);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");

    // No breakpoint anywhere — the freeze comes purely from debug.pause.
    let paused = server.call("debug.pause", serde_json::json!({}));
    assert_contains_all("pause says the watchdog covers it", &paused, &["paused", "watchdog"]);
    let frozen_at = highest_tick(&probe).expect("no tick before the pause");

    // The probe's own ticks resuming is the only proof the VM was really let go.
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at + 3))
            .is_some(),
        "a manual debug.pause was never auto-resumed — the VM was left frozen with no watchdog cover\n  output: {:?}",
        probe.output(),
    );

    // And it is reported honestly: a manual pause has no stop point, so the note must say that rather
    // than claim it failed to identify one.
    let note = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_contains_all(
        "the pause resume is reported as a pause",
        &note,
        &["watchdog auto-resumed", "debug.pause"],
    );
    assert!(
        !note.contains("could not identify"),
        "a manual pause must not be reported as an unidentifiable stop point: {note}"
    );
}

/// SAFE-5: the watchdog identified the offending stop point from the newest buffered event, which
/// `get_last_event {drain:true}` erases — so the polling caller `drain` exists for was exactly the one
/// whose freeze never got disarmed. The cause is recorded at suspension time now.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn watchdog_disarms_even_after_the_events_were_drained() {
    let Some(jdk) = jdk_or_skip("watchdog_disarms_even_after_the_events_were_drained") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    // 5s, not 1s: the drain has to land BEFORE the watchdog fires, or the watchdog reads the event it
    // was always going to read and the test proves nothing. (Measured: with 1s it passed even against
    // the old `events.back()` derivation, because the resume raced ahead of the drain.)
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "5")]).expect("start server");
    server.attach(probe.port);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let line = probe_line(&probe_source("WatchProbe"), "counter = counter + 1;");
    let set =
        server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
    let bp_id = grab_token(&set, "bp_").expect("no bp id");
    server.wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT).expect("breakpoint never fired");
    let frozen_at = highest_tick(&probe).expect("no tick before suspension");

    // Read AND DRAIN the events — the normal polling pattern EVT-1 added `drain` for. This is what used
    // to erase the watchdog's only record of which request froze the VM.
    let drained = server.call("debug.get_last_event", serde_json::json!({"drain": true}));
    assert!(drained.contains("breakpoint"), "expected the breakpoint hit before draining: {drained}");

    // The VM must be resumed…
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at)).is_some(),
        "probe never resumed after the watchdog window\n  output: {:?}",
        probe.output(),
    );

    // …and STAY resumed. This is the assertion that actually discriminates: if the watchdog resumed
    // without disarming (because the drain erased the only record of the offender), the still-armed
    // breakpoint re-freezes within ~150ms and the probe sits still until the *next* watchdog cycle.
    // Checking "was it disarmed" alone can't tell the two apart — a second cycle eventually disarms it
    // and the listing looks identical. Tick rate over a window shorter than one watchdog period can.
    let resumed_at = highest_tick(&probe).expect("no tick after the resume");
    std::thread::sleep(std::time::Duration::from_secs(3));
    let after = highest_tick(&probe).expect("no tick reading");
    assert!(
        after - resumed_at > 5,
        "the probe advanced only {} tick(s) in 3s after the watchdog resume — it re-froze, so the \
         offending breakpoint was resumed but never disarmed (SAFE-5)",
        after - resumed_at,
    );

    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_contains_all(
        "the offender is still identified and disabled after a drain",
        &listed,
        &[&bp_id, "DISABLED", "watchdog auto-resumed"],
    );

    server.panic_reset();
}

/// SAFE-6: read-only is enforced at the invocation boundary, so the paths a text-level guard missed —
/// `toString()` rendering, a `List` subscript, a condition/`trace_expr` — are all refused too.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn read_only_blocks_every_invocation_path() {
    let Some(jdk) = jdk_or_skip("read_only_blocks_every_invocation_path") else { return };
    let probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start_with_env(&[("JDWP_READONLY", "1")]).expect("start server");
    server.attach(probe.port);

    let line = probe_line(&probe_source("DeepProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in DeepProbe.inspect never fired");

    // 1. toString() rendering. `order` has a `toString()`, and this expression contains no parentheses,
    //    so the old text-level guard let it through and the debuggee ran arbitrary code. It must now
    //    render from the type name and id instead — no invocation.
    let obj = server.evaluate("order");
    assert!(
        obj.contains("(id=0x"),
        "a read-only object render must fall back to Type (id=0x…) rather than invoking toString(): {obj}"
    );

    // 2. A List subscript invokes List.get(int) — also parenthesis-free, also previously missed.
    let sub = server.evaluate("order.lines[0]");
    assert_contains_all("a List subscript is refused", &sub, &["Read-only"]);

    // 3. An explicit call is still refused (it always was).
    assert_contains_all(
        "an explicit call is refused",
        &server.evaluate("order.lines[0].getQty()"),
        &["Read-only"],
    );

    // 4. Reads needing no invocation keep working — the honest cost is shallower output, not no output.
    assert_contains_all("a field read still works", &server.evaluate("order.status"), &["\"OPEN\""]);
    assert_contains_all("an array index still works", &server.evaluate("order.numbers[2]"), &["(int) 3"]);
    assert_contains_all(
        "a nested field read still works",
        &server.evaluate("order.customer.name"),
        &["\"Ana\""],
    );
    assert_contains_all(
        "get_stack still works",
        &server.call("debug.get_stack", serde_json::json!({})),
        &["inspect"],
    );

    // 5. An invoking condition / trace_expr is refused at ARM time, so it fails once where the caller is
    //    looking instead of silently on every hit inside the event pump.
    let cond = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "DeepProbe", "line": line, "condition": "order.getTotal() > 1"}),
    );
    assert_contains_all("an invoking condition is refused at arm time", &cond, &["Read-only", "condition"]);
    let texpr = server.call(
        "debug.set_field_stop",
        serde_json::json!({"class_name": "DeepProbe", "field_name": "threshold", "trace": true, "trace_expr": "order.toString()"}),
    );
    assert_contains_all(
        "an invoking trace_expr is refused at arm time",
        &texpr,
        &["Read-only", "trace_expr"],
    );

    // 6. Writes are refused at the wire too, not just by the handler.
    assert_contains_all(
        "set_value is refused",
        &server.call("debug.set_value", serde_json::json!({"target": "order.status", "value": "\"X\""})),
        &["Read-only"],
    );
    assert_contains_all("the write really didn't happen", &server.evaluate("order.status"), &["\"OPEN\""]);

    server.panic_reset();
}

/// BP-3: a deferred breakpoint is listed, so toggling one must explain itself rather than claiming the
/// id doesn't exist.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn toggling_a_deferred_breakpoint_explains_itself() {
    let Some(jdk) = jdk_or_skip("toggling_a_deferred_breakpoint_explains_itself") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // A class the probe will never load, so the breakpoint stays deferred.
    let set = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "com.example.NeverLoaded", "line": 10}),
    );
    assert_contains_all("the breakpoint is deferred", &set, &["Deferred", "bp_"]);
    let bp_id = grab_token(&set, "bp_").expect("no bp id in deferred reply");

    // It IS listed, so "not found" would be a lie.
    assert_contains_all(
        "a deferred breakpoint is listed",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &[&bp_id],
    );

    let toggled =
        server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": bp_id, "enabled": false}));
    assert_contains_all(
        "toggling a deferred breakpoint names its deferred state",
        &toggled,
        &["deferred", "NeverLoaded"],
    );
    assert!(
        !toggled.contains("not found"),
        "a listed breakpoint must never be reported as not found: {toggled}"
    );

    server.panic_reset();
}

/// SETF-2: `set_value` can copy a live reference (`this.a = other.b`), not just a literal — validating
/// the source's runtime type against the target's declared type, and refusing a mismatch.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn set_value_copies_a_live_reference_and_refuses_a_mismatch() {
    let Some(jdk) = jdk_or_skip("set_value_copies_a_live_reference_and_refuses_a_mismatch") else { return };
    let probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let line = probe_line(&probe_source("DeepProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in DeepProbe.inspect never fired");

    // Copy a live String reference: order.status (a String field) <- order.customer.name ("Ana").
    let copied = server.call(
        "debug.set_value",
        serde_json::json!({"target": "order.status", "value": "order.customer.name"}),
    );
    assert!(!copied.contains("not a literal"), "an expression value must be accepted now: {copied}");
    assert_contains_all("the live value was copied", &server.evaluate("order.status"), &["\"Ana\""]);

    // A type-incompatible source is refused, naming both types (Customer field <- String value).
    let refused = server
        .call("debug.set_value", serde_json::json!({"target": "order.customer", "value": "order.status"}));
    assert_contains_all("a mismatched reference is refused", &refused, &["mismatch", "java.lang.String"]);

    // Literals still work unchanged.
    server.call("debug.set_value", serde_json::json!({"target": "order.status", "value": "\"DONE\""}));
    assert_contains_all("literals still work", &server.evaluate("order.status"), &["\"DONE\""]);

    server.panic_reset();
}

/// SAFE-3: a read-only session refuses mutation (`set_value`, `force_return`, method invocation) while
/// still allowing reads, and is flagged in `list_sessions`. A guard against accident, not a security
/// boundary.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn read_only_refuses_mutation_but_allows_reads() {
    let Some(jdk) = jdk_or_skip("read_only_refuses_mutation_but_allows_reads") else { return };
    let probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start_with_env(&[("JDWP_READONLY", "1")]).expect("start server");
    server.attach(probe.port);

    assert_contains_all(
        "read-only is flagged in list_sessions",
        &server.call("debug.list_sessions", serde_json::json!({})),
        &["read-only"],
    );

    let line = probe_line(&probe_source("DeepProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in DeepProbe.inspect never fired");

    // Reads that need no invocation still work.
    assert_contains_all("a field read still works", &server.evaluate("order.status"), &["\"OPEN\""]);
    assert_contains_all("an array index still works", &server.evaluate("order.numbers[2]"), &["(int) 3"]);
    assert_contains_all(
        "get_stack still works",
        &server.call("debug.get_stack", serde_json::json!({})),
        &["inspect"],
    );

    // Mutations and invocation are refused.
    assert_contains_all(
        "set_value is refused",
        &server.call("debug.set_value", serde_json::json!({"target": "order.status", "value": "\"X\""})),
        &["Read-only"],
    );
    assert_contains_all(
        "force_return is refused",
        &server.call("debug.force_return", serde_json::json!({"value": "true"})),
        &["Read-only"],
    );
    assert_contains_all(
        "a method call in evaluate is refused",
        &server.evaluate("order.lines[0].getQty()"),
        &["Read-only"],
    );
    // And the value is unchanged, since set_value never ran.
    assert_contains_all("the write really didn't happen", &server.evaluate("order.status"), &["\"OPEN\""]);

    server.panic_reset();
}

/// SAFE-7: JDWP counts suspends, so a second suspend needs a second resume. `debug.pause` is therefore
/// idempotent, and `debug.continue` clears whatever depth exists — otherwise "pause twice" needed two
/// continues, and the watchdog's single resume left the VM frozen while reporting it rescued.
///
/// Verified on a real JVM before the fix: two pauses then one continue left the probe at 0 ticks.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn pause_is_idempotent_and_continue_clears_any_suspend_depth() {
    let Some(jdk) = jdk_or_skip("pause_is_idempotent_and_continue_clears_any_suspend_depth") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    // Watchdog off: this test is about the tools' own resume arithmetic, not the rescue.
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    server.attach(probe.port);
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");

    server.call("debug.pause", serde_json::json!({}));
    // The second pause must be a no-op, not a second suspend.
    let second = server.call("debug.pause", serde_json::json!({}));
    assert_contains_all("a second pause is refused as a no-op", &second, &["Already suspended", "no-op"]);

    let frozen_at = highest_tick(&probe).expect("no tick before the pause");
    server.call("debug.continue", serde_json::json!({}));

    // ONE continue must be enough. Before the fix the probe sat at 0 ticks here.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at + 3)).is_some(),
        "one debug.continue did not resume the VM after two pauses — suspends stacked up\n  output: {:?}",
        probe.output(),
    );
}

/// SAFE-7: pausing while already stopped at a breakpoint must not overwrite the `StopPoint` cause with
/// `ManualPause` — doing so lost the SAFE-2 disarm, so the watchdog resumed and the breakpoint re-froze
/// the VM on the next hit.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn pausing_at_a_breakpoint_keeps_the_disarm_target() {
    let Some(jdk) = jdk_or_skip("pausing_at_a_breakpoint_keeps_the_disarm_target") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "5")]).expect("start server");
    server.attach(probe.port);
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");

    let line = probe_line(&probe_source("WatchProbe"), "counter = counter + 1;");
    let set =
        server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
    let bp_id = grab_token(&set, "bp_").expect("no bp id");
    server.wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT).expect("breakpoint never fired");

    // Pause while already suspended at the breakpoint. This is the call that used to clobber the cause.
    server.call("debug.pause", serde_json::json!({}));
    let frozen_at = highest_tick(&probe).expect("no tick before suspension");

    // The watchdog must still disarm the breakpoint (not just resume), and the VM must STAY running —
    // a lost disarm shows up as the probe re-freezing within ~150ms of the resume.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at)).is_some(),
        "the VM was never resumed\n  output: {:?}",
        probe.output(),
    );
    let resumed_at = highest_tick(&probe).expect("no tick after the resume");
    std::thread::sleep(std::time::Duration::from_secs(3));
    let after = highest_tick(&probe).expect("no tick reading");
    assert!(
        after - resumed_at > 5,
        "the probe advanced only {} tick(s) in 3s — it re-froze, so pausing at a breakpoint lost the \
         disarm target (SAFE-7)",
        after - resumed_at,
    );
    assert_contains_all(
        "the breakpoint was still identified and disabled",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &[&bp_id, "DISABLED"],
    );

    server.panic_reset();
}

/// BP-4: re-arming re-resolves the location by name, so a stop point whose class is no longer loaded is
/// reported as such rather than re-armed against a stale JDWP id.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn rearming_reresolves_by_name_and_reports_a_missing_class() {
    let Some(jdk) = jdk_or_skip("rearming_reresolves_by_name_and_reports_a_missing_class") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");

    // A traced watchpoint, so nothing freezes while we toggle it.
    let set = server.call(
        "debug.set_field_stop",
        serde_json::json!({"class_name": "WatchProbe", "field_name": "counter", "trace": true}),
    );
    let watch_id = grab_token(&set, "watch_modify_").expect("no watch id");

    // Disable then re-arm: the re-arm must re-resolve WatchProbe.counter by name and fire again.
    server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": watch_id, "enabled": false}));
    server.call("debug.get_traces", serde_json::json!({"clear": true}));
    let on = server
        .call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": watch_id, "enabled": true}));
    assert_contains_all("re-armed by name", &on, &["Re-armed", "WatchProbe.counter"]);
    assert!(
        (0..40).any(|_| {
            let got = count_trace_records(&server.call("debug.get_traces", serde_json::json!({}))) > 0;
            if !got {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            got
        }),
        "the re-armed watchpoint never fired — re-resolution by name failed"
    );

    // A deferred breakpoint's class never loads; arming it is fine, but its class genuinely isn't there,
    // which is the state BP-4 says must be reported rather than guessed at.
    let deferred = server
        .call("debug.set_line_stop", serde_json::json!({"class_pattern": "com.example.GoneAway", "line": 3}));
    assert_contains_all("still deferred", &deferred, &["Deferred"]);

    server.panic_reset();
}

// ---------------------------------------------------------------------------------------------
// The resume-honesty invariant
//
// Every safety bug in this project so far has been the same shape: a resume path was tested in the
// one state its author had in mind, and broke in a state nobody enumerated.
//
//   SAFE-1  disconnect, from "suspended at a breakpoint"     → froze the JVM forever
//   SAFE-4  the watchdog, from "suspended by debug.pause"    → never fired at all
//   SAFE-7  any resume, from "suspended twice"               → reported a rescue it never made
//
// Each was fixed with its own test, and each time the *next* review found another state. So this
// asserts the invariant itself rather than another happy path:
//
//   after any resume path, from any suspended state, the VM is genuinely running —
//   or the reply said out loud that it isn't.
//
// The dangerous half is the silent one. A tool that admits "still suspended" is merely unhelpful; a
// tool that claims success while the debuggee is frozen is what actually hurt, three times. Verified
// load-bearing by reverting SAFE-1/SAFE-4/SAFE-7 in turn and watching the matrix name the exact
// (state, path) pair that broke.
//
// SCOPE, stated so it isn't mistaken for broader than it is: this covers *resume* honesty, not *disarm*
// honesty. SAFE-2/SAFE-5 were bugs where the VM genuinely resumed but the offending stop point stayed
// armed and re-froze it — invisible here, because these cases use a one-shot breakpoint that cannot fire
// twice. That half is asserted by `watchdog_disarms_even_after_the_events_were_drained` and
// `pausing_at_a_breakpoint_keeps_the_disarm_target`, which measure the probe's tick *rate* after a
// rescue. A future review wanting to fold the two together should add a repeating-breakpoint state whose
// expectation differs per path (`continue` may legitimately re-freeze; a rescue path may not).
// ---------------------------------------------------------------------------------------------

/// A way to leave the VM suspended. Each entry is a state that has broken something historically.
#[derive(Debug, Clone, Copy)]
enum Freeze {
    /// One-shot breakpoint hit (`hit_count: 1`, so it expires and can't re-freeze on resume).
    Breakpoint,
    /// A manual `debug.pause` — the state SAFE-4 found unguarded.
    Pause,
    /// A pause **on top of** a breakpoint: suspend depth 2, which SAFE-7 showed one resume can't clear.
    BreakpointThenPause,
    /// A breakpoint hit whose events were then drained — the state SAFE-5 found.
    BreakpointDrained,
    /// Suspended at the end of a single step, so a pending step request is armed.
    Step,
    /// FILT-7's escalation: a CONDITIONAL breakpoint whose condition held, so the debugger — not the JVM
    /// — issued the VM-wide suspend, on top of the hold the event's own `EventThread` policy had already
    /// taken on the hit thread.
    ///
    /// New to the axis with #91, and it belongs here for the reason the axis exists: it is a way of
    /// leaving the VM suspended that no resume path was written against. It arrives at suspend depth 2 on
    /// the hit thread the way `BreakpointThenPause` does, but by a route with no `debug.pause` in it — so
    /// a rescue that re-derived the depth from the tools it had seen called would be wrong here and right
    /// there, which is exactly the shape of every bug this matrix has caught.
    ConditionEscalated,
}

/// A way we claim to un-freeze it.
#[derive(Debug, Clone, Copy)]
enum Resume {
    Continue,
    Panic,
    Watchdog,
    /// `debug.disconnect` — the SAFE-1 case, and the one whose name most implies it is safe.
    Disconnect,
}

impl Freeze {
    const ALL: [Self; 6] = [
        Self::Breakpoint,
        Self::Pause,
        Self::BreakpointThenPause,
        Self::BreakpointDrained,
        Self::Step,
        Self::ConditionEscalated,
    ];
}

/// Drive one (freeze, resume) pair and assert the invariant. Panics with the offending combination
/// named, so a failure says which state broke which path.
fn assert_resume_is_honest(jdk: &Jdk, freeze: Freeze, resume: Resume) {
    // The watchdog must be ON for its own case and OFF for the others — otherwise it would rescue a
    // broken `continue`/`panic` and the test would pass on someone else's work.
    let watchdog = if matches!(resume, Resume::Watchdog) { "1" } else { "0" };
    let mut probe = Probe::launch(jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", watchdog)]).expect("start server");
    // The other test seen failing at attach with `Connection refused` (TEST-21, #56) runs through here —
    // `disconnect_is_honest_from_every_suspended_state` is four of these. `probe.attach` captures what
    // the probe said and whether anything holds the port; `server.attach` cannot.
    probe.attach(&mut server);
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");

    let line = probe_line(&probe_source("WatchProbe"), "counter = counter + 1;");
    // `hit_count: 1` on purpose: the breakpoint expires after one hit, so it cannot re-freeze the VM
    // after a resume. That keeps this test about resume honesty rather than about disarming (see the
    // SCOPE note above). Built per use, since `Server::call` takes its arguments by value.
    let hit_once = || serde_json::json!({"class_pattern": "WatchProbe", "line": line, "hit_count": 1});

    // --- put the VM into the state under test ---
    match freeze {
        Freeze::Pause => {
            server.call("debug.pause", serde_json::json!({}));
        }
        Freeze::Breakpoint | Freeze::BreakpointDrained | Freeze::BreakpointThenPause => {
            server.call("debug.set_line_stop", hit_once());
            server
                .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
                .unwrap_or_else(|| panic!("{freeze:?}: breakpoint never fired"));
            if matches!(freeze, Freeze::BreakpointDrained) {
                server.call("debug.get_last_event", serde_json::json!({"drain": true}));
            }
            if matches!(freeze, Freeze::BreakpointThenPause) {
                server.call("debug.pause", serde_json::json!({}));
            }
        }
        Freeze::Step => {
            server.call("debug.set_line_stop", hit_once());
            server
                .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
                .unwrap_or_else(|| panic!("{freeze:?}: breakpoint never fired"));
            server.call("debug.step_over", serde_json::json!({}));
            server
                .wait_for_event("\"event\":\"step\"", EVENT_TIMEOUT)
                .unwrap_or_else(|| panic!("{freeze:?}: step never landed"));
        }
        Freeze::ConditionEscalated => {
            // A condition that is already true — the probe has ticked, so `counter` is past zero — so the
            // first hit escalates rather than being dropped. Still `hit_count: 1`, so the request expires
            // and this stays a test about resume honesty rather than about disarming.
            server.call(
                "debug.set_line_stop",
                serde_json::json!({
                    "class_pattern": "WatchProbe",
                    "line": line,
                    "hit_count": 1,
                    "condition": "WatchProbe.counter > 0",
                }),
            );
            let hit = server
                .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
                .unwrap_or_else(|| panic!("{freeze:?}: the conditional breakpoint never fired"));
            // The state is only the one under test if the escalation actually took the VM. A failed one is
            // its own case, asserted by `a_matched_condition_that_cannot_freeze_the_vm_reports_both_facts`,
            // and letting it through here would quietly turn this into a test of a running VM.
            assert!(
                hit.contains("[suspended] true"),
                "{freeze:?}: the escalation did not suspend the VM, so this is not the state under \
                 test: {hit}"
            );
        }
    }

    // The debuggee's own output is the only witness that matters — every tool reports success either
    // way, which is precisely how these bugs survived.
    let frozen_at = highest_tick(&probe).unwrap_or(-1);

    // --- apply the resume path under test ---
    let reply = match resume {
        Resume::Continue => server.call("debug.continue", serde_json::json!({})),
        Resume::Panic => server.call("debug.panic", serde_json::json!({})),
        Resume::Disconnect => server.call("debug.disconnect", serde_json::json!({})),
        // Nothing to call: the watchdog's whole point is that it acts without being asked.
        Resume::Watchdog => String::new(),
    };

    // --- the invariant ---
    let advanced =
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at + 2)).is_some();

    // Whatever the path says about itself, gathered after the fact: `continue`/`panic` answer inline,
    // the watchdog leaves its note on `list_stop_points` / `get_last_event`. After a disconnect there is
    // no session left to ask, so the inline reply is all there is.
    let said = if matches!(resume, Resume::Disconnect) {
        reply
    } else {
        format!(
            "{reply}\n{}\n{}",
            server.call("debug.list_stop_points", serde_json::json!({})),
            server.last_event(),
        )
    };
    let admitted_still_stuck = said.contains("STILL suspended");

    assert!(
        advanced || admitted_still_stuck,
        "INVARIANT VIOLATED — {resume:?} from {freeze:?}: the probe did not advance past tick \
         {frozen_at}, and nothing said the VM was still suspended. A resume path reported success \
         while the debuggee stayed frozen.\n  said: {said}\n  probe output: {:?}",
        probe.output(),
    );

    // The other half: it must not cry wolf either. Claiming "still suspended" while the VM is plainly
    // running would send a caller hunting a freeze that isn't there.
    assert!(
        !(advanced && admitted_still_stuck),
        "INVARIANT VIOLATED — {resume:?} from {freeze:?}: the VM resumed, but a reply claimed it was \
         STILL suspended.\n  said: {said}"
    );

    if !advanced {
        // Legitimate but worth seeing in the log: it failed honestly.
        println!("note: {resume:?} from {freeze:?} did not resume, and said so (acceptable)");
    }
}

/// ADR-0003's honest-failure path, reached at last: a resume that cannot free the VM must SAY so.
///
/// This branch is why `resume_all_fully` exists — "resume" and "is it running" are different questions in
/// JDWP, and a watchdog that reported success on the strength of a command returning OK was the SAFE-7 bug.
/// It was also the one path TODO.md's coverage review called **unreachable through the tool's own API**:
/// getting there needs a suspend count that outlives `MAX_RESUME_ATTEMPTS` resumes, and since ADR-0003 made
/// `debug.pause` idempotent, no sequence of this tool's own calls can build one. So the most important
/// failure branch in the codebase had never executed, and the tests around it only ever proved the *success*
/// side.
///
/// `FaultRelay` reaches it by making the JVM lie: every `ThreadReference.SuspendCount` reply comes back as 9,
/// so however many resumes are issued the count never falls. That is exactly the observable shape of the
/// real hazard — something outside this session holding the VM down — without needing a second debugger.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_resume_that_cannot_free_the_vm_reports_it_instead_of_claiming_success() {
    let Some(jdk) = jdk_or_skip("a_resume_that_cannot_free_the_vm_reports_it_instead_of_claiming_success")
    else {
        return;
    };
    let probe = Probe::launch(&jdk, "ExcProbe").expect("launch ExcProbe");

    // A count that never reaches zero, whatever we resume. 9 is above MAX_RESUME_ATTEMPTS (8), so the loop
    // exhausts rather than happening to succeed on its last try.
    let relay = FaultRelay::start(
        probe.port,
        vec![(
            11, // ThreadReference
            12, // SuspendCount
            Fault::Payload(9i32.to_be_bytes().to_vec()),
        )],
    )
    .expect("start fault relay");

    let mut server = Server::start().expect("start server");
    server.attach(relay.port);
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).is_some(),
        "probe never ticked, so the relay may not be passing traffic at all\n  output: {:?}",
        probe.output()
    );

    // Freeze it on a real breakpoint rather than with `debug.pause`. That is not incidental: the verifying
    // resume needs a thread to probe the count of, and it only has one after an event has named one — a
    // bare pause takes the plain `resume_all` path and never asks the question this test is about.
    let src = probe_source("ExcProbe");
    let line = probe_line(&src, "// BP2");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "ExcProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("the breakpoint never fired, so no thread was ever recorded to probe");

    // Every resume path must reach the same verdict: it could not confirm the VM is running, and says so.
    // `debug.panic` also clears the breakpoint, which is what lets the probe run freely afterwards.
    for tool in ["debug.continue", "debug.panic"] {
        let said = server.call(tool, serde_json::json!({}));
        assert!(
            said.contains("STILL suspended"),
            "{tool} must report that the VM is still suspended rather than claiming success — this is the \
             ADR-0003 invariant, and the reply was:\n{said}"
        );
        assert!(
            said.contains("resume(s)"),
            "{tool} should say how many resumes it issued, so the caller knows it tried: {said}"
        );
        // And it must name the next move rather than leaving the caller with a dead end.
        assert!(
            said.contains("debug.continue") || said.contains("debug.panic"),
            "{tool} should point at what to try next: {said}"
        );
    }

    // The lie is confined to the count, so the VM really was resumed underneath — the probe keeps ticking.
    // Worth asserting: it separates "reported honestly" from "actually broke the debuggee".
    let base = highest_tick(&probe).unwrap_or(0);
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base)).is_some(),
        "the probe stopped ticking — the faulted count should not have left the VM frozen\n  output: {:?}",
        probe.output()
    );
}

/// Invariant: `debug.continue` either resumes the VM or says it didn't — from every suspended state.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn continue_is_honest_from_every_suspended_state() {
    let Some(jdk) = jdk_or_skip("continue_is_honest_from_every_suspended_state") else { return };
    for freeze in Freeze::ALL {
        assert_resume_is_honest(&jdk, freeze, Resume::Continue);
    }
}

/// Invariant: `debug.panic` — the escape hatch — either resumes the VM or says it didn't.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn panic_is_honest_from_every_suspended_state() {
    let Some(jdk) = jdk_or_skip("panic_is_honest_from_every_suspended_state") else { return };
    for freeze in Freeze::ALL {
        assert_resume_is_honest(&jdk, freeze, Resume::Panic);
    }
}

/// Invariant: the watchdog either resumes the VM or says it didn't. This is the one that matters most —
/// it acts while nobody is watching, so a false success is invisible until the JVM is found frozen.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn the_watchdog_is_honest_from_every_suspended_state() {
    let Some(jdk) = jdk_or_skip("the_watchdog_is_honest_from_every_suspended_state") else { return };
    for freeze in Freeze::ALL {
        assert_resume_is_honest(&jdk, freeze, Resume::Watchdog);
    }
}

/// Invariant: `debug.disconnect` leaves the VM running from every suspended state. This is SAFE-1's bug
/// generalised — walking away used to freeze the JVM permanently, and it is the tool whose name most
/// suggests it is the safe way out.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn disconnect_is_honest_from_every_suspended_state() {
    let Some(jdk) = jdk_or_skip("disconnect_is_honest_from_every_suspended_state") else { return };
    for freeze in Freeze::ALL {
        assert_resume_is_honest(&jdk, freeze, Resume::Disconnect);
    }
}

/// TEST-6 / BP-4, approximated locally: re-arming after the target class has been **reloaded through a new
/// classloader** — the shape of a redeploy, which is the case BP-4's by-name re-resolution exists for.
///
/// `ReloadProbe` loads `Worker` through a throwaway `URLClassLoader`, exercises it, then drops it and loads
/// it again through a fresh loader. The second copy is a genuinely different reference type with different
/// JDWP ids, so a re-arm that trusted the ids captured at first arm would target a type that no longer
/// exists. What this does NOT reproduce is `WildFly`'s own module/deployment machinery — see issue #13.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn rearming_survives_a_classloader_reload() {
    let Some(jdk) = jdk_or_skip("rearming_survives_a_classloader_reload") else { return };
    let probe = Probe::launch(&jdk, "ReloadProbe").expect("launch ReloadProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // Wait until Worker exists at all (it is compiled and loaded at runtime).
    probe.wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("tick ")).expect("probe never ticked");

    // A traced watchpoint on the reloadable class's own static field, so nothing freezes while we work.
    let set = server.call(
        "debug.set_field_stop",
        serde_json::json!({"class_name": "Worker", "field_name": "calls", "trace": true}),
    );
    assert_contains_all("watchpoint armed on the reloadable class", &set, &["watch_modify_"]);
    let watch_id = grab_token(&set, "watch_modify_").expect("no watch id");

    // It fires against the CURRENT generation.
    assert!(
        (0..40).any(|_| {
            let got = count_trace_records(&server.call("debug.get_traces", serde_json::json!({}))) > 0;
            if !got {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            got
        }),
        "the watchpoint never fired before the reload"
    );

    // Disable it, then wait for the probe to swap classloaders — this is the "disable, redeploy" half.
    server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": watch_id, "enabled": false}));
    let reloaded = probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("reloaded gen "))
        .expect("probe never reloaded its worker class");
    println!("observed: {reloaded}");

    // Re-arm. The stored (declaringType, fieldId) pair now refers to a discarded type; only a by-name
    // re-resolve can find the live one.
    server.call("debug.get_traces", serde_json::json!({"clear": true}));
    let on = server
        .call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": watch_id, "enabled": true}));
    assert!(
        on.contains("Re-armed") || on.contains("not loaded any more"),
        "a re-arm after a reload must either succeed or say the class is gone — got: {on}"
    );

    // If it claimed success, it has to actually work against the NEW generation. A stale-id re-arm that
    // the JVM happens to accept would report success here and then never fire, which is the failure this
    // test exists to catch.
    if on.contains("Re-armed") {
        assert!(
            (0..60).any(|_| {
                let got = count_trace_records(&server.call("debug.get_traces", serde_json::json!({}))) > 0;
                if !got {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                got
            }),
            "the re-armed watchpoint reported success but never fired against the reloaded class — \
             it was re-armed against the discarded type (BP-4)"
        );
    }

    // Whatever happened to the watchpoint, the debuggee must still be running.
    let now = probe.output().iter().filter(|l| l.starts_with("tick ")).count();
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("tick ")).is_some() && now > 0,
        "the probe stopped running during the reload test"
    );

    server.panic_reset();
}

/// DISC-1 (#29): class discovery. The debuggee is the only thing that knows what it loaded, and
/// until this tool existed the caller had to already know every name they wanted to arm.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn list_classes_finds_loaded_types_and_bounds_the_answer() {
    let Some(jdk) = jdk_or_skip("list_classes_finds_loaded_types_and_bounds_the_answer") else { return };
    let probe = eval_probe_running(&jdk);
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // Substring match finds the probe and its nested classes.
    let mine = server.call("debug.list_classes", serde_json::json!({"filter": "EvalProbe"}));
    assert_contains_all("probe classes", &mine, &["EvalProbe", "EvalProbe$Item", "EvalProbe$Task"]);
    // Dotted FQNs, never the `Lpkg/Cls;` form the JVM actually reports — the caller composes against
    // the former and JDWP speaks the latter, which is the whole reason this renders anything.
    assert!(!mine.contains("LEvalProbe"), "signatures must be decoded, not raw JNI: {mine}");

    // A prefix anchors at the start, so it must not behave as a substring.
    let prefixed =
        server.call("debug.list_classes", serde_json::json!({"filter": "java.util.*", "limit": 200}));
    assert!(prefixed.contains("java.util."), "prefix filter found nothing: {prefixed}");
    for line in prefixed.lines().skip(1).filter(|l| !l.starts_with('…')) {
        assert!(
            line.starts_with("java.util."),
            "a prefix filter matched something it should not have: {line}"
        );
    }

    // An unfiltered call is bounded and says so — a page must never read as the whole answer (DUMP-1).
    let bounded = server.call("debug.list_classes", serde_json::json!({"limit": 5}));
    assert_contains_all("bounded listing", &bounded, &["loaded in the VM", "more (raise limit"]);

    // Arrays are excluded by default and available on request.
    let no_arrays = server.call("debug.list_classes", serde_json::json!({"filter": "*[]"}));
    assert!(no_arrays.starts_with("0/0 "), "arrays must be excluded by default: {no_arrays}");
    let arrays = server
        .call("debug.list_classes", serde_json::json!({"filter": "*[]", "include_arrays": true, "limit": 5}));
    assert!(arrays.contains("[]"), "include_arrays must surface array types: {arrays}");

    // Nothing matched must not read as "no such class" — the JVM only knows what it has loaded. Three
    // readings rather than two since SIG-1 (#46): the tool's own spelling of a name is the third thing
    // that can be wrong, and a miss it cannot resolve has to say so instead of picking one.
    let absent = server.call("debug.list_classes", serde_json::json!({"filter": "com.nosuch.*"}));
    assert_contains_all(
        "absent class",
        &absent,
        &["Nothing matched", "not be loaded yet", "no such class", "spelled differently"],
    );
}

/// DISC-2 (#30): a method listing the caller can compose a `debug.evaluate` call from, rather than
/// guessing at parameter lists that overload resolution will then refuse.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn list_methods_renders_java_signatures_and_marks_static() {
    let Some(jdk) = jdk_or_skip("list_methods_renders_java_signatures_and_marks_static") else { return };
    let probe = eval_probe_running(&jdk);
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);
    disc2_method_listing(&mut server);
}

/// `EvalProbe`, launched and **actually running** — not merely accepting a JDWP connection.
///
/// The distinction cost three tests one afternoon (`b64d55d`): attaching proves the agent is listening,
/// which happens before `main` does anything, and every question below is about what the debuggee has
/// *loaded*. `EvalProbe`'s nested types are created by its static initialiser, so the first `work` line is
/// the evidence that `Item`, `Task` and `Subtask` are all in the VM. It matters more here than anywhere
/// else in this file, because a recording taken too early does not fail — it bakes "not loaded" into a
/// cassette and every replay of it is confidently wrong.
///
/// Every `EvalProbe` test whose first call asks about loaded state now comes through here rather than
/// remembering the wait itself: TEST-17 (#49) found `list_classes` and `source` still racing after the same
/// failure had been seen on `list_methods`, so the one-line wait was clearly not something to keep
/// re-deciding per test. The wait lives in [`Probe::launch_running`], which is where the failure text that
/// names the race lives too.
fn eval_probe_running(jdk: &Jdk) -> Probe {
    // `work …` rather than a tick: EvalProbe has no heartbeat, and its first `work` line is printed from
    // inside the loop, after the static initialiser has built `holder`, `task`, `subtask` and `words`.
    Probe::launch_running(jdk, "EvalProbe", |l| l.starts_with("work ")).unwrap_or_else(|e| panic!("{e}"))
}

/// TEST-17 (#49): the readiness wait is all that keeps three discovery tests honest, so prove it works
/// rather than trusting that it does — and prove that losing the race is reported *as* a race.
///
/// The race is normally unreproducible on demand: it needs a runner slow enough that a JVM which is already
/// answering JDWP has still not loaded its main class, which is why this shape gets found in CI and not
/// here. [`Probe::launch_delayed`] manufactures exactly that state, so the two halves below are the real
/// failure and the real fix rather than a simulation of them.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_probe_that_has_not_run_yet_reads_as_a_race_rather_than_an_unloaded_class() {
    let Some(jdk) = jdk_or_skip("a_probe_that_has_not_run_yet_reads_as_a_race_rather_than_an_unloaded_class")
    else {
        return;
    };
    // Long enough to attach and ask a question inside the window, short enough not to pad the suite.
    let probe = Probe::launch_delayed(&jdk, "EvalProbe", std::time::Duration::from_secs(8))
        .expect("launch EvalProbe");

    // The trap in three lines: attaching succeeds against a debuggee that has run no code at all, and the
    // discovery tool then answers correctly and uselessly. This is what the three tests were asserting on.
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);
    let too_early = server.call("debug.list_classes", serde_json::json!({"filter": "EvalProbe"}));
    assert!(too_early.starts_with("0/0 "), "EvalProbe cannot be loaded this early: {too_early}");
    assert!(
        too_early.contains("not be loaded yet"),
        "the tool's answer is correct here, which is the whole problem: {too_early}"
    );

    // So a test that asks for readiness and does not get it has to be told it raced, in those words —
    // otherwise the reader starts in the handler, hunting #46's wrong-answer bug, which is not there.
    let raced = probe
        .wait_until_running(std::time::Duration::from_millis(500), |l| l.starts_with("work "))
        .expect_err("EvalProbe cannot be running yet");
    assert_contains_all(
        "a lost race says so",
        &raced,
        &["EvalProbe", "RACE", "listening, not running", "#49"],
    );

    // And the wait is not just a sleep: it ends when the probe really is running, and the same question
    // asked then gives the three discovery tests the answer they were always meant to assert on.
    probe.wait_until_running(EVENT_TIMEOUT, |l| l.starts_with("work ")).unwrap_or_else(|e| panic!("{e}"));
    let now_loaded = server.call("debug.list_classes", serde_json::json!({"filter": "EvalProbe"}));
    assert_contains_all("loaded once running", &now_loaded, &["EvalProbe", "EvalProbe$Item"]);
}

/// DISC-2's assertions, against whatever is on the other end of `server` — a JVM or a cassette.
///
/// Lifted out of the test above for TEST-12 (#37). The acceptance criterion was that an existing
/// JVM-dependent test be runnable from a cassette *as well*, "proving equivalence rather than asserting
/// it", and the only way to prove it is for both to be literally the same assertions rather than two
/// hand-copied sets that agree today. Three callers now: the probe test above, the record-and-replay test,
/// and the JDK-free replay of the checked-in cassette.
///
/// This one was chosen because everything it asks is a *question about loaded state* — no breakpoints, no
/// suspension, no probe stdout, and therefore no events, which the first cut of cassettes does not replay.
/// It is also the same on JDK 11, 17 and 21: every name it asserts on belongs to `EvalProbe` itself.
///
/// Returns every reply verbatim, so a caller can compare two runs byte for byte instead of trusting that
/// two passes of the same assertions mean the same output.
fn disc2_method_listing(server: &mut Server) -> Vec<String> {
    let m = server.call("debug.list_methods", serde_json::json!({"class_name": "EvalProbe", "limit": 100}));
    // Primitives, arrays and void all render as Java source types.
    assert_contains_all(
        "rendered signatures",
        &m,
        &[
            "static int twice(int)",
            "static java.lang.String greet(java.lang.String)",
            "static void main(java.lang.String[])",
        ],
    );
    assert!(!m.contains("(I)"), "raw JVM descriptors must not leak into the listing: {m}");

    // Every overload appears — comparing their parameter lists side by side is the point.
    assert_eq!(m.matches("pick(").count(), 4, "all four pick overloads should be listed: {m}");
    // The instance overload is the one with no `static` marker.
    assert!(m.contains("java.lang.String pick(int)"), "instance overload missing: {m}");
    // The static initialiser is not callable and not breakable, so it is not listed.
    assert!(!m.contains("<clinit>"), "<clinit> is noise and should be omitted: {m}");

    let twice = server
        .call("debug.list_methods", serde_json::json!({"class_name": "EvalProbe", "name_filter": "twice"}));
    assert_contains_all("name_filter", &twice, &["twice(int)", "name~\"twice\""]);
    assert!(!twice.contains("greet"), "name_filter must exclude non-matches: {twice}");

    // Declared-only by default; the superclass chain is opt-in and attributes each inherited row.
    let own = server.call("debug.list_methods", serde_json::json!({"class_name": "EvalProbe$Subtask"}));
    assert!(!own.contains("void run()"), "run() is declared on Task, not Subtask: {own}");
    let chain = server.call(
        "debug.list_methods",
        serde_json::json!({"class_name": "EvalProbe$Subtask", "inherited": true, "limit": 100}),
    );
    assert_contains_all("inherited walk", &chain, &["void run()", "[from EvalProbe$Task]"]);

    // "Not loaded" and "no such class" are indistinguishable over JDWP, so the reply must say that
    // rather than pick one — and point at the tool that can tell them apart.
    let missing =
        server.call("debug.list_methods", serde_json::json!({"class_name": "com.example.NoSuchThing"}));
    assert_contains_all("unloaded class", &missing, &["is not loaded", "debug.list_classes"]);

    vec![m, twice, own, chain, missing]
}

/// DISC-5 (#53): the other half of the same question — what state a type HOLDS, for a caller who has the
/// type and no instance to expand.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn list_fields_renders_java_declarations_and_marks_static() {
    let Some(jdk) = jdk_or_skip("list_fields_renders_java_declarations_and_marks_static") else { return };
    let probe = eval_probe_running(&jdk);
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);
    disc5_field_listing(&mut server);
}

/// DISC-5's assertions, against whatever is on the other end of `server` — a JVM or a cassette.
///
/// Split out for the same reason as [`disc2_method_listing`], and asking the same *kind* of question: it
/// is all loaded state, so there are no events on the tape and both halves of the pair can run it.
///
/// `EvalProbe` was given three fields for this (a `static final`, an instance `int`, and a `volatile` on
/// `Task`), because a probe made entirely of statics cannot show that statics are listed first, and one
/// with no inherited field cannot show the superclass walk attributing a row.
fn disc5_field_listing(server: &mut Server) -> Vec<String> {
    let f = server.call("debug.list_fields", serde_json::json!({"class_name": "EvalProbe", "limit": 100}));
    assert_contains_all(
        "rendered declarations",
        &f,
        &[
            "static java.lang.String infra",
            "static int base",
            "static java.lang.String[] words",
            "static final int LIMIT",
            "int seq",
        ],
    );
    assert!(!f.contains("Ljava/lang/String;"), "raw JVM descriptors must not leak into a listing: {f}");
    // Statics lead, because those are the ones readable with no instance — the case the tool is for. The
    // probe's single instance field is therefore last, whatever order the class file listed them in.
    let seq_at = f.find("\nint seq").expect("the instance field is listed");
    assert!(
        f.find("static java.lang.String infra").is_some_and(|s| s < seq_at)
            && f.find("static final int LIMIT").is_some_and(|s| s < seq_at),
        "every static must precede the instance field:\n{f}"
    );

    // Instance fields of a type that has only those — and no `static` marker anywhere, which is the
    // distinction the criterion asks for read from the other side.
    let item = server.call("debug.list_fields", serde_json::json!({"class_name": "EvalProbe$Item"}));
    assert_contains_all("instance fields", &item, &["java.lang.String name", "int qty"]);
    assert!(!item.contains("static"), "Item declares no statics, so nothing may be marked one: {item}");

    // Bounded like every other discovery tool: the header counts what matched, and the truncation is
    // loud rather than a short page that reads like the whole answer.
    // The total is read off the unbounded reply rather than written down, so adding a field to the probe
    // does not silently make this test assert the wrong arithmetic.
    let declared: usize = f
        .split_once('/')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("the header counts shown/matched: {f}"));
    let capped = server.call("debug.list_fields", serde_json::json!({"class_name": "EvalProbe", "limit": 2}));
    assert!(
        capped.starts_with(&format!("2/{declared} field(s) on EvalProbe")),
        "a capped listing states both numbers: {capped}"
    );
    let hidden = format!("… +{} more", declared - 2);
    assert_contains_all("loud truncation", &capped, &[hidden.as_str(), "raise limit"]);

    let filtered =
        server.call("debug.list_fields", serde_json::json!({"class_name": "EvalProbe", "name_filter": "in"}));
    assert_contains_all("name_filter", &filtered, &["infra", "name~\"in\""]);
    assert!(!filtered.contains("words"), "name_filter must exclude non-matches: {filtered}");

    // Declared-only by default. `Subtask` declares nothing at all, which is a CORRECT answer that reads
    // exactly like a failed lookup — so it has to say the class resolved and name the next move.
    let own = server.call("debug.list_fields", serde_json::json!({"class_name": "EvalProbe$Subtask"}));
    assert!(own.starts_with("0/0 field(s) on EvalProbe$Subtask"), "{own}");
    assert_contains_all("resolved but empty", &own, &["RESOLVED", "inherited:true"]);
    assert!(!own.contains("not loaded"), "a class that resolved must never be called unloaded: {own}");

    // And the walk, attributing the row to the class that declares it.
    let chain = server.call(
        "debug.list_fields",
        serde_json::json!({"class_name": "EvalProbe$Subtask", "inherited": true, "limit": 100}),
    );
    assert_contains_all("inherited walk", &chain, &["volatile int runs", "[from EvalProbe$Task]"]);

    // "Not loaded" and "no such class" are indistinguishable over JDWP, so this must say so rather than
    // pick one — the same resolver, and therefore the same answer, as `list_methods` gives.
    let missing =
        server.call("debug.list_fields", serde_json::json!({"class_name": "com.example.NoSuchThing"}));
    assert_contains_all("unloaded class", &missing, &["is not loaded", "debug.list_classes"]);

    vec![f, item, capped, filtered, own, chain, missing]
}

/// The checked-in cassette of the session above, named for what it plays rather than for how it was made.
const DISC2_CASSETTE: &str = "list_methods_disc2";

/// The checked-in cassette of DISC-5's field listing — the JDK-free half of its pair.
const DISC5_CASSETTE: &str = "list_fields_disc5";

/// The checked-in cassette of one method-exit arming, kept for the sake of being edited.
const MEXIT_CASSETTE: &str = "method_exit_arming";

/// TEST-12 (#37): a session recorded through the proxy, then replayed **with the probe stopped**.
///
/// This is the acceptance criterion the whole issue turns on, and the order of the lines below is the
/// argument. The same assertions run twice: once against a JVM with a recorder in the middle, and once
/// against a file, after the JVM has been killed. Their *outputs* are then compared verbatim rather than
/// both merely passing — two passes of the same assertions can hide two different answers, and "the same
/// tool output" is what the issue asked to be shown.
///
/// It also round-trips through a real file on the way, because the format is half of what #37 delivers: a
/// cassette held in memory would prove the recorder works and nothing about whether anyone could read,
/// edit or check in what it produces.
///
/// Run it with `JDWP_RERECORD_CASSETTES=1` to also overwrite the checked-in fixture — deliberately opt-in,
/// since it needs a JDK and rewrites a reviewed artefact.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_recorded_session_replays_with_the_probe_stopped_and_says_the_same_thing() {
    let Some(jdk) = jdk_or_skip("a_recorded_session_replays_with_the_probe_stopped_and_says_the_same_thing")
    else {
        return;
    };
    record_replay_and_compare(&jdk, DISC2_CASSETTE, disc2_method_listing);
}

/// DISC-5 (#53): the field listing's own recording, made the same way and for the same pair.
///
/// A new tool arriving with only a JVM-dependent test would have re-created the gap TEST-12 (#37) closed:
/// the fast half is the one that runs in CI's default `cargo test` and on a machine with no Java on it.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_recorded_field_listing_replays_with_the_probe_stopped_and_says_the_same_thing() {
    let Some(jdk) =
        jdk_or_skip("a_recorded_field_listing_replays_with_the_probe_stopped_and_says_the_same_thing")
    else {
        return;
    };
    record_replay_and_compare(&jdk, DISC5_CASSETTE, disc5_field_listing);
}

/// TEST-12 (#37): the second cassette, recorded for the sake of being **edited**.
///
/// Nothing here asserts anything DISC-2's does not; the value of this recording is what
/// `a_method_exit_armed_against_a_jdwp_1_5_vm_degrades_and_says_so` does to it afterwards. It is recorded
/// and verified by the same helper as the other one so that the raw material is known-good before anyone
/// starts editing it — a hand edit on top of a cassette that was already wrong is very hard to read.
///
/// `EvalProbe$Subtask.run` is the target on purpose: the class is loaded (its static initialiser made one)
/// and nothing ever calls it, so the request arms and never fires. A method-exit request on a class the
/// probe is actually running would fill the recording with events, which are not replayed.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_method_exit_arming_is_recorded_for_the_cassette_that_gets_edited() {
    let Some(jdk) = jdk_or_skip("a_method_exit_arming_is_recorded_for_the_cassette_that_gets_edited") else {
        return;
    };
    let tape = record_replay_and_compare(&jdk, MEXIT_CASSETTE, arm_a_traced_method_exit);
    assert!(
        tape.exchanges().iter().any(|e| (e.set, e.command) == (1, 1)),
        "the edit this cassette exists for rewrites the VirtualMachine.Version reply, and there isn't one \
         on the tape — the debugger stopped asking, so the JDWP < 1.6 branch is no longer reachable this way"
    );
}

/// The hand-edited descendant of [`MEXIT_CASSETTE`]: the same session against a JVM that says `JDWP 1.5`.
const JDWP15_CASSETTE: &str = "method_exit_on_a_jdwp_1_5_vm";

/// Arming one traced method-exit request, as a body a cassette can be recorded from.
fn arm_a_traced_method_exit(server: &mut Server) -> Vec<String> {
    let armed = server.call(
        "debug.set_method_exit_stop",
        serde_json::json!({"class_pattern": "EvalProbe$Subtask", "method": "run", "trace": true}),
    );
    assert_contains_all("armed method exit", &armed, &["mexit_", "EvalProbe$Subtask"]);
    // The control for the hand-edited cassette below. Every JVM this suite can reach speaks JDWP 1.11 or
    // later, so the degraded arming must NOT appear here — otherwise the edited cassette would be proving
    // nothing about the edit.
    assert!(!armed.contains("JDWP < 1.6"), "a modern JVM must arm METHOD_EXIT_WITH_RETURN_VALUE: {armed}");
    vec![armed]
}

/// TEST-12 (#37): a JVM answering **`JDWP 1.5`** — the shape the issue names, and one nothing here can be.
///
/// TODO.md's TEST-11 row states the problem and hands it to this issue: a JDK matrix cannot reach the
/// `JDWP < 1.6` branch, because JDWP's version tracks the JDK's and the oldest JVM in the estate speaks
/// 1.11. So `debug.set_method_exit_stop`'s degraded arming — the one that tells a caller they will get the
/// return *site* and not the return *value* — had never executed, and the warning it prints had never been
/// read by anything but a human.
///
/// Three edits to a five-exchange recording reach it, and the cassette says which three. That is the
/// acceptance criterion about hand-editing, spent on the branch that most needed it rather than
/// demonstrated on a toy: the fixture is not a recording of anything, it is a world that was written down.
///
/// The second and third edits are the interesting ones. Because a cassette is keyed by request payload, the
/// `EventRequest.Set` key had to change too — the debugger arms kind 41 rather than 42 once it believes the
/// version — and an edit that changed the version alone produced a loud miss naming `EventRequest.Set`
/// instead of a quietly wrong pass. The keying and the loudness carried the edit; that is what they are for.
///
/// No JDK: there is no JVM in this test, and there could not be one.
#[test]
fn a_method_exit_armed_against_a_jdwp_1_5_vm_degrades_to_the_return_site_and_says_so() {
    let tape = Cassette::load(&cassette_path(JDWP15_CASSETTE)).expect("load the hand-edited cassette");
    let replay = ReplayServer::start(&tape).expect("start replay server");
    let armed = {
        let mut server = Server::start().expect("start server");
        server.attach(replay.port);
        arm_a_traced_method_exit_on_an_old_vm(&mut server)
    };
    assert_contains_all(
        "degraded arming",
        &armed,
        &["mexit_", "JDWP < 1.6", "Degraded to a plain MethodExit", "return site"],
    );
    replay.assert_no_misses();
}

/// The same call as [`arm_a_traced_method_exit`] without its modern-JVM control, since this one is aimed at
/// a JVM that is deliberately ancient.
fn arm_a_traced_method_exit_on_an_old_vm(server: &mut Server) -> String {
    server.call(
        "debug.set_method_exit_stop",
        serde_json::json!({"class_pattern": "EvalProbe$Subtask", "method": "run", "trace": true}),
    )
}

/// TEST-12 (#37): every checked-in cassette is exactly what the writer emits, hand edits included.
///
/// Two things at once, both cheap. It parses each fixture, which is the only guard a hand edit gets against
/// a stray comma or an odd number of hex digits; and it re-serialises and compares, which says the edit
/// stayed inside the format rather than drifting into something only this parser happens to accept. A
/// format nobody can round-trip is not hand-editable, it is hand-breakable.
///
/// No JDK: it reads files.
#[test]
fn every_checked_in_cassette_round_trips_through_the_writer() {
    for name in [DISC2_CASSETTE, DISC5_CASSETTE, MEXIT_CASSETTE, JDWP15_CASSETTE] {
        let path = cassette_path(name);
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let tape = Cassette::load(&path).unwrap_or_else(|e| panic!("{e}"));
        assert!(!tape.is_empty(), "{name} has no exchanges on it");
        assert_eq!(
            tape.to_json(),
            text,
            "{name} is not in the shape the writer emits — re-record it, or match the layout by hand so \
             the next reader can trust that what they see is what gets served"
        );
    }
}

/// Record `body` against a freshly started `EvalProbe`, then run it again from the recording with the probe
/// **killed**, and insist the two agree reply for reply.
///
/// Returns the cassette, and writes it over the checked-in fixture when `JDWP_RERECORD_CASSETTES` is set.
fn record_replay_and_compare(jdk: &Jdk, name: &str, body: fn(&mut Server) -> Vec<String>) -> Cassette {
    let probe = eval_probe_running(jdk);
    let recorder = CassetteRecorder::start(probe.port).expect("start cassette recorder");

    let live = {
        let mut server = Server::start().expect("start server");
        server.attach(recorder.port);
        body(&mut server)
        // `server` drops HERE, inside the recording. Its `Drop` sends `debug.panic` down the wire, so the
        // cassette carries the shutdown as well as the questions — without it the replayed server's own
        // exit would ask something the tape had never heard, which is a miss in a test that had already
        // finished asserting.
    };

    let mut cassette = recorder.finish(name);
    cassette.recorded_from = format!("EvalProbe on {}", java_version(jdk));
    cassette.note = "Recorded by mcp_integration.rs; re-record with JDWP_RERECORD_CASSETTES=1. Editing it \
                     by hand is a supported way to synthesise a shape no JVM here can produce."
        .to_string();
    assert!(
        !cassette.is_empty(),
        "a session that asked the debuggee questions recorded no exchanges at all — the recorder is not \
         seeing the traffic"
    );
    assert_eq!(
        cassette.events_seen, 0,
        "this session was chosen because it has no events; {} showed up, so either the probe changed or \
         the cassette is now a partial record",
        cassette.events_seen
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.json");
    cassette.save(&path).expect("save cassette");
    let tape = Cassette::load(&path).expect("load the cassette back");
    assert_eq!(tape.len(), cassette.len(), "the file lost exchanges between save and load");

    // The probe dies here. Everything below this line runs against a JSON file.
    drop(probe);

    let replay = ReplayServer::start(&tape).expect("start replay server");
    let from_tape = {
        let mut server = Server::start().expect("start server");
        server.attach(replay.port);
        body(&mut server)
    };
    replay.assert_no_misses();
    assert_eq!(live, from_tape, "the replay answered differently from the JVM it was recorded from");

    if rerecording() {
        let fixture = cassette_path(name);
        cassette.save(&fixture).expect("save the checked-in fixture");
        println!("re-recorded {}", fixture.display());
    }
    cassette
}

/// TEST-12 (#37): DISC-2's whole test again, out of a checked-in file — **no JDK, no JVM, no `#[ignore]`**.
///
/// The point of the issue in one test. Everything `list_methods_renders_java_signatures_and_marks_static`
/// asserts is asserted here too, by the same function, and this one runs in the default `cargo test` on a
/// machine with no Java on it at all. Where the probe test proves the debugger works against a JVM, this
/// proves it still works against the *recorded shape* of one — so a regression in signature rendering,
/// overload listing or the not-loaded message fails a test that costs a file read instead of a JVM launch.
///
/// It does not replace the probe test and must not (the issue is explicit): a cassette is a snapshot and
/// cannot notice the debuggee changing. It is the fast half of a pair.
#[test]
fn list_methods_renders_java_signatures_from_a_cassette() {
    let path = cassette_path(DISC2_CASSETTE);
    let tape = Cassette::load(&path).unwrap_or_else(|e| {
        panic!(
            "{e}\nRe-record it with a JDK:\n  {RERECORD_ENV}=1 scripts/integration-test.sh \
             a_recorded_session_replays"
        )
    });
    let replay = ReplayServer::start(&tape).expect("start replay server");
    {
        let mut server = Server::start().expect("start server");
        server.attach(replay.port);
        disc2_method_listing(&mut server);
    }
    // Explicit as well as in `Drop`, so the failure names this test rather than arriving during unwinding.
    replay.assert_no_misses();
}

/// DISC-5 (#53): the field listing out of a checked-in file — **no JDK, no JVM, no `#[ignore]`**.
///
/// Same shape and same argument as the test above. The bounding, the statics-first order, the empty
/// answer that must not read as a failure and the not-loaded reply are all regressions a file read can
/// catch, and only the questions about a *live* debuggee need the probe.
#[test]
fn list_fields_renders_java_declarations_from_a_cassette() {
    let path = cassette_path(DISC5_CASSETTE);
    let tape = Cassette::load(&path).unwrap_or_else(|e| {
        panic!(
            "{e}\nRe-record it with a JDK:\n  {RERECORD_ENV}=1 scripts/integration-test.sh \
             a_recorded_field_listing"
        )
    });
    let replay = ReplayServer::start(&tape).expect("start replay server");
    {
        let mut server = Server::start().expect("start server");
        server.attach(replay.port);
        disc5_field_listing(&mut server);
    }
    replay.assert_no_misses();
}

/// TEST-12 (#37): an unmatched request must fail **loudly**, and this is what proves it does.
///
/// The acceptance criterion is stated as a hazard rather than a feature: "a replay that quietly returns an
/// error reply would make every test using it meaningless". That is not a hypothetical failure mode in this
/// repo, it is the recurring one — a SIGKILL'd coverage counter, an undetectable JDK, a filter matching no
/// tests, all of them green. So the empty cassette below is the worst case on purpose: it can answer
/// *nothing*.
///
/// Two things are asserted, and the second is the one that matters. The tool call must fail — not come back
/// with a plausible-looking empty listing — and the miss must **name the command**, because the reader's
/// next move is either to re-record or to add that exchange by hand, and neither is possible from "it
/// didn't work".
///
/// Note what the attach itself proves: nothing. A JDWP attach is a handshake and no commands at all, so it
/// succeeds against a cassette with no exchanges in it — which is exactly why the loudness has to live at
/// the first real question rather than at connect time.
///
/// No JDK, for the same reason as the test above: there is no JVM in it.
#[test]
fn a_cassette_that_cannot_answer_says_which_command_it_could_not_answer() {
    let replay = ReplayServer::start(&Cassette::default()).expect("start replay server");
    let asked = {
        let mut server = Server::start().expect("start server");
        server.call("debug.attach", serde_json::json!({"host": "127.0.0.1", "port": replay.port}));
        server.call("debug.list_methods", serde_json::json!({"class_name": "EvalProbe"}))
        // Dropped inside the scope so its own shutdown traffic lands before the misses are read.
    };
    assert!(
        !asked.contains("method(s) on"),
        "a cassette with nothing on it must not produce a method listing: {asked}"
    );

    // Drained rather than read, so the `Drop` backstop does not fire on the miss this test went looking for.
    let misses = replay.take_misses();
    assert!(!misses.is_empty(), "an empty cassette answered a question without recording a miss: {asked}");
    assert!(
        misses.iter().any(|m| m.starts_with("VirtualMachine.") || m.starts_with("ReferenceType.")),
        "a miss must NAME the command, so the reader knows what to add to the cassette: {misses:?}"
    );
}

/// The `java -version` banner's first line, for the record kept inside a cassette. A cassette is a snapshot
/// of one debuggee on one JVM, and which JVM is the first thing a reader of it will want to know.
fn java_version(jdk: &Jdk) -> String {
    std::process::Command::new(&jdk.java)
        .arg("-version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or_default().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "an unidentified JVM".to_string())
}

/// DISC-3 (#31): what a loaded class was compiled from, and the source behind a stack frame's line.
///
/// The two halves are asserted separately on purpose. The JVM half is driven with `source_roots: []`
/// so it is proven to need no local file at all — that is the half that answers "is this checkout the
/// code that is running?", and a test that always had the file on disk could not tell the two apart.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn source_reports_the_compiled_from_file_and_reads_a_window_from_a_root() {
    let Some(jdk) = jdk_or_skip("source_reports_the_compiled_from_file_and_reads_a_window_from_a_root")
    else {
        return;
    };
    let probe = eval_probe_running(&jdk);
    // `examples/probes` is a source root of exactly the shape the tool expects: EvalProbe is in the
    // default package, so its file sits directly in the root with no package directories between.
    let root = probe_source_path("EvalProbe").parent().expect("probe source has a parent").to_path_buf();
    let root_str = root.to_string_lossy().into_owned();
    let mut server = Server::start_with_env(&[("JDWP_SOURCE_ROOTS", &root_str)]).expect("start server");
    server.attach(probe.port);

    let source = probe_source("EvalProbe");
    let total = source.lines().count();
    let bp1 = probe_line(&source, "// BP1");

    // --- the JVM half, with the disk half switched off for this call ---
    let jvm_only =
        server.call("debug.source", serde_json::json!({"class_name": "EvalProbe", "source_roots": []}));
    assert_contains_all(
        "JVM-reported source file",
        &jvm_only,
        &["EvalProbe.java", "reported by the JVM", "No source roots are configured"],
    );
    assert!(!jvm_only.contains("// BP1"), "an empty root list must read no file at all: {jvm_only}");
    // A JDK class proves the command rather than our probe's build: nothing local could supply this.
    let jdk_class = server
        .call("debug.source", serde_json::json!({"class_name": "java.lang.String", "source_roots": []}));
    assert!(jdk_class.contains("String.java"), "SourceFile for a JDK class: {jdk_class}");

    // --- a bounded window around a line, numbered ---
    let window = server
        .call("debug.source", serde_json::json!({"class_name": "EvalProbe", "line": bp1, "context": 2}));
    let span = format!("lines {}-{} of {total}", bp1 - 2, bp1 + 2);
    let numbered = format!("{bp1} | ");
    assert_contains_all("line window", &window, &["// BP1", &span, &numbered]);
    // Bounded, and the bound is stated — a page that reads as the whole file is the DUMP-1 failure.
    assert!(
        !window.contains("public static void main"),
        "a window must not carry the rest of the file: {window}"
    );
    assert!(window.contains("line(s) shown"), "a partial view must say so: {window}");

    // --- whole_file is possible, and capped loudly rather than silently ---
    let whole =
        server.call("debug.source", serde_json::json!({"class_name": "EvalProbe", "whole_file": true}));
    let all = format!("lines 1-{total} of {total}");
    assert_contains_all("whole file", &whole, &[&all, "public static void main"]);
    let capped = server.call(
        "debug.source",
        serde_json::json!({"class_name": "EvalProbe", "whole_file": true, "max_lines": 5}),
    );
    assert_contains_all(
        "capped whole file",
        &capped,
        &[&format!("lines 1-5 of {total}"), "5 of", "line(s) shown"],
    );

    // --- an inner class resolves to its ENCLOSING file, which is the case a class-name-derived
    //     resolver gets wrong: there is no EvalProbe$Item.java anywhere on disk ---
    let item_line = probe_line(&source, "Item(String n, int q)");
    let inner = server.call(
        "debug.source",
        serde_json::json!({"class_name": "EvalProbe$Item", "line": item_line, "context": 1}),
    );
    assert_contains_all("inner class", &inner, &["EvalProbe.java", "Item(String n, int q)"]);
    assert!(
        !inner.contains("EvalProbe$Item.java"),
        "the file name must come from the JVM, not the class name: {inner}"
    );

    // --- the failure modes stay distinguishable ---
    let unloaded = server.call("debug.source", serde_json::json!({"class_name": "com.example.NoSuchThing"}));
    assert_contains_all("unloaded class", &unloaded, &["is not loaded", "debug.list_classes"]);

    // A root that exists but does not hold this class: the JVM's answer must survive the local miss.
    let elsewhere = server.call(
        "debug.source",
        serde_json::json!({
            "class_name": "EvalProbe",
            "line": bp1,
            "source_roots": [env!("CARGO_MANIFEST_DIR")],
        }),
    );
    assert_contains_all(
        "no root holds it",
        &elsewhere,
        &["reported by the JVM", "Not found on disk", "Searched 1 root"],
    );

    // A line past the end of the file is source DRIFT, not an error — and telling the caller which it
    // is only works because the JVM's answer and the local file arrive in the same reply.
    let stale =
        server.call("debug.source", serde_json::json!({"class_name": "EvalProbe", "line": total + 500}));
    assert_contains_all("stale line number", &stale, &["past the end", "does not match the running build"]);
}

/// TEST-14 (#39): the fourth `debug.source` answer — loaded, and compiled with no `SourceFile` at all.
///
/// The realistic shape of this is a vendored jar, a shaded dependency, or an app server's own internals,
/// all of which routinely ship without `-g`; `StrippedProbe` is the stand-in, and the only probe the
/// harness compiles `-g:none`. Until it existed the branch was unreachable *by construction* — `-g` is
/// right for every other probe, and no amount of different Java produces a class file missing the
/// attribute this branch is about the absence of.
///
/// What makes it worth a test rather than a shrug is that all four of `debug.source`'s empty-handed
/// answers have to stay **distinguishable**. "Not loaded", "no root holds it", and this one send the
/// reader somewhere completely different, and it is the moment someone is already asking why they cannot
/// see their code — the worst possible moment to hand them the wrong one.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn source_says_when_a_loaded_class_carries_no_source_file_attribute() {
    let Some(jdk) = jdk_or_skip("source_says_when_a_loaded_class_carries_no_source_file_attribute") else {
        return;
    };
    let probe = Probe::launch_stripped(&jdk, "StrippedProbe").expect("launch StrippedProbe");
    // Wait for it to actually be RUNNING, not merely accepting a JDWP connection. The agent listens
    // before the main class is loaded, and this test's entire premise is the difference between "loaded
    // but stripped" and "not loaded" — so racing the class load turns the assertion into a coin flip that
    // reports the wrong finding when it loses. It lost on CI's JDK 11 leg while passing everywhere else.
    // The launch is not `launch_running` only because the class files are built differently here; the wait
    // is the same one, and so is the failure text (TEST-17, #49).
    probe.wait_until_running(EVENT_TIMEOUT, |l| tick_index(l).is_some()).unwrap_or_else(|e| panic!("{e}"));
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let stripped =
        server.call("debug.source", serde_json::json!({"class_name": "StrippedProbe", "source_roots": []}));
    assert_contains_all(
        "no SourceFile attribute",
        &stripped,
        &["NO source file", "-g:none", "debug.list_methods"],
    );
    // Not the unloaded answer. The class is right there and the attribute is what is missing, so a reply
    // saying "not loaded" would send someone hunting a classpath problem that does not exist.
    assert!(!stripped.contains("is not loaded"), "ABSENT_INFORMATION is not 'not loaded': {stripped}");
    let methods = server.call("debug.list_methods", serde_json::json!({"class_name": "StrippedProbe"}));
    assert_contains_all("the stripped class really is loaded", &methods, &["main"]);

    // Control, same session and same connection: a class that DID keep the attribute still answers.
    // Without it this test would also pass with `SourceFile` broken outright, or with every reply an
    // error the assertions happened to match — the green-run-of-nothing shape this repo keeps finding.
    let jdk_class = server
        .call("debug.source", serde_json::json!({"class_name": "java.lang.String", "source_roots": []}));
    assert_contains_all(
        "a class that kept its attribute",
        &jdk_class,
        &["String.java", "reported by the JVM"],
    );

    // The one that would be easy to get wrong, and quietly: `StrippedProbe.java` is sitting in
    // `examples/probes`, so a resolver that derived a file name from the CLASS name would find it and
    // print source for a build carrying no record of having come from that file. Right lines, wrong
    // reason — worse than the error, because nothing about the output looks wrong.
    let on_disk = probe_source_path("StrippedProbe");
    let root = on_disk.parent().expect("probe source has a parent");
    let with_root = server.call(
        "debug.source",
        serde_json::json!({"class_name": "StrippedProbe", "source_roots": [root.to_string_lossy()]}),
    );
    assert_contains_all("a root holding the file changes nothing", &with_root, &["NO source file"]);
    assert!(
        !with_root.contains("public class StrippedProbe"),
        "the file on disk must not be guessed from the class name: {with_root}"
    );
}

/// TEST-15 (#40): a class whose `.java` is an intermediate, and the SMAP that says what it came from.
///
/// This is the JSP case, which is why the SMAP path exists: Jasper compiles `hello.jsp` into a servlet
/// and records a JSR-45 SMAP mapping the generated lines back. Ask such a class what it was compiled
/// from and the honest answer — the generated `.java` — is a **confidently wrong** one, because the file
/// someone actually wrote is named nowhere else. `debug.source` reporting the intermediate as though it
/// were source is the failure this branch exists to prevent, and until now the branch had never run:
/// `javac` cannot emit the attribute, so no probe carried one (see `install_source_debug_extension` for
/// how one does now, and what was rejected on the way).
///
/// `Neighbour` is the control, and the reason the probe has two classes. It comes out of the same `javac`
/// invocation, from the same file, and is deliberately NOT patched — so a `debug.source` that announced
/// an SMAP unconditionally, or a splice that quietly did nothing, both fail here rather than passing.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn source_reports_the_smap_when_the_class_was_translated_from_another_file() {
    let Some(jdk) = jdk_or_skip("source_reports_the_smap_when_the_class_was_translated_from_another_file")
    else {
        return;
    };
    let probe = Probe::launch_with_smap(&jdk, "SmapProbe").expect("launch SmapProbe");
    // Same race as the stripped probe above: the agent listens before the classes are loaded. Waiting for
    // a tick settles both at once — the heartbeat prints `Neighbour.touched`, so a tick proves the control
    // class is loaded too, and the control is the whole reason a passing SMAP assertion means anything.
    probe.wait_until_running(EVENT_TIMEOUT, |l| tick_index(l).is_some()).unwrap_or_else(|e| panic!("{e}"));
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // Needs no local file: the SMAP travels in the class, so this half answers on a box holding no
    // source at all — which is the half that matters when the question is what the deployed thing is.
    let translated =
        server.call("debug.source", serde_json::json!({"class_name": "SmapProbe", "source_roots": []}));
    assert_contains_all(
        "SMAP present",
        &translated,
        &["SmapProbe.java", "JSR-45 SMAP) present", "is the intermediate", "hello.jsp", "*S JSP"],
    );
    // The whole attribute came back, not just a header that happened to match: `*L` and a line-section
    // entry are the last bytes of the fixture, so their arrival proves the length field was right too.
    assert_contains_all("the whole SMAP round-tripped", &translated, &["*L", "1#1,3:24", "*E"]);

    // The control: same file, same compile, no attribute. `debug.source` must say nothing about an SMAP
    // for it — an unconditional banner would be worse than none, since it would teach the reader to
    // ignore the one case where the `.java` really is not what anyone wrote.
    let plain =
        server.call("debug.source", serde_json::json!({"class_name": "Neighbour", "source_roots": []}));
    assert_contains_all("the unpatched neighbour", &plain, &["SmapProbe.java", "reported by the JVM"]);
    assert!(
        !plain.contains("SMAP") && !plain.contains("hello.jsp"),
        "a class with no source debug extension must not be announced as translated: {plain}"
    );

    // And with roots configured, both halves arrive: the intermediate IS readable and worth reading —
    // it is what the stack line numbers refer to — but it arrives labelled as an intermediate rather
    // than handed over as the source. Reading the window without the label is the confidently wrong
    // answer this whole branch is here to avoid.
    let on_disk = probe_source_path("SmapProbe");
    let root = on_disk.parent().expect("probe source has a parent");
    let both = server.call(
        "debug.source",
        serde_json::json!({
            "class_name": "SmapProbe",
            "whole_file": true,
            "source_roots": [root.to_string_lossy()],
        }),
    );
    assert_contains_all(
        "labelled intermediate, still read",
        &both,
        &["is the intermediate", "hello.jsp", "public class SmapProbe"],
    );
}

/// `retired=<n>` from a `ChurnProbe` heartbeat — how many workers have finished so far.
fn churn_retired(line: &str) -> Option<u64> {
    line.split("retired=").nth(1)?.split_whitespace().next()?.parse().ok()
}

/// `held=<n>` from a `ChurnProbe` heartbeat — retired workers whose `Thread` object the probe is still
/// holding, and which the debugger can therefore still resolve *after* they have finished.
///
/// The probe's side of TEST-19 (#54). See `ChurnProbe.HELD`.
fn churn_held(line: &str) -> Option<u64> {
    line.split("held=").nth(1)?.split_whitespace().next()?.parse().ok()
}

/// The latest value of one of `ChurnProbe`'s heartbeat counters, or 0 before the first tick.
fn churn_counter(probe: &Probe, read: fn(&str) -> Option<u64>) -> u64 {
    probe.output().iter().rev().find_map(|l| read(l)).unwrap_or(0)
}

/// How many rows a dump actually printed — one header line per thread, at the start of a line.
fn dump_row_count(dump: &str) -> u64 {
    dump.lines().filter(|l| l.starts_with("0x")).count() as u64
}

/// TEST-10 (#35): a dump taken while a pool retires and replaces its workers must account for the
/// threads that went away underneath it.
///
/// `collect_dump_rows` asks the JVM about each thread separately from the `AllThreads` that named it, so
/// every dump is answering questions about a list that is already out of date. On a real request pool
/// that is not the exotic case, it is the *only* case — and until this probe existed the suite had never
/// produced one, because every other probe's threads outlive the test. Two different things can happen
/// to a thread that vanishes between the list and the read, and this asserts what the reply says about
/// **both**:
///
/// 1. The thread has finished but the debugger still holds its `Thread` object, so `Status` answers
///    `ZOMBIE` quite happily (the same JDWP behaviour FILT-2 turned on). The row is printed.
/// 2. The object has since been collected as well, so the id is not valid any more and the status read
///    *fails*. That is the `continue` arm the coverage review named — the row is dropped.
///
/// **Which of the two a given worker leaves behind is the probe's decision, not the collector's**
/// (TEST-19, [#54](https://github.com/YgorPerez/java-debugging-mcp/issues/54)). A JDWP object id is a weak
/// reference, so a retired worker's `Thread` is unreachable the moment it exits and the probe's explicit
/// collector then invalidates the id — state (2). `ChurnProbe` *holds* every second retirement's `Thread`
/// so that it cannot go that way, which is state (1). Both populations therefore exist by construction,
/// alternating in creation order, and the only thing a dump still has to do is overlap some deaths.
///
/// It used to be the collector's decision, and that was the flake. Which state a dump found depended on
/// where each death fell between `AllThreads` and the next `System.gc()`, i.e. on **how long the dump
/// took** — so a loaded JDK 11 run, where the same dump costs ~950ms instead of ~500ms, found every
/// listed worker already collected and no `[zombie]` at all, roughly three runs in five. Twelve attempts
/// did not help, because a slower dump made every attempt worse in the same direction. Now a slower dump
/// overlaps *more* deaths and finds more of both, which is the failure mode the right way round.
///
/// Both of those states were originally pinned as WRONG rather than fixed, because a test that reaches a
/// line and asserts nothing about it is how this repo has previously reported coverage it had not looked
/// at. DUMP-4 ([#47](https://github.com/YgorPerez/java-debugging-mcp/issues/47)) fixed them and flipped
/// the two assertions, which is exactly what the pins were for: (1) a `ZOMBIE` row said `running — … pass
/// suspend:true`, the opposite answer plus a remedy that can never apply, and now says it finished; (2)
/// the rows lost to (2) were counted into `… +N more thread(s) (raise limit, or narrow with name_filter)`
/// against a `limit` of 500 that had not bound, and now have a line of their own that suggests nothing.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
// Both outcomes a vanishing thread can produce — a `[zombie]` row and a dropped row — have to be asserted
// against ONE churn window, because which one you get depends on whether the id was collected yet. Split
// into two tests it would be two probes and two windows, and neither half could then claim the other's
// outcome was even possible. The length is the single setup, not repetition.
#[allow(clippy::too_many_lines)]
fn a_dump_of_a_churning_pool_accounts_for_the_threads_that_vanished_under_it() {
    let Some(jdk) = jdk_or_skip("a_dump_of_a_churning_pool_accounts_for_the_threads_that_vanished_under_it")
    else {
        return;
    };
    let probe = Probe::launch(&jdk, "ChurnProbe").expect("launch ChurnProbe");
    // Through the relay, deliberately. A dump that finishes in ten milliseconds barely overlaps
    // anything, and the state this test is about — the thread list going stale while it is being read —
    // is a function of how LONG the read takes. On loopback that is nearly nothing, which is why the arm
    // has never executed. At a 2ms round trip the same ~130-packet dump takes a quarter of a second,
    // which is what it costs against an instance one hop away: the debuggee is unchanged and only the
    // wire is slower, exactly as in ADR-0011's measurements. Declared before the server so it outlives
    // it.
    let relay = LatencyRelay::start(probe.port, std::time::Duration::from_millis(2)).expect("start relay");
    let mut server = Server::start().expect("start server");
    server.attach(relay.port);

    // Both preconditions, read off the debuggee's own heartbeat rather than inferred from how long
    // anything took (TEST-19, #54).
    //
    // `retired >= 48`: a whole generation of workers must have turned over before anything is asserted —
    // at `SLOTS / LIFE_MS` a first tick can arrive before any churn worker has retired at all, and a dump
    // taken then would be a dump of a perfectly stable JVM.
    //
    // `held > 0`: the probe is holding at least one *finished* worker's `Thread`, so at least one thread
    // exists that the debugger can list, watch end, and still resolve. That is exactly the state the
    // `[zombie]` half below asserts against, and it is not a state a debugger can ask about — it is a
    // property of the debuggee's reference graph, so racing for it is the only alternative to being told.
    // The probe tells. Which is why a failure below is now a statement about `collect_dump_rows` and not
    // about the host's timing.
    let heartbeat = probe
        .wait_for_line(EVENT_TIMEOUT, |l| {
            churn_retired(l).is_some_and(|n| n >= 48) && churn_held(l).is_some_and(|n| n > 0)
        })
        .unwrap_or_else(|| {
            panic!(
                "the pool never reached a full retired generation with a held worker in it\n  output: {:?}",
                probe.output()
            )
        });
    let held = churn_held(&heartbeat).unwrap_or_default();

    // `limit` is deliberately an order of magnitude above the thread count. It matters later: whatever
    // the dump ends up withholding, the caller's limit is provably not the reason.
    let ask = serde_json::json!({"limit": 500, "monitors": false, "max_frames": 3});
    let mut vanished_but_reported: Option<String> = None;
    let mut vanished_and_dropped: Option<String> = None;
    // How many workers finished *while a dump was in flight*, summed over the attempts. The number that
    // separates "the probe stopped churning" from "the probe churned and the dump did not notice" — two
    // failures that used to be reported as the first one whichever had happened.
    let mut retired_during_a_dump = 0_u64;
    for _ in 0..12 {
        let before = churn_counter(&probe, churn_retired);
        let dump = server.call("debug.thread_dump", ask.clone());
        retired_during_a_dump += churn_counter(&probe, churn_retired).saturating_sub(before);

        // Whatever the churn did, the reply is a whole reply about a JVM that still has its stable half.
        // This runs on every attempt, so a dump that fell apart under churn cannot hide behind one that
        // did not.
        assert!(dump.contains("Cost:"), "a dump under churn must still complete:\n{dump}");
        let (read, total) = dump_thread_counts(&dump)
            .unwrap_or_else(|| panic!("no `N/M thread(s)` header in:\n{}", head_of(&dump)));
        assert_eq!(
            read,
            dump_row_count(&dump),
            "the header's count must be the rows it really printed:\n{}",
            head_of(&dump)
        );
        assert!(read <= total, "a dump cannot read more threads than the JVM listed:\n{}", head_of(&dump));
        let stable = dump.lines().filter(|l| l.contains("\"stable-worker-")).count();
        assert_eq!(
            stable,
            8,
            "the eight non-churning workers must be in every dump — they are the fixed point that makes \
             the rest of this test about churn rather than about noise:\n{}",
            head_of(&dump)
        );

        if vanished_but_reported.is_none() && dump.contains("[zombie]") {
            vanished_but_reported = Some(dump.clone());
        }
        if vanished_and_dropped.is_none() && read < total {
            vanished_and_dropped = Some(dump);
        }
        if vanished_but_reported.is_some() && vanished_and_dropped.is_some() {
            break;
        }
    }

    // --- (1) finished, still readable: reported as a row, with a status that says so ---
    let reported = vanished_but_reported.unwrap_or_else(|| {
        assert!(
            retired_during_a_dump > 0,
            "twelve dumps and not one worker finished while any of them was running — ChurnProbe is not \
             churning, so there was never a thread to catch vanishing\n  output: {:?}",
            probe.output()
        );
        // Which leaves the interesting failure, and it is now unambiguous. Workers DID finish under these
        // dumps, and the probe holds every second one's `Thread` so that finishing does not make it
        // unreadable — for ~6s, against dumps that take a fraction of that. Whether the row appears is
        // therefore no longer a question about GC timing or host load (TEST-19, #54): the JVM listed a
        // thread, the thread ended, the id still resolves, and the dump did not say so.
        panic!(
            "{retired_during_a_dump} worker(s) finished while these twelve dumps were reading, and the \
             probe was holding {held} finished workers' Thread objects so their ids stay resolvable — yet \
             no dump reported a [zombie] row. `collect_dump_rows` listed threads that then ended and did \
             not report them as finished\n  output: {:?}",
            probe.output()
        )
    });
    let row = reported
        .lines()
        .find(|l| l.starts_with("0x") && l.contains("[zombie]"))
        .expect("a zombie row was found a moment ago");
    assert!(
        row.contains("\"churn-worker-"),
        "the thread that vanished is still named, not reduced to a bare id: {row}"
    );
    let name = row.split('"').nth(1).unwrap_or_default().to_string();
    let section =
        dump_section(&reported, &name).unwrap_or_else(|| panic!("no section for {name} in:\n{reported}"));
    // WAS the finding, now the fix (DUMP-4, #47). The JVM has just answered ZOMBIE — this thread is
    // *finished* — and the row used to explain its unreadable stack as "running" and advise
    // `suspend:true`, which can never help because a finished thread is not suspendable. Both halves are
    // asserted: that it says what the thread actually is, and that it stops offering the impossible.
    assert!(
        section.contains("finished — this thread has already terminated (JDWP reports ZOMBIE)"),
        "a finished thread's row must say it finished, not that it is running:\n{section}"
    );
    assert!(
        !section.contains("pass suspend:true"),
        "a finished thread can never be suspended, so its row must not advise it:\n{section}"
    );

    // --- (2) finished AND collected: the id is gone, so the row is dropped ---
    let dropped = vanished_and_dropped.unwrap_or_else(|| {
        panic!(
            "twelve dumps, {retired_during_a_dump} worker(s) finished under them, and no thread id ever \
             went stale mid-dump — so `collect_dump_rows`' dead-thread arm still has not executed. The \
             odd-numbered half of the churn population is deliberately left unheld for this, so either \
             the probe's collector is not reclaiming them or every one of them outlived its own read\n  \
             output: {:?}",
            probe.output()
        )
    });
    let (read, total) = dump_thread_counts(&dropped).expect("counts");
    let missing = total - read;
    // Not silently dropped: the difference is stated, and it is the arithmetic the header promised.
    assert!(
        dropped.contains(&format!("… +{missing} more thread(s)")),
        "{missing} thread(s) went away mid-dump and the reply must SAY there are that many it did not \
         show — silence here reads as a complete dump:\n{}",
        head_of(&dropped)
    );
    // WAS the finding, now the fix (DUMP-4, #47). The only explanation used to be the caller's own
    // `limit` — 500 against ~63 threads, so raising it changes nothing, and narrowing with `name_filter`
    // cannot bring back a thread that no longer exists. Two remedies, neither able to alter the outcome.
    // The cause has its own sentence now, and `limit` is not blamed for a truncation it did not cause.
    assert!(
        dropped.contains(&format!("… +{missing} more thread(s) ENDED while this dump was reading")),
        "the shortfall is the churn, and the reply must attribute it to the churn:\n{}",
        head_of(&dropped)
    );
    assert!(
        !dropped.contains("raise limit"),
        "`limit` was 500 against ~63 threads and never bound — offering it as the remedy is a no-op:\n{}",
        head_of(&dropped)
    );

    // --- and the same pool, dumped with the VM frozen ---
    // Freezing right after `AllThreads` closes almost the whole window, which is why this half exists:
    // the states above are a property of reading a *live* list, not a fault the churn induces in the
    // dump. What is asserted here is coherence — every row it printed that is still a thread is one it
    // was actually holding — plus a real stack off the stable half and a VM that is running afterwards.
    let base = highest_tick(&probe).expect("no tick to count from");
    let frozen =
        server.call("debug.thread_dump", serde_json::json!({"limit": 500, "suspend": true, "max_frames": 4}));
    assert_contains_all(
        "a suspending dump of a churning pool completes and resumes it",
        &frozen,
        &["verified running", "Cost:"],
    );
    assert!(
        frozen.contains("ChurnProbe.park:"),
        "the frozen dump must have read a real stable worker's stack, not only counted threads:\n{}",
        head_of(&frozen)
    );
    let incoherent: Vec<&str> = frozen
        .lines()
        .filter(|l| l.starts_with("0x") && !l.contains("debugger-suspended") && !l.contains("[zombie]"))
        .collect();
    assert!(
        incoherent.is_empty(),
        "a dump that froze the VM must be holding every live thread it reports; these rows are neither \
         held nor finished: {incoherent:?}"
    );

    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2)).is_some(),
        "the probe stopped ticking after a suspending dump of a churning pool\n  output: {:?}",
        probe.output().len(),
    );

    server.panic_reset();
}

/// The `⏱  Held the VM suspended for Nms.` figure a dump reports, in milliseconds.
fn dump_held_ms(dump: &str) -> Option<u64> {
    let at = dump.find("Held the VM suspended for ")? + "Held the VM suspended for ".len();
    dump.get(at..)?.split_once("ms").and_then(|(n, _)| n.trim().parse().ok())
}

/// DUMP-3 (#43) and DUMP-5 (#51): the threads a caller came to look at are the ones a server starts LAST,
/// so the default dump — and the default listing that decides what to dump — have to reach them.
///
/// Measured against a real `WildFly` 21 (TEST-8, #24), a default `debug.thread_dump` of a 267-thread loaded
/// instance returned **zero application threads**. All 40 slots went to JVM internals, MSC service
/// threads, the deployment scanner and Undertow selectors, while 13 `default task-*` workers sat a median
/// 328 frames deep in application code and were never read. The cause was ordering, not size:
/// `collect_dump_rows` walked JDWP `AllThreads` — creation order — and stopped at `limit`, and an app
/// server creates its request pool last. The header said `40/267 thread(s)`, which reads as a sample.
///
/// `ChurnProbe` was built for exactly this shape (#35): its eight `stable-worker-*` threads start **after**
/// all 48 churn slots, so they sit behind ~55 earlier ids and the churn keeps minting more after them.
/// Before this fix the default dump reached **0 of 8**; only `limit: 500` reached all eight — which is the
/// finding, restated as a test.
///
/// **Reversing the walk is not the fix, and this probe is why.** Newest-first would return churn workers,
/// which are created continuously and forever. The rule has to be about *what* the threads are, from data
/// the dump already reads (the name), not about where they sit in the list.
///
/// **`debug.list_threads` is read here too, against the same probe and the same attach (DUMP-5, #51).** It
/// kept the creation-order truncation for as long as the dump had it fixed, which is the worse half of the
/// two whatever its packet cost says: it is the call someone runs *first*, precisely to decide what to
/// dump. Reading both in one test is the only way to assert the thing #51 was actually about — that the
/// cheap call and the expensive one select the same population — and it costs one JVM rather than two,
/// which matters on a suite whose neighbouring test is timing-sensitive about a pool that churns.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
// One probe, one attach, five readings that only mean something together: the default dump reaches the
// pool, the default LISTING reaches the same pool for a stated price, both say how they chose, and the
// wider selection is still bounded by the suspension budget. Split up, each part would be asserting
// against a debuggee the others never proved was in the state that matters.
#[allow(clippy::too_many_lines)]
fn a_default_dump_and_a_default_listing_reach_a_pool_the_debuggee_started_last() {
    let Some(jdk) =
        jdk_or_skip("a_default_dump_and_a_default_listing_reach_a_pool_the_debuggee_started_last")
    else {
        return;
    };
    let probe = Probe::launch(&jdk, "ChurnProbe").expect("launch ChurnProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // A full generation must have turned over first — the stable eight are started only after all 48
    // churn slots have been staggered, so a dump taken before that would be a dump of half a probe.
    probe.wait_for_line(EVENT_TIMEOUT, |l| churn_retired(l).is_some_and(|n| n >= 48)).unwrap_or_else(|| {
        panic!("the pool never retired a full generation\n  output: {:?}", probe.output())
    });

    // --- the reading itself: DEFAULT arguments, which is the whole point ---
    let dump = server.call("debug.thread_dump", serde_json::json!({}));
    let (read, total) = dump_thread_counts(&dump)
        .unwrap_or_else(|| panic!("no `N/M thread(s)` header in:\n{}", head_of(&dump)));
    // Without this the test could pass on a debuggee small enough that `limit` never bit, which is the
    // one shape that cannot reproduce the bug.
    assert!(
        total > 40,
        "ChurnProbe must have more threads than the default limit, or truncation never happens: {read}/{total}"
    );
    let stable = dump.lines().filter(|l| l.contains("\"stable-worker-")).count();
    // TEST-22 (#57): the count alone cannot say WHY it is not 8, and the two causes call for opposite
    // responses. If the debuggee has fewer than 8 stable workers at this moment, the premise failed and
    // the selection rule is not implicated at all; if it has 8 and the dump reached fewer, the rule is.
    // Establish that from the debuggee before blaming either.
    let existing = stable_workers_in_debuggee(&mut server);
    assert_eq!(
        stable,
        8,
        "a default dump reached {stable} of the 8 threads this debuggee started LAST, and the debuggee \
         has {existing} stable worker(s) alive right now.\n  If that count is 8, this is the WildFly \
         reading (#43): 40 slots spent on the least interesting end of the list because `AllThreads` is \
         creation order.\n  If it is fewer, the PROBE never reached its steady state and the selection \
         rule is not what failed — the wait above passed on retirements, which does not prove the stable \
         eight have started.\n{}",
        head_of(&dump)
    );
    // …and it did not get there by reading everything: the churn population is still mostly withheld, so
    // the pool was reached by CHOOSING, not by the limit quietly growing.
    let churn = dump.lines().filter(|l| l.contains("\"churn-worker-")).count();
    assert!(
        churn < 40,
        "the whole churn population was read, so `limit` is no longer bounding anything: {churn} rows\n{}",
        head_of(&dump)
    );

    // --- and it SAYS what the forty are, which is the other half of the finding ---
    assert_contains_all(
        "a truncated dump states the rule it selected by, so a caller need not read the source",
        &dump,
        &["by NAME FAMILY", "printed in creation order"],
    );
    assert!(
        dump.contains("… +") && dump.contains("more thread(s)"),
        "the withheld remainder is still counted:\n{}",
        head_of(&dump)
    );
    assert!(
        dump.contains("churn-worker-#"),
        "the footer must name the groups it withheld — the caller's next question is 'what am I not \
         seeing', and 227 of them being one pool is the answer:\n{dump}"
    );

    // --- the CHEAP call, on the same debuggee, in the same state (DUMP-5, #51) ---
    let listed = server.call("debug.list_threads", serde_json::json!({}));
    let (shown, listed_total) = list_thread_counts(&listed)
        .unwrap_or_else(|| panic!("no `N/M thread(s)` header in:\n{}", head_of(&listed)));
    assert!(
        listed_total > shown,
        "the listing must be truncated or there is no selection to test: {shown}/{listed_total}"
    );
    let listed_stable = listed.lines().filter(|l| l.contains("stable-worker-")).count();
    assert_eq!(
        listed_stable, 8,
        "a default listing reached {listed_stable} of the 8 threads this debuggee started LAST, with \
         {existing} alive in the debuggee when the dump above was taken. Before #51 it reached 0, for the \
         same reason the dump did: forty slots spent on the least interesting end of a list that is in \
         creation order. A count below 8 ALIVE is a different finding — the probe, not the rule \
         (TEST-22, #57).\n{listed}"
    );
    // The whole of #51 in one assertion: a caller lists, picks a thread, then dumps. If the two tools
    // chose by different rules the thread they picked would not be in the dump.
    assert_eq!(
        listed_stable, stable,
        "the cheap call and the expensive one must select the same population:\n{listed}"
    );
    assert_contains_all(
        "a truncated listing states its rule in the DUMP's words — one wording, because two would be how \
         the two tools start meaning different things",
        &listed,
        &["by NAME FAMILY", "printed in creation order"],
    );
    assert!(
        listed.contains("… +") && listed.contains("more (raise limit"),
        "the withheld remainder is still counted:\n{listed}"
    );
    assert!(
        listed.contains("churn-worker-#"),
        "the listing's footer must name the groups it withheld too:\n{listed}"
    );

    // --- what choosing cost the cheap call, which is the criterion #51 set for it ---
    let listed_cost = dump_packet_cost(&listed)
        .unwrap_or_else(|| panic!("a truncated listing must report what it spent:\n{listed}"));
    // One packet per thread NAME plus the thread list itself. A rate rather than an absolute, because the
    // probe's population is only approximately fixed — but the rate IS the property being defended: a dump
    // reads ~8 packets per thread it shows, and a selection pass that made the reconnaissance call as
    // expensive as the dump would have missed the point of having it.
    assert!(
        listed_cost <= listed_total + 2,
        "choosing by family must cost one packet per thread name and no more: {listed_cost} packets for \
         {listed_total} threads\n{listed}"
    );
    let dump_cost = dump_packet_cost(&dump).unwrap_or_else(|| panic!("no cost line in:\n{}", head_of(&dump)));
    // Against the CHEAPEST dump there is — this one suspended nothing, so it read two packets a thread and
    // no frames at all. A listing that read every name in the JVM still spends less than that.
    assert!(
        listed_cost < dump_cost,
        "the whole point of list_threads is being the cheap call: it spent {listed_cost} packets against \
         the {dump_cost} of a dump that read no stacks at all\n{listed}"
    );

    // A narrowed listing is untouched by any of this: one family, so the round-robin IS creation order,
    // and a rule that changed nothing must not announce itself.
    let narrowed =
        server.call("debug.list_threads", serde_json::json!({"name_filter": "stable-worker", "limit": 8}));
    assert!(
        !narrowed.contains("NAME FAMILY"),
        "a listing that withheld nothing must not explain a rule that changed nothing:\n{narrowed}"
    );
    assert_eq!(
        narrowed.lines().filter(|l| l.contains("stable-worker-")).count(),
        8,
        "the filter still finds all eight:\n{narrowed}"
    );

    // --- the cost of choosing stays inside the budget, and is still reported ---
    let frozen = server.call(
        "debug.thread_dump",
        // Six frames, because a parked worker's own code is four down: `Object.wait0` / `wait` / `wait`
        // sit above `ChurnProbe.park`. Reading a stack that stops inside `java.lang.Object` would prove
        // the row exists, not that the thread this dump went looking for was actually read.
        serde_json::json!({"suspend": true, "max_frames": 6}),
    );
    assert_contains_all(
        "a suspending default dump completes, resumes and reports both costs",
        &frozen,
        &["verified running", "Held the VM suspended for", "Cost:"],
    );
    let held = dump_held_ms(&frozen).unwrap_or_else(|| panic!("no held figure in:\n{}", head_of(&frozen)));
    assert!(
        held <= 2000,
        "the default 2000ms budget must still bound a dump that reads every thread's NAME before \
         choosing: held {held}ms\n{}",
        head_of(&frozen)
    );
    assert!(
        frozen.contains("ChurnProbe.park:"),
        "the frozen default dump must have read a stable worker's real stack, not merely listed it:\n{}",
        head_of(&frozen)
    );

    // The listing against a dump that actually read stacks — the comparison the cost line's "~8 packets
    // per thread shown" is making, now that there is one in hand. Printed as well as asserted: these
    // numbers ARE #51's acceptance criterion, and a run that proves them should not leave the next reader
    // to re-derive them. Measured ~3.7× on this probe (103 against 381 on JDK 11, 104 against 385 on JDK
    // 25); asserted at 2× because the multiple depends on how deep the stacks happen to be, and the claim
    // being defended is "the cheap call is still the cheap call", not a particular ratio.
    let frozen_cost =
        dump_packet_cost(&frozen).unwrap_or_else(|| panic!("no cost line in:\n{}", head_of(&frozen)));
    eprintln!(
        "DUMP-5 cost on {listed_total} threads: listing {listed_cost}, dump reading no stacks \
         {dump_cost}, dump reading stacks {frozen_cost}"
    );
    assert!(
        listed_cost * 2 < frozen_cost,
        "a listing must stay materially cheaper than the dump it is reconnaissance FOR: {listed_cost} \
         against {frozen_cost}\n{listed}"
    );

    // A budget that cannot be met has to bind on the *choosing* too. If the name pass ran to completion
    // regardless of the deadline, this dump would hold the VM for as long as it took to name 60 threads
    // while claiming a 1ms budget — the widened read paying for itself out of somebody else's instance.
    let base = highest_tick(&probe).expect("no tick to count from");
    let starved = server.call("debug.thread_dump", serde_json::json!({"suspend": true, "max_suspend_ms": 1}));
    assert_contains_all(
        "the selection pass is inside the budget, not before it",
        &starved,
        &["Stopped early", "suspension budget ran out", "INCOMPLETE", "verified running"],
    );
    let starved_held =
        dump_held_ms(&starved).unwrap_or_else(|| panic!("no held figure in:\n{}", head_of(&starved)));
    assert!(
        starved_held < 500,
        "a 1ms budget held the VM for {starved_held}ms — the name pass is not checking the deadline\n{}",
        head_of(&starved)
    );
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2)).is_some(),
        "the probe stopped ticking after a starved dump\n  output: {:?}",
        probe.output().len(),
    );

    server.panic_reset();
}

/// A dump section's own thread id — the `0x…` that opens its header line.
fn section_thread_id(section: &str) -> Option<&str> {
    section.lines().next()?.split_whitespace().next()
}

/// The single monitor a `holds:` line names, e.g. `ContendedProbe$Lock2@3f`.
fn sole_held_lock(section: &str) -> Option<&str> {
    let held = section.lines().find_map(|l| l.trim().strip_prefix("holds: "))?;
    (!held.contains(", ")).then_some(held)
}

/// TEST-10 (#35): with four locks and forty-eight waiters in one dump, the holder each waiter is told
/// about has to be the RIGHT one.
///
/// `thread_dump_shows_stacks_and_the_deadlock_cycle` already proves the correlation exists, and
/// `DeadlockProbe` is the right shape for that: a cross-pairing that a report merely listing monitors per
/// thread would get backwards. What two threads and two locks cannot show is whether the correlation
/// still picks the right holder when there is a choice — with one other thread in the dump, `← held by
/// 0x…` is right by construction, and every wrong answer is also the right one.
///
/// Here every waiter is on a lock that three other threads in the same dump are each holding one of, so
/// naming the holder means finding the one row out of four whose `holds` list contains this waiter's
/// contended monitor. The expected string is built from the dump's OWN answers — the label off the
/// holder's `holds:` line and the id off its header — rather than from anything this test assumes, so
/// what is checked is that the two halves agree, not that both match a guess.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_contended_lock_names_its_real_holder_out_of_four() {
    /// The probe's own monitors. Every assertion here is scoped to these, so a thread blocked on some
    /// other lock — the debuggee's, or the JVM's — is not mistaken for one of this test's waiters.
    const PROBE_LOCK: &str = "ContendedProbe$Lock";

    let Some(jdk) = jdk_or_skip("a_contended_lock_names_its_real_holder_out_of_four") else { return };
    let probe = Probe::launch(&jdk, "ContendedProbe").expect("launch ContendedProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // The probe polls every waiter's own `getState()` and only says `armed` once all 48 report BLOCKED,
    // so this is the JVM's answer rather than a sleep long enough to look like one.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("armed"))
        .unwrap_or_else(|| panic!("the probe never got all waiters blocked\n  output: {:?}", probe.output()));
    // `armed` is printed BEFORE the first tick, so the heartbeat has to be waited for separately rather
    // than assumed to exist by now. Taking the baseline straight off `armed` passed every time this test
    // was run on its own and failed the first time it ran alongside 59 others, which is the whole reason
    // the tick baseline is read from a line that has actually arrived.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some())
        .unwrap_or_else(|| panic!("the probe never ticked after arming\n  output: {:?}", probe.output()));
    let base = highest_tick(&probe).expect("a tick line just arrived");

    // monitors_only: the lock graph is the entire question here, and 52 stacks would be another test's
    // worth of suspension for nothing this one reads.
    let dump = server
        .call("debug.thread_dump", serde_json::json!({"limit": 200, "suspend": true, "monitors_only": true}));
    assert_contains_all("the dump completed and resumed the VM", &dump, &["verified running", "Cost:"]);

    // The premise, checked before anything is concluded from it. If the contention had not formed, every
    // assertion below would pass vacuously on a dump with no `waiting to enter` lines in it at all.
    //
    // Counted over THIS probe's locks rather than over every `waiting to enter:` line in the dump. The
    // first version counted all of them and demanded exactly 48; CI failed it on all three JDK legs with
    // 50, because a debuggee contains blocked threads this test did not create — a holder pausing on
    // `System.out`'s monitor to print its heartbeat is enough. Scoping to `ContendedProbe$Lock` keeps the
    // count exact where exactness means something (all 48 waiters really are queued on the four locks
    // under test) without asserting anything about monitors that are not this test's business.
    let waiting =
        dump.lines().filter(|l| l.contains("waiting to enter:")).filter(|l| l.contains(PROBE_LOCK)).count();
    assert_eq!(
        waiting,
        48,
        "this test is about contention at scale, so all 48 waiters must be queued on the probe's own \
         four locks in the dump it reads:\n{}",
        head_of(&dump)
    );

    let mut locks_seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for k in 0..4 {
        let holder_name = format!("holder-{k}");
        let holder = dump_section(&dump, &format!("\"{holder_name}\""))
            .unwrap_or_else(|| panic!("no {holder_name} section in:\n{dump}"));
        // One lock, and the right one. A holder that had released it — `Object.wait()` does, which is how
        // every other parking probe here waits — would show no `holds:` line, and the test would be
        // measuring nothing.
        let lock = sole_held_lock(&holder).unwrap_or_else(|| {
            panic!("{holder_name} must hold exactly one lock and nothing else:\n{holder}")
        });
        assert!(
            lock.starts_with(&format!("ContendedProbe$Lock{k}@")),
            "{holder_name} must hold Lock{k} — a bare java.lang.Object here could be paired any way at \
             all and still look right: {lock}"
        );
        let holder_id = section_thread_id(&holder)
            .unwrap_or_else(|| panic!("no thread id on {holder_name}'s header:\n{holder}"));
        locks_seen.insert(lock.to_string());

        // Every one of this lock's twelve waiters, not just the first: a correlation that resolved the
        // holder once and reused it would pass a spot check on waiter 0 and be wrong for the other 47.
        let expected = format!("waiting to enter: {lock} ← held by {holder_id} \"{holder_name}\"");
        for i in 0..12 {
            let waiter_name = format!("waiter-{k}-{i}");
            let waiter = dump_section(&dump, &format!("\"{waiter_name}\""))
                .unwrap_or_else(|| panic!("no {waiter_name} section in:\n{dump}"));
            assert!(
                waiter.contains(&expected),
                "{waiter_name} must be shown blocked on the lock {holder_name} is actually holding, named \
                 by that thread's own id — expected `{expected}`:\n{waiter}"
            );
        }
    }

    // Four distinct monitor objects, or "the right one out of four" was never a choice: if two locks
    // shared an object id, naming either holder would satisfy every assertion above.
    assert_eq!(locks_seen.len(), 4, "the four locks must be four distinct objects, saw {locks_seen:?}");

    // And nobody was handed a holder that holds a different lock. The loop above proves each waiter got
    // the right annotation; this proves the dump contains no other kind, including on rows the loop never
    // named.
    //
    // Scoped to the probe's own locks for the same reason as the count above: a thread blocked on a
    // monitor this test did not create has no `$Lock<k>` in its name and no `holder-<k>` holding it, so it
    // read as a mispairing and would have failed here the moment it failed the count.
    let wrong: Vec<&str> = dump
        .lines()
        .filter(|l| l.contains("waiting to enter:"))
        .filter(|l| l.contains(PROBE_LOCK))
        .filter(|l| {
            let (before, after) = l.split_once(" ← held by ").unwrap_or((l, ""));
            // `…$Lock2@3f ← … "holder-2"` — the digit in the lock's class name must be the digit in the
            // holder's name, so a swapped pair is visible without trusting the loop above.
            let lock_k = before.split("$Lock").nth(1).and_then(|s| s.chars().next());
            let holder_k = after.split("\"holder-").nth(1).and_then(|s| s.chars().next());
            lock_k.is_none() || holder_k.is_none() || lock_k != holder_k
        })
        .collect();
    assert!(wrong.is_empty(), "every contended lock must name its own holder; these do not: {wrong:?}");

    // TRACE-2 discipline: only the debuggee can report that the suspension ended, and the 52 wedged
    // threads never will.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2)).is_some(),
        "the probe stopped ticking after a dump of 52 contended threads\n  output: {:?}",
        probe.output(),
    );

    server.panic_reset();
}

/// The one frame line in a stack that mentions `needle`, or `None`.
fn frame_line<'a>(stack: &'a str, needle: &str) -> Option<&'a str> {
    stack.lines().map(str::trim).find(|l| l.starts_with('#') && l.contains(needle))
}

/// TEST-10 (#35): a lambda, a method reference and an anonymous inner class in one stack, and what
/// `decode_signature` makes of the names the JVM invents for them.
///
/// Seventeen probes and not one of them put a synthetic frame in a stack a test read, so the signature
/// decoder had only ever been handed names a human wrote. A dump of any real application server is full
/// of the other kind, and the failure mode is not a crash — it is a class name that comes back subtly
/// wrong and reads as plausible.
///
/// Two of the three are fine and this says so specifically, because "it did not crash" is not a finding
/// either way:
///
/// * `SyntheticProbe$1.run:<line>` — the anonymous class's name says nothing about what it does, but the
///   frame carries a source line, and the line is what makes it actionable.
/// * `SyntheticProbe.lambda$lambdaStep$0:<line>` — the lambda's BODY is an ordinary method on the
///   enclosing class, named after the method it was written in, with a line of its own.
///
/// The third was not fine, and SIG-1 (#46) fixed it. The JVM names a lambda's hidden class
/// `SyntheticProbe$$Lambda/0x00007f…`, where the `/` separates the class from a JVM-assigned suffix and
/// is **not** a package separator. `decode_signature` used to replace every `/` in a `L…;` signature with
/// a `.`, because in a JNI signature that is what a `/` is — so a caller was shown
/// `SyntheticProbe$$Lambda.0x00007f…`, which reads exactly like a class `0x00007f…` in a package
/// `SyntheticProbe$$Lambda` and is a name the JVM will not answer to. Worse one step out:
/// `debug.list_classes` decoded identically, so searching for the JVM's own spelling returned `0/0` and
/// the miss was explained with *"a class the JVM has not loaded yet does not appear here at all"* — about
/// three classes that were loaded and in the very list being searched.
///
/// **The suffix's shape is JDK-dependent and is deliberately not asserted here.** #36's matrix caught the
/// original pinned assertion passing on JDK 21 and failing on JDK 11: 15+ produces a real hidden class
/// with a hex address (`$$Lambda/0x0000000087040970`), 11 a VM-anonymous class with an ordinal and a
/// plain decimal (`$$Lambda$3/397187020`). Pinning either spelling reproduces that failure the other way
/// round. What holds on every JDK is the boundary — a `/`, followed by a suffix beginning with a digit,
/// which no Java name may — and the round trip, which is the criterion that actually matters: the exact
/// string a caller reads out of a stack, pasted back into `list_classes`, finds the class.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
// The three constructs are one stack and one JVM, and the point of the test is what they look like side
// by side — split across functions each half would relaunch the probe and could no longer compare.
#[allow(clippy::too_many_lines)]
fn synthetic_frames_render_with_names_a_reader_can_act_on() {
    let Some(jdk) = jdk_or_skip("synthetic_frames_render_with_names_a_reader_can_act_on") else { return };
    let probe = Probe::launch(&jdk, "SyntheticProbe").expect("launch SyntheticProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // `parked` means the whole chain is on the worker's stack — main polls the worker's own `getState()`
    // for it, so this is not a sleep dressed up as a barrier.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("parked"))
        .unwrap_or_else(|| panic!("the worker never parked\n  output: {:?}", probe.output()));
    // Both of these must happen BEFORE the pause below. `parked` is printed before the first tick, and
    // the pause freezes the thread that prints them — so a baseline taken after it would be read from a
    // probe that can no longer produce the line it is waiting for.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some())
        .unwrap_or_else(|| panic!("the probe never ticked\n  output: {:?}", probe.output()));
    let base = highest_tick(&probe).expect("a tick line just arrived");

    let tid = thread_hex_for(&mut server, "synthetic-worker")
        .unwrap_or_else(|| panic!("no synthetic-worker thread"));
    // JDWP will not read a running thread's frames, and the worker is parked rather than at a stop point,
    // so nothing else has suspended it.
    server.call("debug.pause", serde_json::json!({}));
    let stack = server.call(
        "debug.get_stack",
        serde_json::json!({"thread_id": tid, "max_frames": 20, "include_variables": false}),
    );

    let source = probe_source("SyntheticProbe");

    // --- the two that are readable, each pinned to the line it was written on ---
    let anon_line = probe_line(&source, "call(lambdaStep());");
    assert!(
        stack.contains(&format!("SyntheticProbe$1.run:{anon_line}")),
        "the anonymous inner class must appear with its real name AND the line it runs — `$1` alone names \
         nothing a reader could go and look at:\n{stack}"
    );
    let lambda_line = probe_line(&source, "return () -> call(SyntheticProbe::viaMethodReference);");
    assert!(
        stack.contains(&format!("SyntheticProbe.lambda$lambdaStep$0:{lambda_line}")),
        "the lambda's body must be attributed to the method it was written in, with its own line:\n{stack}"
    );
    // The method reference resolves to the ordinary method it names, which is the whole point of one.
    let ref_line = probe_line(&source, "        park();");
    assert!(
        stack.contains(&format!("SyntheticProbe.viaMethodReference:{ref_line}")),
        "a method reference must reach the method it refers to, by name:\n{stack}"
    );

    // The anonymous inner class again, from the other side: `SyntheticProbe$1` is separated by a `$`,
    // never by a `/`, so `decode_signature` had nothing to get wrong about it and #46 changed nothing
    // here. Confirmed rather than assumed, because "the other two were fine" is a claim.
    let anon = server.call("debug.list_classes", serde_json::json!({"filter": "SyntheticProbe$1"}));
    assert!(
        anon.starts_with("1/1 class(es)") && anon.contains("SyntheticProbe$1"),
        "an anonymous inner class is named with a `$` and must round-trip unchanged:\n{anon}"
    );

    // --- the hidden classes ---
    // Three of them: the lambda, the method reference, and the one `new Thread(SyntheticProbe::worker)`
    // created. Each is a frame in its own right, between the ordinary frames on either side.
    let hidden: Vec<&str> = stack.lines().map(str::trim).filter(|l| l.contains("$$Lambda")).collect();
    assert_eq!(
        hidden.len(),
        3,
        "a lambda and a method reference each add a hidden-class frame of their own, and the thread's own \
         method reference adds a third:\n{stack}"
    );
    // SIG-1 (#46). Asserted by SHAPE rather than by spelling, because the suffix is not stable across
    // versions and the first draft of this test pinned JDK 21's — #36's matrix caught it on the 11 leg
    // the day it landed, which is the whole reason the matrix exists:
    //
    //   JDK 21:  SyntheticProbe$$Lambda/0x0000000092040970
    //   JDK 11:  SyntheticProbe$$Lambda$3/574182878        <- ordinal, and decimal rather than hex
    //
    // What both have in common is the boundary: a `/`, and after it a suffix the JVM assigned rather
    // than package structure. That is the invariant, and it is what is asserted.
    let mut rendered: Vec<&str> = Vec::new();
    for line in &hidden {
        let named = line
            .split_once(".run")
            .unwrap_or_else(|| panic!("a hidden-class frame is a call to its generated `run`: {line}"))
            .0;
        let class =
            named.split_once(' ').unwrap_or_else(|| panic!("a frame line starts with `#N `: {line}")).1;
        let (owner, assigned) = class.rsplit_once('/').unwrap_or_else(|| {
            panic!(
                "a hidden class is named `<class>/<suffix the JVM assigned>` and the `/` must survive \
                 into what a caller is shown — this is the SIG-1 (#46) regression: {line}"
            )
        });
        assert!(
            owner.contains("$$Lambda"),
            "the part before the `/` is the class the JVM generated the lambda for: {line}"
        );
        assert!(
            assigned.as_bytes().first().is_some_and(u8::is_ascii_digit),
            "the part after the `/` is a JVM-assigned suffix — hex on 15+, decimal on 11, a digit on \
             both, which is exactly why no Java name can be mistaken for one: {line}"
        );
        // SyntheticProbe is in the default package, so its hidden classes have no package structure at
        // all: any `.` left in the name is the old rewrite, whatever the JDK spelled the suffix as.
        assert!(
            !class.contains('.'),
            "pinned the other way now: the JVM's `/` must NOT come back as a `.` — that name reads as a \
             class in a package `SyntheticProbe$$Lambda` and the JVM will not answer to it: {line}"
        );
        rendered.push(class);
    }
    // And it has no line number, because a hidden class has no line table — so the one frame a reader
    // cannot act on is also the one with nothing to look up.
    let generated = frame_line(&stack, "$$Lambda").expect("a hidden-class frame was found a moment ago");
    let after_run = generated.split(".run").nth(1).unwrap_or("!");
    assert!(
        after_run.trim().is_empty(),
        "a hidden class has no line table, so its frame must carry no line: {generated}"
    );

    // THE ROUND TRIP, which is the whole acceptance criterion: the exact string a caller reads out of the
    // stack, pasted straight back in, finds the one class it names. Each hidden class's suffix is unique
    // to it, so `1/1` is the honest expectation and not a lucky substring.
    for class in &rendered {
        let found = server.call("debug.list_classes", serde_json::json!({"filter": class}));
        assert!(
            found.starts_with("1/1 class(es)") && found.contains(class),
            "a name this tool printed must find its class again when pasted back: {class}\n{found}"
        );
    }
    // The prefix a human would type finds all three, and none of them is spelled the old way.
    let by_prefix =
        server.call("debug.list_classes", serde_json::json!({"filter": "SyntheticProbe$$Lambda"}));
    assert!(
        !by_prefix.starts_with("0/0 class(es)") && !by_prefix.contains("$$Lambda."),
        "the listing must spell a hidden class the way the JVM does:\n{by_prefix}"
    );

    // THE SECOND HALF OF #46, and the more dangerous one. The mangled spelling is now a genuine miss —
    // but the classes are loaded, this tool is looking straight at them, and the reply has to say so.
    // "A class the JVM has not loaded yet does not appear here at all" sent a caller hunting a code path
    // that had never been the problem. CONTEXT.md's rule under **Loaded** is that where the tool cannot
    // tell it offers both readings; here it *can* tell, so it names the actual cause.
    let legacy = rendered[0].replace('/', ".");
    let by_legacy = server.call("debug.list_classes", serde_json::json!({"filter": legacy}));
    assert!(
        by_legacy.starts_with("0/0 class(es)"),
        "the mangled spelling is not a name the debuggee uses, so it must not match:\n{by_legacy}"
    );
    assert!(
        by_legacy.contains(rendered[0]) && by_legacy.contains("spelling difference"),
        "a miss on a loaded class must hand back the spelling that does work:\n{by_legacy}"
    );
    assert!(
        !by_legacy.contains("has not loaded yet") && !by_legacy.contains("may not be loaded"),
        "a class the tool is looking at must never be explained away as one the JVM has not \
         loaded:\n{by_legacy}"
    );

    // The dump renders frames through the same decoder, so the two views must not disagree — a caller who
    // reads one and pastes into the other has to get the same class.
    let dump = server
        .call("debug.thread_dump", serde_json::json!({"name_filter": "synthetic-worker", "max_frames": 20}));
    // Compared against the exact name `get_stack` printed, rather than a spelling written in here — same
    // reason as the round trip above, and it asserts more: not "both look mangled" but "both produced the
    // identical string", which is what a caller moving between the two views actually depends on.
    assert!(
        dump.contains(rendered[0]) && !dump.contains("$$Lambda."),
        "the dump and get_stack must spell a hidden class the same way:\n{dump}"
    );

    server.panic_reset();
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 1)).is_some(),
        "the probe stopped ticking after the pause was released\n  output: {:?}",
        probe.output(),
    );
}

/// DISC-4 (#50): the name a stack printed for a hidden class, pasted into the tools that answer
/// questions ABOUT a class — which is the step SIG-1 (#46) made reachable and did not take.
///
/// #46 ended with a caller able to read `SyntheticProbe$$Lambda/0x00007cd1e0001220` off a stack and find
/// it again with `list_classes`. Asking anything about it still failed: `resolve_loaded_class` built one
/// ordinary descriptor, `L<name with dots as slashes>;`, and on JDK 15+ the real descriptor carries a
/// **dot** before the address, because a `/` there would not be a legal descriptor (JVMS §4.2.2). So the
/// tool refused the very name it had just handed out, and refused it with "not loaded" — about a class it
/// had just printed a frame for.
///
/// **This is the half of the matrix a single JDK cannot test.** On JDK 11 the descriptor genuinely uses a
/// slash (`LSyntheticProbe$$Lambda$3/574182878;`), so the old code was already right there and a green run
/// on 11 says nothing about 15+; pin either separator and the other leg of #36's matrix fails. Nothing
/// here is spelled out for that reason: the class name comes off the live stack and goes straight back in,
/// which is the criterion in the caller's terms and is JDK-agnostic by construction.
///
/// `debug.list_fields` was named in #50's criteria and **did not exist** when this test was written, which
/// is how DISC-5 ([#53](https://github.com/YgorPerez/java-debugging-mcp/issues/53)) came to be filed. What
/// both criteria were really about is the one resolver behind every tool that takes a class name, so the
/// tools that did exist stood in for it: `list_methods`, and `debug.source` for the "answers plainly"
/// criterion — a hidden class has no `SourceFile` attribute, and saying so is a real answer where "not
/// loaded" was not. `list_fields` now exists and is asked the same round trip below, so the criterion is
/// met by the tool it named rather than by a stand-in.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_hidden_class_answers_questions_asked_under_the_name_the_stack_printed() {
    let Some(jdk) = jdk_or_skip("a_hidden_class_answers_questions_asked_under_the_name_the_stack_printed")
    else {
        return;
    };
    let probe = Probe::launch(&jdk, "SyntheticProbe").expect("launch SyntheticProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // Same barrier as the SIG-1 test above, and for the same reason: `parked` means the whole chain is on
    // the worker's stack, and both waits must happen before the pause freezes the thread that prints them.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("parked"))
        .unwrap_or_else(|| panic!("the worker never parked\n  output: {:?}", probe.output()));
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some())
        .unwrap_or_else(|| panic!("the probe never ticked\n  output: {:?}", probe.output()));
    let base = highest_tick(&probe).expect("a tick line just arrived");

    let tid = thread_hex_for(&mut server, "synthetic-worker")
        .unwrap_or_else(|| panic!("no synthetic-worker thread"));
    server.call("debug.pause", serde_json::json!({}));
    let stack = server.call(
        "debug.get_stack",
        serde_json::json!({"thread_id": tid, "max_frames": 20, "include_variables": false}),
    );

    // STEP ONE OF THE ROUND TRIP: read the name, exactly as a caller would — off the printed frame, with
    // no reconstruction. A hidden-class frame is `#N <class>.run` and carries no line, so everything
    // between the frame number and `.run` is the name the tool chose to show.
    let frame = frame_line(&stack, "$$Lambda")
        .unwrap_or_else(|| panic!("no hidden-class frame in the worker's stack:\n{stack}"));
    let named = frame
        .split_once(".run")
        .unwrap_or_else(|| panic!("a hidden-class frame is a call to its generated `run`: {frame}"))
        .0;
    let printed =
        named.split_once(' ').unwrap_or_else(|| panic!("a frame line starts with `#N `: {frame}")).1;

    // STEP TWO: ask about it. The lambda's generated class implements `Runnable`, so its own `run` is the
    // method that must come back — on JDK 11's VM-anonymous class as much as on 15+'s hidden one.
    let methods = server.call("debug.list_methods", serde_json::json!({"class_name": printed}));
    assert!(
        !methods.contains("is not loaded"),
        "DISC-4 (#50): {printed} is the name this tool printed for a frame in the stack it just read — \
         calling it unloaded is wrong about a class the debugger is looking straight at:\n{methods}"
    );
    assert!(
        methods.contains(&format!("method(s) on {printed}")),
        "list_methods must answer under the name it was asked about:\n{methods}"
    );
    assert!(
        methods.contains("void run()"),
        "a lambda's hidden class implements Runnable, so its generated `run` is the method a caller came \
         for — an empty listing would resolve and still answer nothing:\n{methods}"
    );

    // DISC-5 (#53): the tool the issue behind #50 went looking for and did not find now exists, so the
    // stand-in above is joined by the real thing. Nothing is spelled out about WHAT a hidden class holds,
    // and that is deliberate rather than lazy: whether the JVM gives a non-capturing lambda's class a
    // `LAMBDA_INSTANCE$` static, a captured `arg$1`, or no field at all is a detail of the metafactory
    // that has changed between JDK generations, and pinning it is the JDK-locked assertion #36's matrix
    // exists to catch. What must hold on every JDK is that the class RESOLVES under the name the stack
    // printed and gets a real answer — a listing, or the explicit "it declares none", never "not loaded".
    //
    // Measured rather than guessed, and the same on both legs: `SyntheticProbe`'s lambdas capture nothing,
    // so the reply is `0/0` on JDK 11 (`SyntheticProbe$$Lambda$3/891297757`) and on JDK 25
    // (`SyntheticProbe$$Lambda/0x…`) alike. That makes the hidden class the tool's *typical* empty answer
    // rather than an exotic one — which is exactly why an empty listing has to say the class resolved.
    let fields = server.call("debug.list_fields", serde_json::json!({"class_name": printed}));
    assert!(
        !fields.contains("is not loaded"),
        "DISC-5 (#53): {printed} came off the stack this server just printed — a field listing may not \
         call it unloaded:\n{fields}"
    );
    assert!(
        fields.contains(&format!("field(s) on {printed}")),
        "list_fields must answer under the name it was asked about:\n{fields}"
    );
    assert!(
        !fields.starts_with("0/0") || fields.contains("RESOLVED"),
        "a hidden class that declares nothing is the likeliest answer here, and an empty listing that does \
         not say the class resolved is indistinguishable from a miss:\n{fields}"
    );
    // The ordinary control for the same tool: `SyntheticProbe.GATE` is a plain `static final Object`, and
    // all three of those words are the listing's own work rather than the JVM's.
    let ordinary_fields =
        server.call("debug.list_fields", serde_json::json!({"class_name": "SyntheticProbe"}));
    assert!(
        ordinary_fields.contains("static final java.lang.Object GATE"),
        "an ordinary class must be unaffected by the hidden-class spelling:\n{ordinary_fields}"
    );

    // The other tool that resolves a class name this way, and the "says so plainly" criterion. A hidden
    // class carries no `SourceFile` attribute: that is a real answer about a class that resolved, and it
    // is the answer `debug.source` already gives for a stripped class (TEST-14, #39). The failure it must
    // not fall back into is the unresolvable one. Asserted as "not that", plus whichever real answer this
    // JDK gives, because a JVM that did attach a source file to its generated class would be answering
    // too — what must not happen is the class going missing.
    let source = server.call("debug.source", serde_json::json!({"class_name": printed, "source_roots": []}));
    assert!(
        !source.contains("is not loaded"),
        "a class that just appeared in a stack must never come back as one the JVM has not loaded:\n{source}"
    );
    assert!(
        source.contains("NO source file") || source.contains("reported by the JVM"),
        "having resolved the class, source has to say something true about it — either the file the JVM \
         names or that there is none:\n{source}"
    );

    // The control, on the same connection: an ordinary class still resolves through the same path, so a
    // resolver that had started answering everything with the first class it found would fail here.
    let ordinary = server.call("debug.list_methods", serde_json::json!({"class_name": "SyntheticProbe"}));
    assert!(
        ordinary.contains("static void park()") && ordinary.contains("method(s) on SyntheticProbe"),
        "an ordinary class must be unaffected by the hidden-class spelling:\n{ordinary}"
    );
    // And a name nothing loaded still gets the honest "not loaded" answer, which the extra candidate must
    // not have turned into a false hit.
    let missing =
        server.call("debug.list_methods", serde_json::json!({"class_name": "com.example.Nope$$Lambda/0x1"}));
    assert!(
        missing.contains("is not loaded"),
        "trying a second spelling must not invent a class that is not there:\n{missing}"
    );

    server.panic_reset();
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 1)).is_some(),
        "the probe stopped ticking after the pause was released\n  output: {:?}",
        probe.output(),
    );
}

/// The right-hand side of a `debug.evaluate` reply — `PrimitiveProbe.sByte = (byte) -7` → `(byte) -7`.
fn evaluated(reply: &str) -> &str {
    reply.split_once(" = ").map_or(reply, |(_, v)| v).trim()
}

/// TEST-10 (#35): every Java primitive, and an array of every Java primitive, read as a local, as a
/// static field and as an instance field — and rendered identically by all three.
///
/// `jdwp-client/src/types.rs` measured 16.67% region coverage, and the coverage review's verdict was "one
/// big match over value kinds and most arms are for types the probes never produce — low percentage, not
/// a finding". That was true *because of the probes*: the suite deals in `int`, `String` and objects, so
/// `byte`, `short`, `char`, `float` and `boolean` had never once come back over the wire in a test. A
/// renderer nobody has run is not a renderer whose output anyone knows.
///
/// **The arrays are the half that matters, and not for symmetry.** `handlers.rs` used to render a bare
/// primitive with its own private copy of the match (`render_primitive`), so the copy in `types.rs` —
/// `Value::format`, the one that measured 16.67% — was reached only through ARRAY ELEMENTS and the
/// type-mismatch message. A probe with eight primitive locals and no arrays would have exercised the
/// duplicate and left the original exactly as unmeasured as before, which is the kind of coverage that
/// reports a number without having looked. Reading through both paths is what caught it; TYPE-1
/// ([#48](https://github.com/YgorPerez/java-debugging-mcp/issues/48)) then deleted the duplicate, so
/// there is now one renderer in `jdwp-client` and every route below reaches it. The arrays stay: they are
/// still the only way to see an element rendered on its own, and a `short[]` read as an `int[]` would be
/// visible here rather than plausible.
///
/// The values are picked so the rendering can be pinned rather than merely observed: signed extremes,
/// which catch a width or signedness mistake that `3` never could, and floats that are exact binary
/// fractions, so the expected string does not depend on anyone's rounding.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
// Six primitives read three ways each — local, field, array element — is eighteen assertions that only
// mean something side by side: the point is that the SAME value renders identically by all three routes,
// and that comparison cannot be made across separate test functions. One line per assertion is the floor.
#[allow(clippy::too_many_lines)]
fn every_primitive_and_its_array_renders_the_same_as_local_field_and_element() {
    let Some(jdk) = jdk_or_skip("every_primitive_and_its_array_renders_the_same_as_local_field_and_element")
    else {
        return;
    };
    let probe = Probe::launch(&jdk, "PrimitiveProbe").expect("launch PrimitiveProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let source = probe_source("PrimitiveProbe");
    let line = probe_line(&source, "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "PrimitiveProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .unwrap_or_else(|| panic!("the breakpoint in PrimitiveProbe.work never fired"));

    let stack =
        server.call("debug.get_stack", serde_json::json!({"max_frames": 1, "include_variables": true}));

    // `(local name, static field, instance field, rendered value)` — the same eight values reached three
    // ways. Reading all three and comparing them is the point: they are three different resolution paths
    // in the handlers, and a difference between them is a bug that no single-path test can see.
    let scalars = [
        ("b", "sByte", "b", "(byte) -7"),
        ("s", "sShort", "s", "(short) -300"),
        ("c", "sChar", "c", "(char) 'Q'"),
        ("i", "sInt", "i", "(int) -2147483648"),
        ("j", "sLong", "j", "(long) 9000000000"),
        ("f", "sFloat", "f", "(float) 1.5"),
        ("d", "sDouble", "d", "(double) -2.25"),
        ("z", "sBoolean", "z", "(boolean) true"),
    ];
    // And the arrays, which are the only route to `Value::format`. Every element carries its own type
    // prefix, so a `short[]` read as an `int[]` would be visible here rather than plausible.
    let arrays = [
        ("bs", "sBytes", "bs", "byte[3]{(byte) 1, (byte) -2, (byte) 127}"),
        ("ss", "sShorts", "ss", "short[3]{(short) -300, (short) 0, (short) 300}"),
        (
            "cs",
            "sChars",
            "cs",
            "char[3]{(char) 'a', (char) 'Z', (char) '\\uD800' (unpaired surrogate, not a character)}",
        ),
        ("is", "sInts", "is", "int[3]{(int) 0, (int) -1, (int) 2147483647}"),
        ("js", "sLongs", "js", "long[2]{(long) -9000000000, (long) 9000000000}"),
        ("fs", "sFloats", "fs", "float[2]{(float) 0.5, (float) -1.25}"),
        ("ds", "sDoubles", "ds", "double[2]{(double) 2.5, (double) -0.125}"),
        ("zs", "sBooleans", "zs", "boolean[2]{(boolean) true, (boolean) false}"),
    ];

    for (local, static_field, instance_field, want) in scalars.iter().chain(arrays.iter()) {
        assert!(
            stack.contains(&format!("{local} = {want}")),
            "the local `{local}` must render as `{want}`:\n{stack}"
        );
        let by_static = server.evaluate(&format!("PrimitiveProbe.{static_field}"));
        assert_eq!(
            evaluated(&by_static),
            *want,
            "the static field `{static_field}` holds the same value as the local `{local}` and must \
             render the same way; got {by_static}"
        );
        let by_instance = server.evaluate(&format!("PrimitiveProbe.holder.{instance_field}"));
        assert_eq!(
            evaluated(&by_instance),
            *want,
            "the instance field `holder.{instance_field}` holds the same value and must render the same \
             way; got {by_instance}"
        );
    }

    // WAS the finding, now the fix (TYPE-1, #48). `chars[2]` is `(char) 0xD800`, a lone surrogate — an
    // ordinary thing to find in a Java `char[]`, since a `char` is a UTF-16 code unit and not a Unicode
    // scalar value. It is not representable as a Rust `char`, and the renderer's `unwrap_or('?')` used to
    // fire and hand back `(char) '?'`, byte-identical to a real question mark with nothing in the reply
    // to tell the two apart. The array above pins the new rendering; this pins the property that made it
    // a finding — that it can be told apart from a genuine `'?'` rather than merely rendered as
    // *something*.
    assert!(
        !stack.contains("(char) 'Z', (char) '?'"),
        "a lone surrogate must not render as a literal '?', which is a value the debuggee could really \
         hold:\n{stack}"
    );
    assert!(
        stack.contains("(char) '\\uD800' (unpaired surrogate"),
        "it renders as the code unit it is, and says what that is:\n{stack}"
    );
    let real_question_mark = server.evaluate("PrimitiveProbe.sChars[1]");
    assert_eq!(
        evaluated(&real_question_mark),
        "(char) 'Z'",
        "sanity: element 1 is a Z, so the surrogate above came from element 2 and not from a failed read"
    );

    // The other route into `Value::format`: the type-mismatch message renders the value that was refused.
    // Both object shapes go through it, and neither is reachable from an array of primitives.
    let null_into_int =
        server.call("debug.set_value", serde_json::json!({"target": "PrimitiveProbe.sInt", "value": "null"}));
    assert_contains_all(
        "null refused for an int, showing what was refused",
        &null_into_int,
        &["is declared int", "(object) null", "not assignable"],
    );
    let string_into_int = server
        .call("debug.set_value", serde_json::json!({"target": "PrimitiveProbe.sInt", "value": "\"nope\""}));
    assert_contains_all(
        "a reference refused for an int, showing its id",
        &string_into_int,
        &["is declared int", "(object) @", "not assignable"],
    );
    // Refused means refused: the field still holds what it did, so the loop above is describing the same
    // JVM the assertions below would see.
    assert_eq!(
        evaluated(&server.evaluate("PrimitiveProbe.sInt")),
        "(int) -2147483648",
        "a refused set_value must not have written anything"
    );

    let base = highest_tick(&probe).unwrap_or(0);
    server.panic_reset();
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 1)).is_some(),
        "the probe stopped ticking after the breakpoint was cleared\n  output: {:?}",
        probe.output(),
    );
}

/// The tick number out of one `SwapProbe` line (`answer 1 tick 12`), or `None` if it is not one.
///
/// Separate from [`tick_index`] because this probe prints the swapped value *before* the tick, and a
/// test that cannot read the tick cannot tell "the swap did nothing" from "the JVM never resumed".
fn swap_tick(line: &str) -> Option<i64> {
    let (_, after) = line.split_once(" tick ")?;
    after.split_whitespace().next()?.parse().ok()
}

/// The answer `SwapProbe` last printed, and the tick it printed it on.
fn last_answer(probe: &Probe) -> Option<(i64, i64)> {
    probe.output().iter().rev().find_map(|l| {
        let answer = l.strip_prefix("answer ")?.split_whitespace().next()?.parse().ok()?;
        Some((answer, swap_tick(l)?))
    })
}

/// One edit to a probe's source, boxed so a table of cases can hold several.
type SourceEdit = Box<dyn Fn(String) -> String>;

/// Compile a `SwapProbe` whose `answer()` returns `value`, into a fresh class root.
///
/// Returns the temp dir (the *class root*, which is what `debug.reload_class` wants) — the caller must
/// hold it, since dropping it deletes the bytes the JVM is being asked to load.
fn swap_probe_returning(jdk: &Jdk, value: i32) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir for the recompiled probe");
    jdk.compile_probe_variant("SwapProbe", dir.path(), |src| {
        src.replace("int v = 1; // SWAP_VALUE", &format!("int v = {value}; // SWAP_VALUE"))
    })
    .expect("compile the modified SwapProbe");
    dir
}

/// DISC-9 (#63): the edit a line-table comparison cannot see, and bytecode can.
///
/// `int v = 1;` to `int v = 2;` is `iconst_1` to `iconst_2` — one byte, same length, same line, so the
/// line table is **identical** and `check_stale`'s default answer is a clean "no line moved". That is not
/// an exotic case: a changed constant, a flipped comparison, a swapped operator are what you actually
/// iterate on in a compile-and-retest loop, so the cheaper evidence is quietest exactly where the loop
/// lives.
///
/// Both halves are asserted in one test on purpose — the claim is a *contrast* between the two evidences
/// on one build, and splitting it across two tests would let either half drift without the pairing failing.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn bytecode_catches_a_same_line_edit_that_the_line_table_calls_clean() {
    let Some(jdk) = jdk_or_skip("bytecode_catches_a_same_line_edit_that_the_line_table_calls_clean") else {
        return;
    };
    let probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // The JVM is running `v = 1`; the build on disk says `v = 2`. Nothing else differs.
    let edited = swap_probe_returning(&jdk, 2);
    let root = edited.path().display().to_string();
    let args = serde_json::json!({"class_name": "SwapProbe", "class_roots": [root]});

    let lines_only = server.call("debug.check_stale", args.clone());
    assert!(
        !lines_only.contains("🚨 STALE"),
        "the premise of this test is that line tables MISS this edit; if they now catch it, the test is \
         no longer testing anything:\n{lines_only}"
    );
    assert_contains_all(
        "the default answer bounds its own claim",
        &lines_only,
        &["no line moved", "bytecode:true"],
    );

    let mut with_bytecode = args;
    with_bytecode["bytecode"] = serde_json::json!(true);
    let both = server.call("debug.check_stale", with_bytecode);

    assert_contains_all(
        "bytecode catches what the line table could not",
        &both,
        &["🚨 STALE", "bytecode index", "bytecode is the one to believe", "different javac"],
    );
    // The cost claim is part of the interface, so the reply has to carry it.
    assert!(both.contains("JDWP packet(s)"), "the reply must report what it spent:\n{both}");
}

/// The control for the above: with `bytecode:true` on a build that IS running, both evidences must agree
/// and neither may cry stale. A false positive here would be worse than the blind spot it replaces.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn bytecode_comparison_does_not_cry_stale_on_the_running_build() {
    let Some(jdk) = jdk_or_skip("bytecode_comparison_does_not_cry_stale_on_the_running_build") else {
        return;
    };
    let probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // The running source, recompiled by the same javac. A comment-only edit moves no line and changes no
    // byte, so both evidences must come back clean.
    let current = tempfile::tempdir().expect("tempdir");
    jdk.compile_probe_variant("SwapProbe", current.path(), |src| {
        src.replace("// SWAP_VALUE", "// SWAP_VALUE (control build)")
    })
    .expect("recompile the unmodified probe");

    let out = server.call(
        "debug.check_stale",
        serde_json::json!({
            "class_name": "SwapProbe",
            "class_roots": [current.path().display().to_string()],
            "bytecode": true,
        }),
    );

    assert!(!out.contains("STALE"), "false positive with bytecode comparison on a current build:\n{out}");
    assert_contains_all(
        "both evidences answer, and agree",
        &out,
        &["Both evidences agree", "identical bytecode", "strongest answer"],
    );
}

/// Compile `SwapProbe` with two lines inserted above `answer()`, which moves every line number in it
/// without changing the class's shape. The same edit `a_stale_build_is_detected_and_a_current_one_is_not`
/// uses, because it is the drift `debug.source` cannot see: same class, same file name, older bytecode.
fn swap_probe_with_shifted_lines(jdk: &Jdk) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir for the shifted probe");
    jdk.compile_probe_variant("SwapProbe", dir.path(), |src| {
        src.replace("    static int answer() {", "    // shifted\n    // shifted\n    static int answer() {")
    })
    .expect("compile the shifted SwapProbe");
    dir
}

/// Attach with `class_roots` set, which `Server::attach` does not do.
fn attach_with_class_roots(server: &mut Server, port: u16, root: Option<&std::path::Path>) {
    let mut args = serde_json::json!({"host": "127.0.0.1", "port": port});
    if let Some(r) = root {
        args["class_roots"] = serde_json::json!([r.display().to_string()]);
    }
    let out = server.call("debug.attach", args);
    assert!(out.contains("Connected"), "attach failed: {out}");
}

/// DISC-8 (#62): arming a line breakpoint reports stale bytecode **without being asked**.
///
/// `debug.check_stale` already answers this, but only for a caller who suspects drift — and the whole
/// failure is that an agent does not. It arms `:39`, sees it never fire or fire with nonsense locals, and
/// spends its next twenty calls debugging the program instead of the deployment. Arming is where that
/// starts, and it is also the call that has already paid for the line table, so the check is free here.
///
/// `trace:true` keeps the probe running: this test is about the reply, and a suspending breakpoint would
/// freeze the JVM for the rest of it.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn arming_a_breakpoint_against_a_stale_build_says_so_unasked() {
    let Some(jdk) = jdk_or_skip("arming_a_breakpoint_against_a_stale_build_says_so_unasked") else {
        return;
    };
    let probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let shifted = swap_probe_with_shifted_lines(&jdk);
    let mut server = Server::start().expect("start server");
    attach_with_class_roots(&mut server, probe.port, Some(shifted.path()));

    let armed = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "SwapProbe", "line": 39, "trace": true}),
    );

    assert_contains_all(
        "arming reports drift nobody asked about, and names the file",
        &armed,
        &["STALE BYTECODE", "SwapProbe.class", "check_stale", "reload_class"],
    );
    // The warning is an aside, not a refusal: the breakpoint the caller asked for must still be armed.
    assert!(armed.contains("Stop-point ID"), "the breakpoint must still have been set:\n{armed}");
}

/// The two silences, which are what make the warning above worth reading. A detector that fires on a
/// current build is discounted forever after the first time it misleads someone.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn arming_says_nothing_about_drift_on_a_current_build() {
    let Some(jdk) = jdk_or_skip("arming_says_nothing_about_drift_on_a_current_build") else { return };
    let probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    // The running source, recompiled. `compile_probe_variant` refuses a no-op edit, so the change is a
    // comment: it moves no line and alters no bytecode.
    let current = tempfile::tempdir().expect("tempdir");
    jdk.compile_probe_variant("SwapProbe", current.path(), |src| {
        src.replace("// SWAP_VALUE", "// SWAP_VALUE (control build)")
    })
    .expect("recompile the unmodified probe");
    let mut server = Server::start().expect("start server");
    attach_with_class_roots(&mut server, probe.port, Some(current.path()));

    let armed = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "SwapProbe", "line": 39, "trace": true}),
    );

    assert!(armed.contains("Stop-point ID"), "the breakpoint must have been set:\n{armed}");
    assert!(!armed.contains("STALE"), "false positive on a rebuild of the running source:\n{armed}");
}

#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn arming_says_nothing_about_drift_when_no_class_root_is_configured() {
    let Some(jdk) = jdk_or_skip("arming_says_nothing_about_drift_when_no_class_root_is_configured") else {
        return;
    };
    let probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    attach_with_class_roots(&mut server, probe.port, None);

    let armed = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "SwapProbe", "line": 39, "trace": true}),
    );

    assert!(armed.contains("Stop-point ID"), "the breakpoint must have been set:\n{armed}");
    assert!(
        !armed.contains("STALE"),
        "with nothing to compare against, the reply must not mention drift at all:\n{armed}"
    );
}

/// SWAP-1 (#58): the headline claim — a Java change reaches a JVM that was never restarted.
///
/// The assertion is over the **debuggee's own stdout**, not over what the server reported about itself.
/// A tool that answered "✅ Reloaded" while the JVM kept running the old bytecode would pass any
/// assertion made against the reply, and that is precisely the failure this feature is most likely to
/// have: `RedefineClasses` succeeding for a class nobody re-enters looks identical to it doing nothing.
///
/// The tick counter is what separates the two ways this can fail. `answer 2` never appearing is one
/// finding if the probe stopped printing (the JVM froze — a harness or resume bug) and quite another if
/// it kept printing `answer 1` (the swap did nothing — a real bug in this feature), and the assertion
/// message has to be able to say which, per TEST-19 (#54)'s lesson.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_hot_reload_changes_what_a_running_jvm_prints() {
    let Some(jdk) = jdk_or_skip("a_hot_reload_changes_what_a_running_jvm_prints") else { return };
    let probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let classes = swap_probe_returning(&jdk, 2);
    let root = classes.path().display().to_string();

    // A dry run first, which is what a caller should do against a shared JVM: it must name the file and
    // the capability, and it must not change anything.
    let dry = server.call(
        "debug.reload_class",
        serde_json::json!({"class_name": "SwapProbe", "class_roots": [root], "dry_run": true}),
    );
    assert_contains_all(
        "a dry run reports what would be shipped and sends nothing",
        &dry,
        &["Dry run", "Would ship", "SwapProbe.class", "canRedefineClasses=true"],
    );
    let after_dry = last_answer(&probe).expect("the probe prints an answer");
    assert_eq!(after_dry.0, 1, "a dry run must not have changed the running bytecode: {dry}");

    let reloaded = server
        .call("debug.reload_class", serde_json::json!({"class_name": "SwapProbe", "class_roots": [root]}));
    assert_contains_all(
        "the reload reports success and where the bytes came from",
        &reloaded,
        &["✅ Reloaded SwapProbe", "SwapProbe.class"],
    );

    let seen = probe.wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("answer 2 tick "));
    let (answer, tick) = last_answer(&probe).expect("the probe prints an answer");
    assert!(
        seen.is_some(),
        "the JVM never printed the new value after a reload that reported success.\n  \
         reload said: {reloaded}\n  \
         last line: answer {answer} tick {tick} — if the tick is still advancing the swap did nothing \
         (a bug in reload_class); if it stopped, the probe froze (a harness or resume bug).\n  \
         last 5 lines: {:?}",
        probe.output().iter().rev().take(5).collect::<Vec<_>>(),
    );
    // Nothing was suspended at any point, so the swap landed on a JVM that never stopped running.
    assert!(tick > after_dry.1, "the probe must have kept ticking across the swap, not been frozen by it");
}

/// SWAP-2 (#61): the residue is reported at the end of the session that caused it.
///
/// This is the mitigation that let SWAP-1 keep a single read-only gate instead of a third permission
/// axis. The argument was that reporting an unrepairable side effect is more honest than a mode nobody
/// remembers to set — which is only true while the report exists, so it is worth a test that drives the
/// whole path rather than only the renderer.
///
/// Asserted through the real tool replies because that is where the claim is made. `list_sessions` is
/// checked too: a session *someone else* left behind is the case that matters, and the listing is the
/// only place a third party can discover it.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_reloaded_class_is_reported_as_outstanding_when_the_session_ends() {
    let Some(jdk) = jdk_or_skip("a_reloaded_class_is_reported_as_outstanding_when_the_session_ends") else {
        return;
    };
    let probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let classes = swap_probe_returning(&jdk, 2);
    let root = classes.path().display().to_string();

    // Before any reload, nothing anywhere should mention outstanding bytecode.
    let quiet = server.call("debug.list_sessions", serde_json::json!({}));
    assert!(
        !quiet.contains("still reloaded"),
        "a session that has redefined nothing must not be flagged:\n{quiet}"
    );

    // A dry run must not create residue either — it ships nothing, which is exactly why it is allowed
    // in a read-only session.
    server.call(
        "debug.reload_class",
        serde_json::json!({"class_name": "SwapProbe", "class_roots": [root], "dry_run": true}),
    );
    let after_dry = server.call("debug.list_sessions", serde_json::json!({}));
    assert!(
        !after_dry.contains("still reloaded"),
        "a dry run installs nothing, so it must leave no residue:\n{after_dry}"
    );

    server.call("debug.reload_class", serde_json::json!({"class_name": "SwapProbe", "class_roots": [root]}));

    let listed = server.call("debug.list_sessions", serde_json::json!({}));
    assert_contains_all(
        "the listing flags a session holding installed bytecode",
        &listed,
        &["still reloaded"],
    );

    let disconnected = server.call("debug.disconnect", serde_json::json!({}));
    assert_contains_all(
        "disconnecting names the class, says nothing here can undo it, and names the remedy",
        &disconnected,
        &["SwapProbe", "redeploy", "once"],
    );
    assert!(
        disconnected.contains("may still hold the old code"),
        "no frame was popped, so the report must say the running frames may be masking the swap:\n\
         {disconnected}"
    );
}

/// The other half of SWAP-2, and the one that keeps the report from being noise: a session that
/// redefined nothing must say nothing about redefinitions. A line on every disconnect is how a reader
/// learns to skip the reply, at which point the loud case stops working too.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_session_that_reloaded_nothing_mentions_no_residue_on_disconnect() {
    let Some(jdk) = jdk_or_skip("a_session_that_reloaded_nothing_mentions_no_residue_on_disconnect") else {
        return;
    };
    let probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let disconnected = server.call("debug.disconnect", serde_json::json!({}));

    assert!(
        !disconnected.contains("redeploy") && !disconnected.contains("still running bytecode"),
        "nothing was redefined, so the disconnect must not mention residue at all:\n{disconnected}"
    );
}

/// SWAP-1 (#58): the refusals, which are most of the feature's worth.
///
/// `HotSpot` accepts method **body** changes only, and the three edits below are the ones a developer
/// actually makes by accident — add a field, add a method, change a modifier. What the JVM answers is
/// accurate and useless (`SCHEMA_CHANGE_NOT_IMPLEMENTED`), and an agent handed that will recompile and
/// re-try a swap that can never land. Each assertion here is that the reply names the edit and says a
/// redeploy is the route, not that some error occurred.
///
/// The probe is checked after every refusal: `RedefineClasses` is all-or-nothing, and a refusal that
/// had partially landed would be far worse than one that reported badly.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_swap_hotspot_cannot_accept_names_the_edit_and_says_a_redeploy_is_needed() {
    let Some(jdk) = jdk_or_skip("a_swap_hotspot_cannot_accept_names_the_edit_and_says_a_redeploy_is_needed")
    else {
        return;
    };
    let probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // (what the edit does, how to make it, what the reply must say)
    let cases: [(&str, SourceEdit, &str); 3] = [
        (
            "adds a field",
            Box::new(|src: String| {
                src.replace(
                    "    static int answer() {",
                    "    static int extra = 7;\n\n    static int answer() {",
                )
            }),
            "FIELD",
        ),
        (
            "adds a method",
            Box::new(|src: String| {
                src.replace(
                    "    static int answer() {",
                    "    static int added() { return 9; }\n\n    static int answer() {",
                )
            }),
            "ADDED a method",
        ),
        (
            "changes a method modifier",
            Box::new(|src: String| {
                src.replace("    static int answer() {", "    static synchronized int answer() {")
            }),
            "METHOD modifier",
        ),
    ];

    for (what, edit, expected) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        jdk.compile_probe_variant("SwapProbe", dir.path(), edit).expect("compile the variant");
        let refusal = server.call(
            "debug.reload_class",
            serde_json::json!({"class_name": "SwapProbe", "class_roots": [dir.path().display().to_string()]}),
        );
        assert_contains_all(
            &format!("a variant that {what} is refused with advice, not a bare code"),
            &refusal,
            &["was NOT reloaded", expected, "redeploy", "all-or-nothing"],
        );

        // All-or-nothing is a claim about the debuggee, so read it from the debuggee: `answer()` still
        // returns 1, and nothing about the class changed.
        let before = last_answer(&probe).expect("the probe prints an answer");
        assert_eq!(before.0, 1, "a refused swap must leave the old bytecode running: {refusal}");
        assert!(
            probe.wait_for_line(EVENT_TIMEOUT, |l| swap_tick(l).is_some_and(|t| t > before.1)).is_some(),
            "the probe stopped running after a refused swap — a refusal must cost the debuggee nothing"
        );
    }

    // The local guard, which never reaches the JVM: a path that resolved to something that is not a
    // class file. Distinguished from a JVM refusal on purpose — the fix is a different one.
    let not_a_class = tempfile::tempdir().expect("tempdir");
    std::fs::write(not_a_class.path().join("SwapProbe.class"), b"public class SwapProbe {}").expect("write");
    let refused = server.call(
        "debug.reload_class",
        serde_json::json!({
            "class_name": "SwapProbe",
            "class_roots": [not_a_class.path().display().to_string()],
        }),
    );
    assert_contains_all(
        "a file that is not a class file is refused before the wire",
        &refused,
        &["0xCAFEBABE", "Nothing was sent"],
    );
}

/// SWAP-1 (#58), piece 4: the whole point of the feature, and the part that looks broken without
/// `debug.pop_frame` — a request **suspended at a breakpoint** gets the fix without being re-issued.
///
/// The sequence is the one a developer runs: stop inside the method, discover it is wrong, swap it, and
/// re-enter it. What makes it worth a test rather than a paragraph is the middle step: immediately after
/// a successful redefinition the suspended frame is still executing the OLD bytecode, so a caller who
/// resumes here sees their fix do nothing on this request. The reload's reply has to say that, and the
/// pop has to fix it.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_reload_of_the_method_you_are_stopped_in_takes_effect_when_the_frame_is_popped() {
    let Some(jdk) =
        jdk_or_skip("a_reload_of_the_method_you_are_stopped_in_takes_effect_when_the_frame_is_popped")
    else {
        return;
    };
    let probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let line = probe_line(&probe_source("SwapProbe"), "// SWAP_VALUE");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "SwapProbe", "line": line}));
    assert!(
        server.wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT).is_some(),
        "the breakpoint inside SwapProbe.answer never fired; probe said {:?}",
        probe.output().iter().rev().take(5).collect::<Vec<_>>(),
    );

    let classes = swap_probe_returning(&jdk, 3);
    let reloaded = server.call(
        "debug.reload_class",
        serde_json::json!({
            "class_name": "SwapProbe",
            "class_roots": [classes.path().display().to_string()],
        }),
    );
    assert_contains_all(
        "the reload warns that the suspended thread is inside the class it just replaced",
        &reloaded,
        &["✅ Reloaded SwapProbe", "INSIDE SwapProbe", "debug.pop_frame", "stop point(s) are armed"],
    );

    // The outermost frame cannot be popped, and the refusal must say so rather than reporting whatever
    // JDWP's NO_MORE_FRAMES looks like from the wire. main → tick → answer, so #2 is `main`.
    let too_far = server.call("debug.pop_frame", serde_json::json!({"frame": 2}));
    assert_contains_all(
        "popping the outermost frame is refused with the reason",
        &too_far,
        &["outermost frame", "NO_MORE_FRAMES"],
    );

    // Clear the stop point first: leaving it armed re-suspends inside `answer()` the moment the popped
    // call is re-executed, and the test would then be waiting for output the probe cannot reach.
    server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": "bp_1"}));
    let popped = server.call("debug.pop_frame", serde_json::json!({"frame": 0}));
    // The method is reported as OBSOLETE rather than by name, and that is the JVM's own answer rather
    // than a gap in this tool: after a redefinition `HotSpot` gives the suspended frame method id 0,
    // because the code it entered with is no longer part of the class. Asserted deliberately — it is
    // the clearest available evidence that the frame really was running the pre-swap bytecode, which is
    // the fact this whole test exists to establish.
    assert_contains_all(
        "the pop reports where the thread now is and what it does not undo",
        &popped,
        &["Popped frame #0", "SwapProbe.<obsolete method", "CALL SITE", "Side effects are not rewound"],
    );

    server.call("debug.continue", serde_json::json!({}));
    let seen = probe.wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("answer 3 tick "));
    assert!(
        seen.is_some(),
        "re-entering the popped method did not run the new bytecode.\n  reload said: {reloaded}\n  \
         pop said: {popped}\n  last 5 lines: {:?}",
        probe.output().iter().rev().take(5).collect::<Vec<_>>(),
    );

    // SWAP-2: this is the one scenario that reaches the *popped* branch of the residue report, and it is
    // reached here rather than in a second test because a pop is only meaningful after a reload — the
    // setup above is the whole cost of testing it. The distinction being asserted is not cosmetic: an
    // un-popped swap may never have reached the frames that were already running, and this one certainly
    // did, which is a different thing for the next person to check.
    let disconnected = server.call("debug.disconnect", serde_json::json!({}));
    assert_contains_all(
        "the residue report survives a pop and names the class",
        &disconnected,
        &["SwapProbe", "redeploy"],
    );
    assert!(
        disconnected.contains("the new code is live"),
        "a frame was popped, so the report must say the swap is certainly live rather than possibly \
         masked:\n{disconnected}"
    );
}

/// SWAP-1 (#58) under SAFE-3: redefinition is the most far-reaching mutation this server can perform —
/// on a shared instance it is an unannounced deploy — so read-only must refuse it, and refuse
/// `pop_frame` with it.
///
/// `dry_run` is the deliberate exception and is asserted as such rather than left to be discovered: it
/// ships nothing, and "what would this swap do" is a question a read-only session should be able to ask.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn read_only_refuses_a_reload_but_still_answers_a_dry_run() {
    let Some(jdk) = jdk_or_skip("read_only_refuses_a_reload_but_still_answers_a_dry_run") else { return };
    let probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start_with_env(&[("JDWP_READONLY", "1")]).expect("start server");
    server.attach(probe.port);

    let classes = swap_probe_returning(&jdk, 4);
    let root = classes.path().display().to_string();

    let refused = server
        .call("debug.reload_class", serde_json::json!({"class_name": "SwapProbe", "class_roots": [&root]}));
    assert_contains_all(
        "a reload is refused in a read-only session, and says how to lift the guard",
        &refused,
        &["Read-only", "unannounced deploy", "JDWP_READONLY"],
    );
    assert_contains_all(
        "pop_frame is refused too",
        &server.call("debug.pop_frame", serde_json::json!({"thread_id": "0x1"})),
        &["Read-only"],
    );

    assert_contains_all(
        "a dry run still answers, because it ships nothing",
        &server.call(
            "debug.reload_class",
            serde_json::json!({"class_name": "SwapProbe", "class_roots": [root], "dry_run": true}),
        ),
        &["Dry run", "SwapProbe.class"],
    );

    // The refusal is a claim about the debuggee: it is still running what it was running.
    let (answer, tick) = last_answer(&probe).expect("the probe prints an answer");
    assert_eq!(answer, 1, "a refused reload must not have changed anything");
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| swap_tick(l).is_some_and(|t| t > tick)).is_some(),
        "the probe stopped running after a refused reload"
    );
}

/// DISC-7 (#59): the detector, against a JVM whose bytecode is provably older than the build on disk.
///
/// Both directions in one test, because each is meaningless without the other. A detector that always
/// says STALE passes the drift half; one that always says MATCH passes the clean half. The clean half is
/// the one that decides whether anybody keeps using it: a detector that cries stale on a current build
/// is ignored within a day.
///
/// The drift is made the way it happens in a redeploy loop — **the same source, recompiled after lines
/// moved** — rather than by editing a method's behaviour. That is the case `debug.source` provably
/// cannot see: same class, same `SwapProbe.java`, older bytecode.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_stale_build_is_detected_and_a_current_one_is_not() {
    let Some(jdk) = jdk_or_skip("a_stale_build_is_detected_and_a_current_one_is_not") else { return };
    let probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // The control: the very source the running JVM was built from, compiled again. Anything but "match"
    // here is a false positive, and a false positive is what kills a detector.
    let current = tempfile::tempdir().expect("tempdir");
    jdk.compile_probe_variant("SwapProbe", current.path(), |src| {
        // `compile_probe_variant` refuses a no-op edit (a stale copy would pass for the wrong reason),
        // so change a comment: it moves no line and alters no bytecode.
        src.replace("// SWAP_VALUE", "// SWAP_VALUE (control build)")
    })
    .expect("recompile the unmodified probe");
    let clean = server.call(
        "debug.check_stale",
        serde_json::json!({
            "class_name": "SwapProbe",
            "class_roots": [current.path().display().to_string()],
        }),
    );
    assert_contains_all(
        "a rebuild of the running source must not be reported as drift",
        &clean,
        &["matches your build", "SwapProbe.class"],
    );
    assert!(!clean.contains("STALE"), "false positive on a current build: {clean}");
    // The claim has to be the one that was checked, or the next reader will over-trust it.
    assert!(clean.contains("byte-for-byte"), "the reply must bound its own claim: {clean}");

    // Now the drift: two lines inserted above `answer()`, which moves every line number in it. Nothing
    // about the class's shape changes, and `debug.source` would report the same `SwapProbe.java` for
    // both — this is precisely the half #31 could not answer.
    let stale = tempfile::tempdir().expect("tempdir");
    jdk.compile_probe_variant("SwapProbe", stale.path(), |src| {
        src.replace("    static int answer() {", "    // shifted\n    // shifted\n    static int answer() {")
    })
    .expect("compile the shifted probe");
    let drifted = server.call(
        "debug.check_stale",
        serde_json::json!({
            "class_name": "SwapProbe",
            "class_roots": [stale.path().display().to_string()],
        }),
    );
    assert_contains_all(
        "a build whose lines moved is reported stale, naming the method and one difference",
        &drifted,
        &["🚨 STALE", "answer()I", "the JVM has line", "your build has line", "debug.reload_class"],
    );

    // A method the build has and the JVM does not is a class SHAPE change, reported separately because
    // the remedy differs — a hot reload cannot install it.
    let widened = tempfile::tempdir().expect("tempdir");
    jdk.compile_probe_variant("SwapProbe", widened.path(), |src| {
        src.replace(
            "    static int answer() {",
            "    static int added() { return 9; }\n\n    static int answer() {",
        )
    })
    .expect("compile the widened probe");
    let shape = server.call(
        "debug.check_stale",
        serde_json::json!({
            "class_name": "SwapProbe",
            "class_roots": [widened.path().display().to_string()],
        }),
    );
    assert_contains_all(
        "a method only the build has is reported as a shape change",
        &shape,
        &["BUILD declares", "added()I", "redeploy, not a swap"],
    );

    // A root that holds a different class is a wrong path, not drift, and must not be reported as it.
    let wrong = tempfile::tempdir().expect("tempdir");
    jdk.compile_probe("ForceProbe", wrong.path()).expect("compile a different probe");
    std::fs::rename(wrong.path().join("ForceProbe.class"), wrong.path().join("SwapProbe.class"))
        .expect("rename");
    let mismatched = server.call(
        "debug.check_stale",
        serde_json::json!({
            "class_name": "SwapProbe",
            "class_roots": [wrong.path().display().to_string()],
        }),
    );
    assert_contains_all(
        "a class file declaring another class is a wrong path, not drift",
        &mismatched,
        &["declares ForceProbe", "Nothing was compared"],
    );
}

/// DISC-7 (#59): the answer that must never be dressed up as a pass.
///
/// A class compiled `-g:none` has no line tables, so there is nothing to compare and the honest reply is
/// "cannot tell". Reporting that as a match would be the worst possible outcome for this tool — it is
/// the exact reassurance it exists to withhold — and it is easy to write by accident, since zero
/// differences is indistinguishable from a clean build if you only count differences.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_class_with_no_debug_info_cannot_be_checked_and_says_so() {
    let Some(jdk) = jdk_or_skip("a_class_with_no_debug_info_cannot_be_checked_and_says_so") else { return };
    let probe = Probe::launch_stripped(&jdk, "StrippedProbe").expect("launch StrippedProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // Compiled with -g here, deliberately: the JVM side has no line tables (the probe is launched
    // `-g:none`), so this is the *asymmetric* case — a build that could be compared against and a
    // running class that cannot. It must still answer "cannot tell" rather than counting zero
    // differences as agreement.
    let built = tempfile::tempdir().expect("tempdir");
    jdk.compile_probe("StrippedProbe", built.path()).expect("compile StrippedProbe with debug info");

    let verdict = server.call(
        "debug.check_stale",
        serde_json::json!({
            "class_name": "StrippedProbe",
            "class_roots": [built.path().display().to_string()],
        }),
    );
    assert_contains_all(
        "a class with no line tables is reported as unanswerable, not as a match",
        &verdict,
        &["Cannot tell", "-g:none", "NOT a report that the build matches"],
    );
    assert!(!verdict.contains("✅"), "a skipped comparison must not read as a pass: {verdict}");
}

/// TEST-21 ([#56](https://github.com/YgorPerez/java-debugging-mcp/issues/56)), first two acceptance
/// criteria, for the world where the probe's JVM is **gone**.
///
/// This issue has never been reproduced on demand — one occurrence in a 20-run soak, none in 15 CI legs
/// since. So the failure is *manufactured* instead: kill the JVM and attach to the port it used, which is
/// the same state a probe that died on its own would leave. What is asserted is not that attach fails —
/// of course it does — but that the diagnosis **names which world this is**, because
/// `Connection refused` alone is what made both sightings undiagnosable.
///
/// The reply the MCP server gives is byte-identical to the live-session case below
/// (`Failed to connect: IO error: Connection refused (os error 111)`), and so is "nothing is listening".
/// The JVM's exit status is the only thing that separates them, which is why it is read at all.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_refused_attach_says_so_when_the_probe_jvm_has_died() {
    let Some(jdk) = jdk_or_skip("a_refused_attach_says_so_when_the_probe_jvm_has_died") else { return };
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    // Killed *after* a successful launch, so the log carries the banner. That is the point: the banner
    // proves the port was bound once, so its presence must not be read as "it bound it, therefore a
    // session holds it" when the JVM behind it has since died.
    probe.kill_and_wait();

    let mut server = Server::start().expect("start server");
    let out = server.call("debug.attach", serde_json::json!({"host": "127.0.0.1", "port": probe.port}));
    assert!(!out.contains("Connected"), "attaching to a dead JVM's port must not succeed: {out}");

    let diagnosis = probe.diagnose_refusal(&out);
    assert_contains_all(
        "a refused attach names the dead JVM",
        &diagnosis,
        &["THE PROBE JVM IS GONE", "port went with it"],
    );
    assert!(
        !diagnosis.contains("THE PORT WAS NEVER BOUND") && !diagnosis.contains("SESSION IS ALREADY TAKEN"),
        "a dead JVM must not be reported as a port race — that is the wrong half of the system:\n{diagnosis}"
    );
}

/// The same, for the world where a **live handshaked session** already owns the port.
///
/// #55 measured that a second connection to a probe with a live session is refused. One step it did not
/// take: the agent also *stops listening* for that session's life, so this world reports **nothing
/// listening on the port** — and the first cut of this diagnosis keyed its "the port is taken" branch on
/// something being listening, which made that branch unreachable for the one mechanism it named. This
/// test is what caught that, and is why it asserts the verdict rather than the evidence.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_refused_attach_says_the_session_is_taken_when_one_is_already_live() {
    let Some(jdk) = jdk_or_skip("a_refused_attach_says_the_session_is_taken_when_one_is_already_live") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut holder = Server::start().expect("start the holding server");
    let held = probe.attach(&mut holder);
    assert!(held.contains("Connected"), "the first attach must succeed: {held}");

    let mut second = Server::start().expect("start the second server");
    let out = second.call("debug.attach", serde_json::json!({"host": "127.0.0.1", "port": probe.port}));
    assert!(!out.contains("Connected"), "a second attach to a held session must not succeed: {out}");

    let diagnosis = probe.diagnose_refusal(&out);
    assert_contains_all(
        "a refused attach names the live session",
        &diagnosis,
        &["THE SESSION IS ALREADY TAKEN", "closes the listener"],
    );
    assert!(
        !diagnosis.contains("THE PROBE JVM IS GONE") && !diagnosis.contains("THE PORT WAS NEVER BOUND"),
        "a held session must not be reported as a dead JVM or an unbound port:\n{diagnosis}"
    );
}

/// TEST-16 (#45): the two worlds behind *"the caller never observed the forced value"* must not read
/// alike, because one is a debugger bug and the other is the harness failing to resume the VM.
///
/// Needs no JVM: the discriminator is a line count, and that is the whole point — it is cheap enough that
/// there is no excuse for it having been verified once by hand and never again.
#[test]
fn a_missing_forced_value_says_whether_the_debuggee_ran_at_all() {
    let never_ran = resume_verdict(7, 7);
    let ran = resume_verdict(7, 9);

    assert!(
        never_ran.contains("never ran again") && never_ran.contains("NOT as"),
        "a probe that printed nothing must be reported as a resume failure, and must say plainly that \
         this is not force_return's doing: {never_ran}"
    );
    assert!(
        ran.contains("DID run") && ran.contains("without changing what the caller received"),
        "a probe that ran and still did not produce the value is the accusation this test exists to \
         make, and must be worded as one: {ran}"
    );
    // The two verdicts must not be confusable by a reader skimming for a keyword. This is the assertion
    // that would have caught the shape of bug this repo shipped once already: an aside that was present
    // in both worlds, asserted on, and therefore load-bearing for nothing.
    assert_ne!(never_ran, ran);
    assert!(
        !never_ran.contains("DID run") && !ran.contains("never ran again"),
        "the verdicts overlap, so a failure could match both:\n  {never_ran}\n  {ran}"
    );
}

/// TEST-24 ([#65](https://github.com/YgorPerez/java-debugging-mcp/issues/65)): a debuggee that dies
/// mid-session must be reported as a debuggee that died, all the way up to the MCP reply.
///
/// The CI sighting was `Failed to list classes: Protocol error: Reply channel closed` — a message four
/// unrelated worlds produced, with the real `io::Error` logged at a level nothing enabled and then
/// discarded. `e0db036` fixed that in `jdwp-client`, and its own tests cover the event loop directly. This
/// covers the part those cannot: that the cause survives the handler, the MCP serialisation and the
/// transport, and reaches the caller who has to act on it.
///
/// Manufactured rather than waited for, because the sighting was one leg in 24 and this is the same state:
/// the JVM is killed after a successful attach, which is what a probe that dies on its own leaves behind.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_question_asked_after_the_debuggee_dies_names_the_lost_connection() {
    let Some(jdk) = jdk_or_skip("a_question_asked_after_the_debuggee_dies_names_the_lost_connection") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    let attached = probe.attach(&mut server);
    assert!(attached.contains("Connected"), "the session has to exist before it can be lost: {attached}");

    probe.kill_and_wait();

    // Either ordering is fine and both are the same fact: the command may be written before the loop
    // notices EOF (then the pending reply is answered as the loop exits) or after (then it is refused on
    // the way in). Retried a few times only because the *first* call is the one that races; a session
    // whose debuggee is gone cannot recover, so a later call must say the same thing.
    let mut reply = String::new();
    for _ in 0..5 {
        reply = server.call("debug.list_classes", serde_json::json!({"filter": "WatchProbe"}));
        if reply.contains("closed") || reply.contains("Reply channel") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    assert!(
        !reply.contains("Reply channel closed"),
        "the message this issue was filed on is back: it names none of the four worlds that produce it\n  \
         got: {reply}"
    );
    assert_contains_all(
        "a dead debuggee is reported as one",
        &reply,
        &["connection to the debuggee closed", "reading from the debuggee failed"],
    );
}

/// TEST-21 (#56): every world a refused attach can be in, including the two that **no test can build with
/// a real JVM** — a stranger winning `free_port`'s race, and this JVM losing it.
///
/// Those two shipped with no coverage at all, and the first cut of this diagnosis had its tree backwards,
/// so "it looks right" has already been shown to be worth nothing here. No JVM, no ports, no timing.
#[test]
fn every_refused_attach_world_gets_its_own_verdict() {
    let dead = refusal_verdict(JvmState::Exited("signal: 9 (SIGKILL)".into()), false, true);
    let taken = refusal_verdict(JvmState::Alive, false, true);
    let unbound = refusal_verdict(JvmState::Alive, false, false);
    let stranger = refusal_verdict(JvmState::Alive, true, false);
    let unknown = refusal_verdict(JvmState::Unknown("no such process".into()), false, false);

    assert!(dead.contains("THE PROBE JVM IS GONE") && dead.contains("signal: 9"), "{dead}");
    assert!(taken.contains("THE SESSION IS ALREADY TAKEN"), "{taken}");
    assert!(unbound.contains("THE PORT WAS NEVER BOUND"), "{unbound}");
    assert!(stranger.contains("SOMETHING ELSE HOLDS THE PORT"), "{stranger}");
    assert!(unknown.contains("UNDETERMINED") && unknown.contains("no such process"), "{unknown}");

    // A dead JVM outranks every other fact: the port is gone either way, and reporting it as a race sends
    // the reader to the wrong half of the system. Asserted rather than left to arm order.
    for (listening, announced) in [(false, false), (false, true), (true, false), (true, true)] {
        let v = refusal_verdict(JvmState::Exited("exit status: 1".into()), listening, announced);
        assert!(
            v.contains("THE PROBE JVM IS GONE"),
            "a dead JVM must outrank listening={listening}/announced={announced}: {v}"
        );
    }

    // The four verdicts must be mutually exclusive, or a reader grepping for one could match another.
    let all = [&dead, &taken, &unbound, &stranger];
    let headlines = [
        "THE PROBE JVM IS GONE",
        "THE SESSION IS ALREADY TAKEN",
        "THE PORT WAS NEVER BOUND",
        "SOMETHING ELSE HOLDS THE PORT",
    ];
    for (i, verdict) in all.iter().enumerate() {
        let matched: Vec<_> = headlines.iter().filter(|h| verdict.contains(**h)).collect();
        assert_eq!(matched.len(), 1, "verdict {i} matches {matched:?}, so the worlds overlap:\n{verdict}");
    }
}

/// TEST-23 ([#64](https://github.com/YgorPerez/java-debugging-mcp/issues/64)): what the event buffer does
/// when a hit arrives twice — the debugger-side shape of the failure CI caught and this box will not
/// reproduce.
///
/// The sighting was `[pending] 2 older event(s)` where the test staged one, on a JDK this machine does not
/// have. Rather than soak for it, [`EventFault::DuplicateKind`] puts the extra event on the wire, which is
/// the debugger would see if two armed requests had matched one location. What that pins down is everything
/// downstream of the extra event: that the buffer keeps both, that the backlog count follows, and that
/// nothing silently coalesces them — so when the real thing recurs, the reading is about *why a second
/// event existed* rather than about whether the buffer can be trusted.
///
/// It does **not** show that a JVM sends an event twice. See [`EventFault::DuplicateKind`] for that limit.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_duplicated_hit_is_buffered_twice_rather_than_coalesced() {
    let Some(jdk) = jdk_or_skip("a_duplicated_hit_is_buffered_twice_rather_than_coalesced") else { return };
    let probe = Probe::launch(&jdk, "ExcProbe").expect("launch ExcProbe");
    let relay = FaultRelay::start_with_events(
        probe.port,
        vec![],
        Some(EventFault::DuplicateKind { kind: EVENT_KIND_BREAKPOINT, times: 1 }),
    )
    .expect("start the fault relay");
    let mut server = Server::start().expect("start server");
    server.attach(relay.port);

    let line = probe_line(&probe_source("ExcProbe"), "// BP2");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "ExcProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in ExcProbe.main never fired");

    // The instrument first. Without this the assertion below reports `got 1` for two opposite worlds —
    // a fault that never fired and a debugger that coalesced the copies — and the first cut of this test
    // failed in CI with exactly that ambiguity, because it matched events by position and the leading
    // event on that runner was not the breakpoint.
    assert_eq!(
        relay.duplicated(),
        1,
        "the fault never fired, so this test proves nothing about the buffer: no composite event of kind \
         {EVENT_KIND_BREAKPOINT} (breakpoint) crossed the relay. Fix the instrument, not the assertion."
    );

    // One hit, delivered twice. Both copies must be in the buffer: a debugger that quietly dropped the
    // second would be *hiding* the anomaly, which is worse than reporting a backlog nobody expected.
    let buffered = server.call("debug.get_last_event", serde_json::json!({"limit": 10}));
    let hits = buffered.matches("\"event\":\"breakpoint\"").count();
    assert_eq!(
        hits, 2,
        "the fault fired, so the debugger coalesced or dropped a copy — got {hits}:\n{buffered}"
    );

    // And the newest-event view must announce the backlog rather than pretend there is one event.
    let latest = server.last_event();
    assert!(
        latest.contains("[pending] 1 older event"),
        "an unread older copy must be announced as pending: {latest}"
    );

    // Now finish the sequence the flaky test performs, and the result is CI's failure **verbatim**: three
    // events, newest a step, `[pending] 2 older event(s)` where one was staged. That is the value of this
    // simulation — the fingerprint is reproducible on demand, so the remaining question for #64 is only
    // whether a real JVM can deliver a hit twice, and not what such a delivery would look like.
    server.call("debug.step_over", serde_json::json!({}));
    server.wait_for_event("\"event\":\"step\"", EVENT_TIMEOUT).expect("step never reported");
    let after_step = server.last_event();
    assert_contains_all(
        "the injected duplicate reproduces the CI fingerprint exactly",
        &after_step,
        &["\"event\":\"step\"", "[pending] 2 older event(s)"],
    );
}

/// TEST-24 ([#65](https://github.com/YgorPerez/java-debugging-mcp/issues/65)) again, severed at the wire
/// instead of by killing the JVM.
///
/// The kill-based test proves the same message, and this one is the more robust instrument for three
/// reasons: the connection dies at a moment the test picks rather than whenever a killed process is
/// reaped; the probe survives, so it can still be questioned afterwards; and it reproduces the shape the
/// CI sighting actually had — a **connection** that died under a JVM that was very much alive — rather
/// than a dead process, which is a different world (see `refusal_verdict`).
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_severed_connection_is_reported_as_one_while_the_debuggee_lives_on() {
    let Some(jdk) = jdk_or_skip("a_severed_connection_is_reported_as_one_while_the_debuggee_lives_on") else {
        return;
    };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let relay = FaultRelay::start(probe.port, vec![]).expect("start the fault relay");
    let mut server = Server::start().expect("start server");
    let attached = server.attach(relay.port);
    assert!(attached.contains("Connected"), "the session has to exist before it can be lost: {attached}");

    relay.sever();

    let mut reply = String::new();
    for _ in 0..10 {
        reply = server.call("debug.list_classes", serde_json::json!({"filter": "WatchProbe"}));
        if reply.contains("closed") || reply.contains("Reply channel") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    assert!(
        !reply.contains("Reply channel closed"),
        "the message #65 was filed on is back, naming none of the worlds that produce it: {reply}"
    );
    assert_contains_all(
        "a severed connection is reported as one",
        &reply,
        &["connection to the debuggee closed"],
    );
    // The JVM was never touched, so it must still be running — otherwise this test is quietly the
    // kill-based one and proves nothing the other did not.
    assert!(
        probe.output().iter().any(|l| l.contains("Listening for transport")),
        "the probe should still be alive and its log intact"
    );
}

/// EVAL-6 (#70): `debug.evaluate_chain` walks a chain and names the link that went null, in one call.
///
/// Three shapes, because they fail differently and only the first is easy:
///
/// 1. **Null at the end.** Every link resolves; the last one is null. A plain `debug.evaluate` already
///    answers this — the value is `null` — so what is added is the table showing the links that were fine.
/// 2. **Null in the middle.** The links after it cannot be evaluated at all, and `debug.evaluate` reports
///    a null receiver: the question restated, not answered. This must be a *report*, not an error, and it
///    must say how many links it never reached rather than leaving their absence to read as "fine".
/// 3. **Nothing null.** The tool must say so plainly instead of leaving a reader to scan for a `✘` that
///    is not there — an empty collection and a null are the two answers being told apart.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn evaluate_chain_names_the_link_that_went_null() {
    let Some(jdk) = jdk_or_skip("evaluate_chain_names_the_link_that_went_null") else { return };
    let probe = Probe::launch(&jdk, "ChainProbe").expect("launch ChainProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let line = probe_line(&probe_source("ChainProbe"), "// BP_CHAIN");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "ChainProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in ChainProbe.inspect never fired");

    // 1. The issue's own chain: null at the far end, after a collection subscript.
    let tail = server.call(
        "debug.evaluate_chain",
        serde_json::json!({
            "expression": "reserva.getCircuitoParametro().getConfig().getConfigUhList()[0].getSqQuarto()",
        }),
    );
    assert_contains_all(
        "a null at the end is named, with the links that resolved shown above it",
        &tail,
        &["✘", "getSqQuarto()", "null at link 5 of 5", "✔", "getConfigUhList()[0]"],
    );

    // 2. Null in the middle — the case a plain evaluate cannot answer. It must not error, and it must
    //    account for the links it never reached.
    let middle = server.call(
        "debug.evaluate_chain",
        serde_json::json!({"expression": "reserva.getMissing().getConfig().getConfigUhList()"}),
    );
    assert_contains_all(
        "a mid-chain null is a report, not an error, and says what was never evaluated",
        &middle,
        &["null at link 2 of 4", "getMissing()", "2 link(s) after it were never evaluated"],
    );
    // Checked against the table ROWS, not the whole reply — the header echoes the expression the caller
    // passed, so every link name is in there by construction.
    let rows: Vec<&str> = middle.lines().filter(|l| l.contains('✔') || l.contains('✘')).collect();
    assert!(
        !rows.iter().any(|l| l.contains("getConfigUhList") || l.contains("getConfig()")),
        "links after the null must not appear as walked steps — nothing evaluated them: {rows:?}"
    );

    // 3. A chain with nothing null says so, rather than leaving a reader to hunt for a ✘.
    let fine = server.call(
        "debug.evaluate_chain",
        serde_json::json!({"expression": "reserva.getCircuitoParametro().getConfig()"}),
    );
    assert_contains_all("a clean chain is stated as clean", &fine, &["✅", "no link in this chain is null"]);
    assert!(!fine.contains('✘'), "nothing in this chain is null, so no link may be marked: {fine}");

    server.panic_reset();
}
