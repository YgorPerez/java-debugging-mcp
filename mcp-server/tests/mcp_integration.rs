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
    let mut probe = Probe::launch(&jdk, "EvalProbe").expect("launch EvalProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe =
        Probe::launch_running(&jdk, "WatchProbe", |l| tick_index(l).is_some()).expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
        "unfetched class",
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
    probe.attach(&mut server);

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
    probe.attach(&mut server);
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
    // BP-7 (#115) must not tell a family's member to re-arm after a redeploy. A member holds no standing
    // watch of its own because the FAMILY holds one, and a redeploy's copy matches the pattern and is
    // armed as a new member — so that sentence would be false here, and false is worse than absent.
    assert!(
        !list.contains("NOT watching for later copies"),
        "a wildcard family's member is covered by the family's own class-prepare watch, so it must not \
         claim it needs re-arming after a redeploy: {list}"
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
    probe.attach(&mut server);
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
        // TEST-31 (#114): this used to report "the family never armed FamilyGamma" for BOTH failures, and
        // they are different bugs with different fixes. The wait is on a *trace*, so an arm that lands and
        // never fires looks identical to an arm that never happened — and the once-observed failure was the
        // former, while the message sent the reader after the latter. The listing already holds the
        // evidence to tell them apart, so read it rather than guessing.
        let listed = server.call("debug.list_stop_points", serde_json::json!({}));
        let armed = listed.lines().any(|l| l.contains("FamilyGamma") && l.contains("] Family"));
        let hits_zero =
            listed.lines().skip_while(|l| !l.contains("FamilyGamma")).take(6).any(|l| l.trim() == "Hits: 0");
        // The probe prints one line per gamma invocation (TEST-31), which is the fact that splits the two
        // failures apart. Without it, `Hits: 0` on an armed stop point is silence about two different bugs.
        let out = probe.output();
        let invocations = out.iter().filter(|l| l.starts_with("gamma invoked ")).count();
        // The worker's heartbeat (TEST-31): printed every tenth pass at the TOP of the loop, before the
        // calls that a cleared traced stop point could have left it suspended at. Its LAST value is what
        // separates "frozen" from "running but not reaching gamma".
        let beats = out.iter().filter(|l| l.starts_with("worker alive ")).count();
        let loaded_at = out.iter().position(|l| l == "gamma loaded");
        let beats_after_load =
            loaded_at.map_or(0, |i| out[i..].iter().filter(|l| l.starts_with("worker alive ")).count());
        let diagnosis = if armed && hits_zero && invocations == 0 && beats_after_load == 0 {
            "FamilyGamma IS armed, and the probe's worker is FROZEN — it printed no heartbeat after \
             `gamma loaded`, so it never even reached the top of the loop again. This is a suspended \
             worker, not a visibility problem: suspect a traced hit whose request was cleared while the \
             hit was in flight (TRACE-8's window, #72) leaving the thread held. That is a DEBUGGER bug."
        } else if armed && hits_zero && invocations == 0 {
            "FamilyGamma IS armed and the worker is ALIVE (heartbeats after `gamma loaded`) but never \
             invoked gamma. So it is looping past the `gammaHandle != null` check — the volatile handoff \
             is not being observed, which is a PROBE bug and not the debugger's."
        } else if armed && hits_zero {
            "FamilyGamma IS armed, the worker DID invoke it, and the stop point still did not fire. This \
             is a real debugger bug and the most interesting outcome: an armed JDWP request that misses \
             calls. Suspect the arm landing on the wrong reference type or bytecode index — our \
             bookkeeping says armed, and only the JVM can say whether the request exists."
        } else if armed {
            "FamilyGamma is armed and the tally is non-zero, so hits happened but no TRACE was recorded — \
             which is a trace-capture problem, not a watch problem."
        } else {
            "FamilyGamma is NOT armed: no member of the family names it. The watch did not come back when \
             the slot freed — a parked watch that never unparks is worse than one that was never parked. \
             This is the class-load watch."
        };
        panic!(
            "no traced hit on FamilyGamma within the timeout.\n  DIAGNOSIS: {diagnosis}\n  gamma \
             invocations the probe reported: {invocations}\n  heartbeats after `gamma loaded`: \
             {beats_after_load} (total {beats})\n  probe output: {out:?}\n  stop points: {listed}"
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
    let mut probe = Probe::launch(&jdk, "FamilyProbe").expect("launch FamilyProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
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
    // to catch it. The reply is kept because two of the readings below turn on it: a resume that did not
    // take leaves every thread suspended, which is a different failure from anything about class loading.
    let resumed = server.call("debug.continue", serde_json::json!({}));
    let hit = server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .unwrap_or_else(|| diagnose_clinit_miss(&mut server, &set, &launched, &resumed));
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

/// Which of the five readings a missed `<clinit>` breakpoint has to be told apart by, and the panic that
/// names the one observed (TEST-34, #118).
///
/// A function rather than a closure at the call site only because the readings outgrew
/// `clippy::too_many_lines` there. It ENDS the launched JVM to get at its stdout, so it is a failure path
/// and nothing else may call it.
fn diagnose_clinit_miss(server: &mut Server, set: &str, launched: &str, resumed: &str) -> String {
    // TEST-34 (#118). This message used to say `suspend=y did not hold the JVM`, and printed a listing
    // underneath that exonerated it in the same breath: the breakpoint DEFERRED — asserted twenty lines
    // above, and nothing can defer unless the JVM was held before its class loaded. So `suspend=y` is
    // the one mechanism this failure rules OUT, and the reader it sent to audit it lost the trip.
    //
    // What actually did not complete is prepare -> arm -> hit, after `debug.continue`. Two observations
    // separate its four readings, and both are already here: the stop point's own state, and whether
    // the probe printed from inside `<clinit>`. `debug.disconnect` is what carries a launched JVM's
    // captured stdout (`end_launched_jvm` — while it is alive, nothing else does), and ending a JVM we
    // are about to abandon costs nothing.
    let listing = server.call("debug.list_stop_points", serde_json::json!({}));
    // Read BEFORE the disconnect, which ends the JVM this asks about.
    //
    // This is what splits "the class never prepared" in two, and the split comes from the source rather than
    // from a sighting: `try_arm_deferred_breakpoints` arms and only THEN calls `resume_thread`, and every
    // `set_class_prepare` call site passes `SuspendPolicy::EventThread`. So if the JVM generated a
    // CLASS_PREPARE, it is holding the preparing thread waiting for a resume that only the arm's completion
    // sends. `debug.continue` cleared the launch suspend and no stop point has hit — that being the failure —
    // so a suspended thread here can only be that hold.
    let held = server.call("debug.list_threads", serde_json::json!({"only_suspended": true}));
    let farewell = server.call("debug.disconnect", serde_json::json!({}));
    let deferred = listing.contains("waiting for class load");
    let clinit_ran = farewell.contains("clinit ");
    let hits: u32 = listing
        .split("Hits: ")
        .nth(1)
        .and_then(|rest| rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok())
        .unwrap_or(0);
    // Whether the stop point is in the listing AT ALL, which none of the three readings below can see.
    // `deferred` is the absence of a string and `hits` defaults to 0, so a listing with no row for this
    // stop point reads exactly like an armed one that never fired — the same conflation this message was
    // rewritten to remove, one level down.
    let bp = stop_id(set, "bp_");
    let listed = bp.as_ref().is_some_and(|id| listing.contains(&format!("[{id}]")));
    let (n_held, n_total) = held_counts(&held);
    let reading = clinit_reading(listed, deferred, clinit_ran, hits, &held);
    panic!(
        "the breakpoint inside the static initialiser never fired within {EVENT_TIMEOUT:?}. \
         `suspend=y` is NOT the suspect — the stop point deferred, which only a held JVM allows.\n  \
         READING: {reading}\n  hits seen: {hits}, still deferred: {deferred}, <clinit> printed: \
         {clinit_ran}, listed as {bp:?}: {listed}, threads held: {n_held:?} of {n_total:?}\n  \
         launch reply: {launched}\n  debug.continue said: {resumed}\n  stop points: {listing}\n  \
         suspended before the disconnect: {held}\n  \
         the probe's own output, via disconnect: {farewell}"
    )
}

/// Which of the six readings a missed `<clinit>` breakpoint is, from facts already gathered.
///
/// Pure, and separate from [`diagnose_clinit_miss`] for the reason ADR-0034 exists: **two of these branches
/// cannot be staged against a real JVM at all.** A relay cannot sit in front of a JVM `debug.launch` started,
/// so nothing can delay its `CLASS_PREPARE` or stop it loading its own main class on demand — which is
/// precisely why #118 has never been diagnosed. A decision table nobody has exercised is worth as much as the
/// `suspend=y` sentence it replaced, so `the_clinit_readings_are_distinguishable` below drives every branch
/// off captured reply text instead.
fn clinit_reading(listed: bool, deferred: bool, clinit_ran: bool, hits: u32, held: &str) -> &'static str {
    // `N/M thread(s) suspended-only` — BOTH numbers matter, and getting this wrong was the first draft of
    // this split. "Something is suspended" does not mean the JVM is holding a preparing thread: `suspend=y`
    // holds every thread too, so a `debug.continue` that did not take looks identical at the boolean level.
    // The count separates them, because a CLASS_PREPARE hold under `EventThread` is ONE thread and a launch
    // suspension that was never cleared is all of them.
    let (n_held, n_total) = held_counts(held);
    let none_held = n_held == Some(0);
    let all_held = n_held.is_some() && n_held == n_total;
    match (listed, deferred, clinit_ran, hits) {
        (false, ..) => {
            "THE STOP POINT IS NOT IN THE LISTING AT ALL, so `still deferred: false` and `hits seen: 0` \
             below are the absence of a row rather than readings off one. Something cleared or disowned \
             it; none of the arming readings apply and the listing is the thing to look at."
        }
        (_, true, false, _) if all_held => {
            "THE VM IS STILL HELD, so nothing downstream of the resume is implicated at all. Every thread \
             is suspended and `debug.continue`'s own reply is below — a launch suspension that was never \
             cleared holds all of them, which is what distinguishes this from the one-thread hold a \
             CLASS_PREPARE takes. Read `debug.continue`'s reply first; if it claimed success, this is a \
             resume-honesty failure and not a class-loading one."
        }
        (_, true, false, _) if !none_held => {
            "THE CLASS PREPARED AND WE NEVER HEARD ABOUT IT. The stop point is still `waiting for class \
             load` and `<clinit>` never printed — but SOME BUT NOT ALL threads are suspended, and after a \
             debug.continue that took, the only thing here that holds one thread is the JVM parking the \
             preparing thread for a CLASS_PREPARE it has already generated: `EventThread` policy, and \
             `try_arm_deferred_breakpoints` resumes it only after the arm. So the class prepared, the event \
             did not reach us, and the thread is waiting on a resume that never came. That is the EVENT \
             PUMP — not the resume, and not the arming. `THE CLASS NEVER PREPARED` used to claim this case."
        }
        (_, true, false, _) => {
            "THE CLASS NEVER PREPARED. The stop point is still `waiting for class load`, the probe never \
             printed from `<clinit>`, and NOTHING is suspended — so the JVM is not parked on a \
             CLASS_PREPARE either and it simply did not get as far as loading StartupProbe. The resume is \
             the suspect, not the arming."
        }
        (_, true, true, _) => {
            "THE CLASS PREPARED AND THE DEFERRED ARM DID NOT LAND. The probe's own stdout shows \
             `<clinit>` ran, yet the stop point is STILL `waiting for class load` — so the CLASS_PREPARE \
             handler never armed it. This is the reading that makes it a race rather than a slow box. \
             Note that the arm is not obviously racing `<clinit>`: the CLASS_PREPARE request is set with \
             SuspendPolicy::EventThread, so the preparing thread is held while the arm goes out."
        }
        (_, false, _, 0) => {
            "ARMED, NEVER HIT. The class prepared and the breakpoint armed, with zero hits. If the \
             output below shows `<clinit>` ran, the arm landed AFTER the line had already executed — \
             the same race, one step later."
        }
        (_, false, _, _) => {
            "ARMED AND HIT, BUT NO EVENT ARRIVED. Hits are non-zero, so the arming chain is not the \
             problem at all: the event did not reach the buffer within EVENT_TIMEOUT."
        }
    }
}

/// The `N` and `M` of a `debug.list_threads {only_suspended: true}` reply's leading `N/M`.
fn held_counts(held: &str) -> (Option<u32>, Option<u32>) {
    let mut nums = held.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty());
    (nums.next().and_then(|s| s.parse::<u32>().ok()), nums.next().and_then(|s| s.parse::<u32>().ok()))
}

/// Every reading of a missed `<clinit>` breakpoint is reachable, and no two inputs give the same one
/// (TEST-34, #118).
///
/// **Not `#[ignore]`d, and it needs no JDK**, which is the point. ADR-0034 asks that an assertion be seen
/// firing before it is trusted, and this decision table cannot satisfy that against a live JVM: a relay
/// cannot sit in front of a JVM `debug.launch` started, so its `CLASS_PREPARE` cannot be delayed and it
/// cannot be stopped from loading its own main class. Two branches are therefore unreachable on demand — and
/// #118 being undiagnosed for four sessions IS that unreachability. So the table is driven off captured
/// reply text, which is the strongest available form of "seen firing" here.
///
/// The `held` strings are real `debug.list_threads {only_suspended: true}` replies, shape included: a
/// launch-time hold reports every thread, and a `CLASS_PREPARE` hold under `EventThread` reports one.
#[test]
fn the_clinit_readings_are_distinguishable() {
    const NONE: &str = "0/8 thread(s) suspended-only:";
    const ONE: &str = "1/8 thread(s) suspended-only:\n0x1 main [running] ⏸️ SUSPENDED BY YOU (0s, …)";
    const ALL: &str = "8/8 thread(s) suspended-only:\n0x1 main [running]";

    // (listed, deferred, <clinit> ran, hits, suspended reply) -> the phrase that must lead the reading
    let cases = [
        (false, false, false, 0, NONE, "NOT IN THE LISTING"),
        (true, true, false, 0, ALL, "THE VM IS STILL HELD"),
        (true, true, false, 0, ONE, "PREPARED AND WE NEVER HEARD ABOUT IT"),
        (true, true, false, 0, NONE, "THE CLASS NEVER PREPARED"),
        (true, true, true, 0, NONE, "THE DEFERRED ARM DID NOT LAND"),
        (true, false, true, 0, NONE, "ARMED, NEVER HIT"),
        (true, false, true, 3, NONE, "ARMED AND HIT, BUT NO EVENT ARRIVED"),
    ];
    let mut seen: Vec<&str> = Vec::new();
    for (listed, deferred, clinit_ran, hits, held, expected) in cases {
        let got = clinit_reading(listed, deferred, clinit_ran, hits, held);
        assert!(
            got.contains(expected),
            "({listed}, {deferred}, {clinit_ran}, {hits}) with {held:?} must read {expected:?}, got: {got}"
        );
        // Distinct, not merely non-empty. Two inputs landing on one reading is the defect this whole
        // table exists to remove, and it would otherwise pass the assertion above.
        assert!(!seen.contains(&got), "two different states produced the same reading: {got}");
        seen.push(got);
    }

    // The three `held` shapes are what the split turns on, so the parse gets its own check: a boolean
    // "something is suspended" cannot tell the launch hold from the class-prepare hold, and the first draft
    // of this split could not either.
    assert_eq!(held_counts(NONE), (Some(0), Some(8)));
    assert_eq!(held_counts(ONE), (Some(1), Some(8)));
    assert_eq!(held_counts(ALL), (Some(8), Some(8)));
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
    let mut probe = Probe::launch(&jdk, "ForceProbe").expect("launch ForceProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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

/// EVAL-10 (#92) — the reply's marker for a collection read by walking its own fields.
const WALKED: &str = "📐 read structurally";
/// EVAL-10 (#92) — the reply's marker for a collection read by invoking in the debuggee.
const INVOKED: &str = "⚙️ read by invoking";

/// The ANSWER half of a `debug.evaluate` reply — what the two read paths must agree on.
///
/// Deliberately not the whole reply. The EVAL-10 path note is exactly what has to differ, and the
/// header in front of a slice or filter names the runtime type, which differs by construction: one
/// side is a `java.util.HashMap`, the other the wrapper holding it.
fn answer(reply: &str) -> String {
    let body = reply.split_once(" = ").map_or(reply, |(_, v)| v);
    // A multi-value reply is `<type>[…] → N of M unit {`, one line per element, then `}`.
    if let Some((head, rest)) = body.split_once('{') {
        let selection = head.split_once('→').map_or("", |(_, s)| s).trim();
        let elements: Vec<&str> =
            rest.lines().map(str::trim).take_while(|l| *l != "}").filter(|l| !l.is_empty()).collect();
        return format!("{selection}\n{}", elements.join("\n"));
    }
    body.lines().next().unwrap_or_default().trim().to_string()
}

/// Assert the structural and the invoking path returned the same answer over the same objects, and
/// that each reply said which path it took.
///
/// The comparison is against the OTHER PATH, never against a hardcoded expectation — that is what
/// keeps the two from drifting apart as either changes.
fn assert_paths_agree(label: &str, walked: &str, invoked: &str) {
    assert!(walked.contains(WALKED), "{label}: expected a structural read, got:\n{walked}");
    assert!(invoked.contains(INVOKED), "{label}: expected an invoking read, got:\n{invoked}");
    assert_eq!(
        answer(walked),
        answer(invoked),
        "{label}: the two paths disagree\n--- structural ---\n{walked}\n--- invoking ---\n{invoked}"
    );
}

/// The sixteen equal-hash keys `CollectionProbe` builds, rebuilt here rather than copied, so the test
/// and the probe cannot drift apart on which keys collide.
fn colliding_keys() -> Vec<String> {
    let two = ["Aa", "BB"];
    let mut out = Vec::new();
    for a in two {
        for b in two {
            for c in two {
                for d in two {
                    out.push(format!("{a}{b}{c}{d}"));
                }
            }
        }
    }
    out
}

/// EVAL-10 (#92): a subscript, a slice and a filter over the standard collections **with nothing
/// suspended** — which is the whole point, since invoking `get()` needs a suspended thread and the
/// shared 8180 is exactly the instance you cannot afford to suspend.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
// One probe, one premise ("nothing is suspended"), and every assertion below only means anything
// while that premise holds — so this cannot be split without each half re-establishing it.
#[allow(clippy::too_many_lines)]
fn collection_reads_walk_the_layout_with_no_suspended_thread() {
    let Some(jdk) = jdk_or_skip("collection_reads_walk_the_layout_with_no_suspended_thread") else {
        return;
    };
    // The first question here is about static fields, and a static field of a class that has not
    // initialised yet answers "not loaded" *correctly* — so wait for the probe to be RUNNING rather
    // than merely listening (TEST-17).
    let mut probe = Probe::launch_running(&jdk, "CollectionProbe", |l| l.starts_with("inspect "))
        .expect("launch CollectionProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // The premise, asserted rather than assumed: no stop point was armed and nothing is suspended.
    let before = server.call("debug.list_threads", serde_json::json!({"only_suspended": true, "limit": 200}));
    assert!(before.trim_start().starts_with("0/"), "something was already suspended:\n{before}");

    // --- a subscript on each recognised layout, with no thread to invoke on ---
    let hash = server.evaluate("CollectionProbe.HASH[\"b\"].sku");
    assert_contains_all("HashMap key with no suspended thread", &hash, &["\"bb\"", WALKED]);
    // The note is not decoration: it states the two things the value itself cannot say.
    assert_contains_all(
        "the walk states what it did and did not do",
        &hash,
        &["no thread had to be suspended", "SAMPLE of a live collection"],
    );
    assert_contains_all(
        "LinkedHashMap key",
        &server.evaluate("CollectionProbe.LINKED[\"d\"].qty"),
        &["(int) 9", WALKED],
    );
    assert_contains_all(
        "ConcurrentHashMap key",
        &server.evaluate("CollectionProbe.CONCURRENT[\"e\"].sku"),
        &["\"ee\"", WALKED],
    );
    assert_contains_all(
        "ArrayList index",
        &server.evaluate("CollectionProbe.LIST[2].sku"),
        &["\"cc\"", WALKED],
    );
    // An int key is boxed to Integer before `get(Object)` sees it, so the walk compares against an
    // Integer key too — `Integer.equals(Long)` is false and both paths have to say so.
    assert_contains_all(
        "Integer-keyed map",
        &server.evaluate("CollectionProbe.BY_ID[3].sku"),
        &["\"dd\"", WALKED],
    );
    // A key that is not there is `null`, which is an answer rather than an error — the same one
    // `get()` gives.
    assert_contains_all("absent key", &server.evaluate("CollectionProbe.HASH[\"zz\"]"), &["null", WALKED]);

    // --- the treeified bin ---
    // Proven treeified rather than assumed: the table is read the same way the walk reads it, and a
    // bin that treeified holds `HashMap$TreeNode` where an ordinary one holds `HashMap$Node`.
    let table = server.call(
        "debug.evaluate",
        serde_json::json!({"expression": "CollectionProbe.TREEIFIED.table[0..64]", "max_children": 64}),
    );
    assert!(
        table.contains("TreeNode"),
        "the colliding keys did not treeify a bin, so the treeified case went untested:\n{table}"
    );
    assert_contains_all(
        "first colliding key",
        &server.evaluate("CollectionProbe.TREEIFIED[\"AaAaAaAa\"].sku"),
        &["\"t0\"", WALKED],
    );
    assert_contains_all(
        "last colliding key",
        &server.evaluate("CollectionProbe.TREEIFIED[\"BBBBBBBB\"].sku"),
        &["\"t15\"", WALKED],
    );

    // --- a slice: the backing array is 64 long, the list is 5, and the spare slots are null ---
    let sliced = server.evaluate("CollectionProbe.LIST[0..64]");
    assert_contains_all("an over-long slice clamps to size, not capacity", &sliced, &["5 of 5", WALKED]);
    assert!(!sliced.contains("null"), "elementData's trailing nulls leaked into the list:\n{sliced}");

    // --- a filter whose predicate reads a FIELD of each element, so it invokes nothing either ---
    assert_contains_all(
        "filter a HashMap by its values, keeping the keys",
        &server.evaluate("CollectionProbe.HASH[?qty > 3]"),
        &["3 of 5 entr(ies)", "\"b\" →", "\"d\" →", "\"e\" →", WALKED],
    );

    // --- an unrecognised implementation refuses instead of guessing at its internals ---
    assert_contains_all(
        "a synchronizedMap wrapper says what it needs and why",
        &server.evaluate("CollectionProbe.HASH_WRAPPED[\"b\"]"),
        &["needs a suspended thread", "java.util.Collections$SynchronizedMap", "structural reads cover"],
    );
    // A HashMap SUBCLASS has a HashMap's internals and is still not walked: recognition is by exact
    // signature, because the next subclass along may keep its entries somewhere else entirely.
    assert_contains_all(
        "a HashMap subclass is not walked",
        &server.evaluate("CollectionProbe.SUBCLASS[\"b\"]"),
        &["needs a suspended thread"],
    );

    // Nothing above suspended anything, which is the claim being made — so it is asserted, not
    // inferred from the reads having succeeded.
    let after = server.call("debug.list_threads", serde_json::json!({"only_suspended": true, "limit": 200}));
    assert!(after.trim_start().starts_with("0/"), "a collection read suspended a thread:\n{after}");
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| l.contains("inspect ")).is_some(),
        "the probe stopped running while its collections were read"
    );
}

/// EVAL-10 (#92): the walked answer and the invoked answer, over the same objects.
///
/// Every pair below reads one collection twice — once through the field walk, once through a wrapper
/// of the same object that is not a recognised layout and therefore goes through `get()` /
/// `entrySet()` / `toArray()`. Asserting them equal to each other rather than to a written-down
/// expectation is what keeps the two implementations from drifting.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
#[allow(clippy::too_many_lines)]
fn structural_and_invoking_collection_reads_agree() {
    let Some(jdk) = jdk_or_skip("structural_and_invoking_collection_reads_agree") else { return };
    let mut probe = Probe::launch(&jdk, "CollectionProbe").expect("launch CollectionProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // A suspended thread, so that the INVOKING half of every comparison is reachable at all.
    let line = probe_line(&probe_source("CollectionProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "CollectionProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in CollectionProbe.inspect never fired");

    for (label, walked, invoked) in [
        ("HashMap key", "CollectionProbe.HASH[\"b\"]", "CollectionProbe.HASH_WRAPPED[\"b\"]"),
        ("HashMap absent key", "CollectionProbe.HASH[\"zz\"]", "CollectionProbe.HASH_WRAPPED[\"zz\"]"),
        ("LinkedHashMap key", "CollectionProbe.LINKED[\"c\"]", "CollectionProbe.LINKED_WRAPPED[\"c\"]"),
        (
            "ConcurrentHashMap key",
            "CollectionProbe.CONCURRENT[\"e\"]",
            "CollectionProbe.CONCURRENT_WRAPPED[\"e\"]",
        ),
        ("Integer key", "CollectionProbe.BY_ID[3]", "CollectionProbe.BY_ID_WRAPPED[3]"),
        ("ArrayList index", "CollectionProbe.LIST[4]", "CollectionProbe.LIST_WRAPPED[4]"),
        // Same five entries, one map walked and one map's subclass invoked.
        ("HashMap subclass", "CollectionProbe.HASH[\"a\"]", "CollectionProbe.SUBCLASS[\"a\"]"),
        ("ArrayList slice", "CollectionProbe.LIST[1..4]", "CollectionProbe.LIST_WRAPPED[1..4]"),
        (
            "ArrayList slice past its size",
            "CollectionProbe.LIST[0..64]",
            "CollectionProbe.LIST_WRAPPED[0..64]",
        ),
        ("HashMap filter", "CollectionProbe.HASH[?qty > 3]", "CollectionProbe.HASH_WRAPPED[?qty > 3]"),
        // ORDER, not just membership: LINKED was built backwards, so its iteration order and its table
        // order differ, and a walk of the wrong half would return the right entries in the wrong order.
        (
            "LinkedHashMap filter keeps insertion order",
            "CollectionProbe.LINKED[?qty > 0]",
            "CollectionProbe.LINKED_WRAPPED[?qty > 0]",
        ),
        (
            "ConcurrentHashMap filter",
            "CollectionProbe.CONCURRENT[?qty > 3]",
            "CollectionProbe.CONCURRENT_WRAPPED[?qty > 3]",
        ),
        (
            "Integer-keyed filter",
            "CollectionProbe.BY_ID[?qty > 3]",
            "CollectionProbe.BY_ID_WRAPPED[?qty > 3]",
        ),
        (
            "treeified filter",
            "CollectionProbe.TREEIFIED[?qty > 12]",
            "CollectionProbe.TREEIFIED_WRAPPED[?qty > 12]",
        ),
    ] {
        let a = server.evaluate(walked);
        let b = server.evaluate(invoked);
        assert_paths_agree(label, &a, &b);
    }

    // Every one of the sixteen colliding keys, because a treeified bin is exactly where a walk that
    // followed the red-black tree instead of the `next` chain would quietly lose entries.
    for key in colliding_keys() {
        let a = server.evaluate(&format!("CollectionProbe.TREEIFIED[\"{key}\"]"));
        let b = server.evaluate(&format!("CollectionProbe.TREEIFIED_WRAPPED[\"{key}\"]"));
        assert_paths_agree(&format!("treeified key {key}"), &a, &b);
    }

    server.panic_reset();
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| l.contains("inspect ")).is_some(),
        "probe stopped running after the comparison"
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
    let mut probe = Probe::launch(&jdk, "MetricsProbe").expect("launch MetricsProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "ExcProbe").expect("launch ExcProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "ExcMsgProbe").expect("launch ExcMsgProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "RethrowProbe").expect("launch RethrowProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "RethrowProbe").expect("launch RethrowProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
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
    // `probe.attach(&mut server)` rather than `probe.attach(&mut server)`: this is one of the two tests
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
    let mut probe = Probe::launch(&jdk, "CallerProbe").expect("launch CallerProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "CallerProbe").expect("launch CallerProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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

/// How many characters a rendered value kept before the debugger's `… (N chars total)` suffix.
///
/// Panics if the value was not truncated at all — which is the point: "did it truncate, and at exactly
/// what length" is one question here, and a helper that answered `0` for "not truncated" would let the
/// default arm pass on a capture that was never capped.
fn kept_chars(line: &str, after: &str) -> usize {
    let at = line.find(after).unwrap_or_else(|| panic!("no `{after}` in trace line: {line}"));
    let rest = &line[at + after.len()..];
    let end = rest
        .find("… (")
        .unwrap_or_else(|| panic!("`{after}` was not truncated at all in trace line: {line}"));
    rest[..end].trim_start_matches('"').chars().count()
}

/// TRACE-9 (#80): the per-value capture cap is the caller's to raise, and raising it is the only way to
/// see a payload — truncation happens at CAPTURE time, so `debug.get_traces` can never recover the rest.
///
/// Both halves are asserted, and the default half is the one that carries the test. Asserting only that
/// a raised cap reaches the end of the payload would pass identically if `trace_max_length` were parsed
/// and thrown away, because a 2048-character body fits under any cap that was never applied. So the
/// default arm pins the two documented numbers exactly — 100 for an in-scope local, 200 for the
/// `trace_expr` result — and the raised arm proves the marker that lives ONLY in the payload's last
/// field arrives.
///
/// The payload's length and its tail marker are read from the probe's OWN stdout, not from the
/// debugger's reply: the debugger would report a capture happily either way, and a test that took its
/// word for what the JVM was holding would be checking the debugger against itself.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_raised_trace_max_length_captures_a_payload_the_default_cuts() {
    let Some(jdk) = jdk_or_skip("a_raised_trace_max_length_captures_a_payload_the_default_cuts") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "PayloadProbe").expect("launch PayloadProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // The probe's own account of what it is holding. Everything below is asserted against these.
    let announced = probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("payload length="))
        .expect("probe never announced its payload");
    assert!(
        announced.contains("payload length=2048") && announced.contains("tail=TAILMARK"),
        "the probe must hold a 2048-char payload ending in the tail marker: {announced}"
    );
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let base = highest_tick(&probe).expect("no tick to count from");

    let src = probe_source("PayloadProbe");
    let line = probe_line(&src, "// BP1");

    // ---- the default: byte-for-byte what this tool captured before trace_max_length existed ----
    let armed = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "PayloadProbe", "line": line, "trace": true,
            "trace_expr": "response.getBody()",
        }),
    );
    assert!(armed.contains("bp_"), "the traced logpoint must arm: {armed}");
    assert!(
        !armed.contains("trace_max_length"),
        "an unset trace_max_length must add nothing to the reply: {armed}"
    );
    let default_bp = stop_id(&armed, "bp_").expect("no bp_ id in the arm reply");

    let traces = server
        .wait_for_traces("PayloadProbe.handle:", EVENT_TIMEOUT)
        .expect("the traced logpoint never recorded a hit");
    let hit = traces
        .lines()
        .find(|l| l.contains("PayloadProbe.handle:"))
        .unwrap_or_else(|| panic!("no hit line in:\n{traces}"));

    assert_eq!(kept_chars(hit, "body="), 100, "an in-scope local is captured at 100 chars: {hit}");
    assert_eq!(
        kept_chars(hit, "response.getBody() => "),
        200,
        "the trace_expr result is captured at 200 — twice the locals', because it is the value the \
         caller named: {hit}"
    );
    assert!(hit.contains("(2048 chars total)"), "a truncated capture must say how much it cut: {hit}");
    assert!(
        !hit.contains("TAILMARK"),
        "the end of the payload cannot be reachable at the default caps: {hit}"
    );

    // TRACE-2's discipline, which no reply can evidence: reading a 2048-char local must still resume the
    // hit thread. Only the probe's own ticks prove it.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2)).is_some(),
        "probe stopped ticking under a traced logpoint — a hit left it suspended\n  output: {:?}",
        probe.output(),
    );

    // ---- raised: the same line, the same expression, and now the whole payload ----
    server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": default_bp}));
    let raised = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "PayloadProbe", "line": line, "trace": true,
            "trace_expr": "response.getBody()", "trace_max_length": 2500,
        }),
    );
    assert!(raised.contains("bp_"), "the raised logpoint must arm: {raised}");
    assert!(!raised.contains("clamped"), "2500 is under the ceiling and must not be clamped: {raised}");

    // `TAILMARK` exists nowhere but the payload's last field, so a record containing it can only have
    // come from the raised stop point — no filtering by id needed to tell the two arms apart.
    let full = server
        .wait_for_traces("TAILMARK", EVENT_TIMEOUT)
        .expect("a raised trace_max_length never reached the end of the payload");
    let full_hit = full
        .lines()
        .find(|l| l.contains("TAILMARK"))
        .unwrap_or_else(|| panic!("no hit line carrying the tail marker in:\n{full}"));
    assert!(
        !full_hit.contains("chars total"),
        "at 2500 a 2048-char payload is not truncated at all: {full_hit}"
    );
    assert_eq!(
        full_hit.matches("TAILMARK").count(),
        2,
        "BOTH the local and the trace_expr result must reach the end — one knob raises both: {full_hit}"
    );

    // ---- above the ceiling: clamped, and said out loud ----
    let over = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "PayloadProbe", "line": line, "trace": true, "trace_max_length": 99999,
        }),
    );
    assert_contains_all(
        "a request above the ceiling is clamped and the clamp is reported",
        &over,
        &["clamped to 4000", "trace_max_length 99999"],
    );

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
    let mut probe = Probe::launch(&jdk, "CallerProbe").expect("launch CallerProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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

/// The `thread=0x…` id off the first trace snapshot whose line contains `at`.
///
/// A recorded hit is a stronger source for a thread id than `debug.list_threads` is, and the difference
/// is the whole of TEST-42 (#127): a listing proves only that the thread EXISTED when the listing was
/// taken, while a snapshot proves the thread ran the code the test is about. On a pool that sheds load and
/// retires idle workers those are very different claims.
fn traced_thread(traces: &str, at: &str) -> Option<String> {
    let line = traces.lines().find(|l| l.contains(at))?;
    let start = line.find("thread=")? + "thread=".len();
    let rest = line.get(start..)?;
    let end = rest.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(rest.len());
    let id = rest.get(..end)?;
    (id.starts_with("0x") && id.len() > 2).then(|| id.to_string())
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
    let mut probe = Probe::launch(&jdk, "DeadlockProbe").expect("launch DeadlockProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "DeadlockProbe").expect("launch DeadlockProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "SlowToStringProbe").expect("launch SlowToStringProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    probe.attach(&mut server);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");

    // The target comes from a HIT, not from a thread listing (TEST-42, #127). This used to take the first
    // `pool-worker` out of `debug.list_threads` and then wait `EVENT_TIMEOUT` for *that* worker to throw —
    // which the pool does not guarantee: `PoolProbe` sheds load when its queue is full and lets an idle
    // worker die on the keep-alive, both deliberate and both in its own header. So the chosen worker might
    // take no further work, the wait timed out about 1 run in 24, and its message ("never fired while its
    // thread was alive") blamed the filter for the test's own choice of thread. Arming unfiltered inverts
    // it: whichever worker throws first is by construction one that runs `doWork`.
    let scout = server.call(
        "debug.set_exception_stop",
        serde_json::json!({
            "class_pattern": "PoolProbe$PoolException",
            // One hit is all that is needed, and `doWork` throws on every task of a 100-task batch — an
            // unbounded unfiltered trace would capture hundreds of hits a second to no purpose.
            "trace": true, "trace_max_hits": 1,
        }),
    );
    let Some(scout_id) = stop_id(&scout, "exc_") else { panic!("the unfiltered arm failed: {scout}") };
    let observed = server.wait_for_traces("PoolProbe.doWork", EVENT_TIMEOUT).unwrap_or_else(|| {
        panic!(
            "no pool worker threw at all, so the probe is not producing exceptions — a SETUP failure, \
             which says nothing about the pinned filter\n  output: {:?}",
            probe.output()
        )
    });
    let target = traced_thread(&observed, "PoolProbe.doWork")
        .unwrap_or_else(|| panic!("a recorded hit carried no thread= id:\n{observed}"));

    // Clear the scout and its snapshots, so everything the buffer says from here on is the pinned filter's
    // doing and the emptiness asserted below is the filter's silence rather than the scout's.
    server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": scout_id}));
    server.call("debug.get_traces", serde_json::json!({"clear": true}));

    let armed = server.call(
        "debug.set_exception_stop",
        serde_json::json!({
            "class_pattern": "PoolProbe$PoolException",
            "trace": true, "trace_max_hits": 0, "thread_id": target,
        }),
    );
    // This assertion is what the removed `wait_for_traces` was really establishing. The old wait proved the
    // pinned thread was alive by watching it throw again; arming against a *dead* id is refused naming the
    // stale thread (asserted at the end of this test), so a successful arm proves the same liveness — and
    // proves it without depending on the pool scheduling that worker a second time.
    assert!(
        armed.contains("exc_"),
        "the pinned arm was refused, so the worker retired between its hit and this arm — a SETUP failure, \
         not the filter's: {armed}"
    );

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
    let mut probe = Probe::launch(&jdk, "PoolProbe").expect("launch PoolProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // TEST-39: wait for the pool the premise below needs, rather than for A heartbeat.
    //
    // This used to be `wait_for_line(|l| tick_index(l).is_some())`, on the stated reasoning that "the pool
    // pre-starts every core thread, so waiting for one heartbeat is enough for all 200 to exist". It is
    // not. `prestartAllCoreThreads()` runs before the loop, but starting 200 threads takes time the first
    // heartbeat does not wait for, and under contention it is a lot of time: caught in a 4-vCPU soak with
    // the whole suite at 16 threads, where the listing came back `85/91 thread(s) name~"pool-worker"` and
    // the saturation assertion failed with `saw 86 worker(s)`.
    //
    // The probe has been reporting the answer all along — `tick <n> handled=<c> pool=<size>` is
    // `getPoolSize()` — so this asks it rather than inferring it from the clock, which is the same
    // correction `awaitBlocked` is in `MonitorProbe` and `hold` is in `WedgeProbe`. It waits for the very
    // number the assertion then requires, so a failure past this point is about the FILTER rather than
    // about a pool that had not finished starting.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| tick_pool_size(l).is_some_and(|n| n >= MIN_SATURATED_POOL))
        .unwrap_or_else(|| {
            panic!(
                "the pool never reached {MIN_SATURATED_POOL} threads within {EVENT_TIMEOUT:?}, so the \
                 premise of this test was never true — this is a slow start, not a filter fault. The \
                 probe's own last heartbeat: {:?}",
                probe.output().iter().rev().find(|l| tick_pool_size(l).is_some())
            )
        });
    let base = highest_tick(&probe).expect("no tick to count from");

    let threads =
        server.call("debug.list_threads", serde_json::json!({"name_filter": "pool-worker", "limit": 400}));
    // The premise of the test: if the pool were not saturated this would be a handful of threads, and
    // "the filter excluded the others" would prove almost nothing.
    let pool_size = threads.lines().filter(|l| l.contains("pool-worker")).count();
    assert!(
        pool_size >= MIN_SATURATED_POOL,
        "the pool must be saturated for this test to mean anything, saw {pool_size} worker(s). The wait \
         above already required {MIN_SATURATED_POOL} by the probe's own `pool=` count, so this is the \
         LISTING disagreeing with the debuggee rather than a pool that had not started (TEST-39). The \
         probe's last heartbeat: {:?}\n{threads}",
        probe.output().iter().rev().find(|l| tick_pool_size(l).is_some())
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
    let mut probe = Probe::launch(&jdk, "ManyThreadsProbe").expect("launch ManyThreadsProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "PoolShapeProbe").expect("launch PoolShapeProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
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
    let mut probe = Probe::launch(&jdk, "MixedPoolProbe").expect("launch MixedPoolProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
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
    let mut probe = Probe::launch(&jdk, "PoolShapeProbe").expect("launch PoolShapeProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
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
    let mut probe = Probe::launch(&jdk, "DeadlockProbe").expect("launch DeadlockProbe");
    let mut server = Server::start_with_env(&[("JDWP_READONLY", "1")]).expect("start server");
    probe.attach(&mut server);
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
    let mut probe = Probe::launch(&jdk, "ReturnProbe").expect("launch ReturnProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "ReturnProbe").expect("launch ReturnProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
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
    let mut probe = Probe::launch(&jdk, "ExcProbe").expect("launch ExcProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    // And a census by kind, because the buffer alone still leaves the reader counting. Which KIND doubled
    // is the issue's first acceptance criterion, and the two candidates need opposite responses: a second
    // `breakpoint` means the stop point at BP2 — never cleared before the step, on a line inside a
    // 100000-iteration loop — fired twice, so the test's staging is what is wrong; a second `step` means
    // one `debug.step_over` produced two events, which would be the buffer's or the stepper's doing.
    let census = |kind: &str| buffered.matches(&format!("\"event\":\"{kind}\"")).count();
    assert_contains_all(
        &format!(
            "newest event, and the backlog is announced\nthe buffer holds {} breakpoint and {} step \
             event(s), of {} in all — this test stages exactly one of each\nthe whole buffer was:\n{buffered}",
            census("breakpoint"),
            census("step"),
            buffered.matches("\"event\":\"").count(),
        ),
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
    let mut probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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

    // --- SETF-3 (#119): a bracket inside the KEY is content, and both tools have to agree it is ---
    //
    // These are the targets the two scanners disagreed about. `debug.evaluate` read them all, because
    // expression resolution goes through the forward scanners EVAL-8 (#82) made quote-aware;
    // `debug.set_value` walked backwards counting brackets with no idea a key could contain one, so the
    // `]` inside the key inflated the depth and the real opening bracket never brought it back to zero.
    // The refusal — `Could not find the final subscript` — named the caller's syntax for a limitation in
    // our own parser, which is the failure mode this repo treats as worse than an error.
    //
    // Asserted as a PAIR each time: what `evaluate` resolves, `set_value` writes. That is the property
    // that drifted, so it is the property under test rather than either tool alone.
    for (target, before, after) in [
        ("bracketKeyed[\"]\"]", "(int) 1", "11"),
        ("bracketKeyed[\"[\"]", "(int) 2", "12"),
        ("bracketKeyed[\"a[b]c\"]", "(int) 3", "13"),
        // EVAL-8 made `char` literals legal map keys, so this target became writable-in-principle in the
        // same change that left this scanner behind.
        ("charKeyed[']']", "(int) 4", "14"),
        ("charKeyed['[']", "(int) 5", "15"),
    ] {
        assert_contains_all(&format!("evaluate resolves {target}"), &server.evaluate(target), &[before]);
        let set = write(&mut server, target, after);
        assert!(
            !set.contains("Could not find the final subscript"),
            "set_value must accept the target evaluate just read, {target}:\n{set}"
        );
        assert_contains_all(&format!("{target} written via put()"), &set, &["put()"]);
        assert_contains_all(
            &format!("and {target} stuck"),
            &server.evaluate(target),
            &[&format!("(int) {after}")],
        );
    }

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

/// How many `PoolProbe` workers have to exist before "the filter excluded the others" proves anything.
///
/// The same number the saturation assertion uses, deliberately: the wait and the assertion must not be able
/// to disagree, which is how a race becomes an assertion failure about something else (TEST-39).
const MIN_SATURATED_POOL: usize = 100;

/// The `pool=<size>` of `PoolProbe`'s `tick <n> handled=<c> pool=<size>` heartbeat — `getPoolSize()` as the
/// debuggee itself reports it, which is the only thing that knows whether `prestartAllCoreThreads()` has
/// finished.
fn tick_pool_size(line: &str) -> Option<usize> {
    line.split("pool=").nth(1)?.trim().parse().ok()
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
    let mut probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "1")]).expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "1")]).expect("start server");
    probe.attach(&mut server);
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
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "1")]).expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    // Watchdog disabled, so ONLY the disconnect can rescue the VM — otherwise the watchdog could be
    // what resumes it and the test would pass without disconnect doing its job.
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    let attach = probe.attach(&mut server);
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
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "ThreadProbe").expect("launch ThreadProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "1")]).expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    // 5s, not 1s: the drain has to land BEFORE the watchdog fires, or the watchdog reads the event it
    // was always going to read and the test proves nothing. (Measured: with 1s it passed even against
    // the old `events.back()` derivation, because the resume raced ahead of the drain.)
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "5")]).expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start_with_env(&[("JDWP_READONLY", "1")]).expect("start server");
    probe.attach(&mut server);

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
        obj.contains("@0x"),
        "a read-only object render must fall back to Type @0x… rather than invoking toString(): {obj}"
    );

    // 2. A List subscript used to invoke `List.get(int)` — also parenthesis-free, also previously
    //    missed by the text guard. Since EVAL-10 (#92) it invokes nothing: an `ArrayList` is read by
    //    walking `elementData`, so read-only ALLOWS it and the reply says which path it took. ADR-0001
    //    is unchanged — reads needing no invocation were always untouched; what moved is which reads
    //    those are.
    let sub = server.evaluate("order.lines[0]");
    assert_contains_all("a List subscript is a field walk now, and says so", &sub, &["Line", WALKED]);
    // A `Map` that is NOT a recognised layout still reaches its entry through `get()`, so the wire
    // guard still refuses it — the property this case was added for is intact.
    assert_contains_all(
        "a subscript that must still invoke is still refused",
        &server.evaluate("order.wrappedCounts[\"a\"]"),
        &["Read-only"],
    );

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
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "DeepProbe").expect("launch DeepProbe");
    let mut server = Server::start_with_env(&[("JDWP_READONLY", "1")]).expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    // Watchdog off: this test is about the tools' own resume arithmetic, not the rescue.
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    probe.attach(&mut server);
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
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "5")]).expect("start server");
    probe.attach(&mut server);
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
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
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
    /// ONE thread frozen by `debug.suspend_thread` (SAFE-11), the whole VM still running. The probe's
    /// ticking thread is the one held, so the same tick witness answers the same question — and this is a
    /// genuinely new way to leave the debuggee stopped: no `suspended_since`, no `SuspendCause`, so
    /// nothing the VM-wide rescue paths look at is set.
    ThreadSuspend,
    /// The same thread frozen TWICE, so it carries a per-thread suspend depth of 2. SAFE-7's shape
    /// arriving through the new door: one decrement leaves it stopped, and the reply must say so.
    ThreadSuspendTwice,
}

/// A way we claim to un-freeze it.
#[derive(Debug, Clone, Copy)]
enum Resume {
    Continue,
    Panic,
    Watchdog,
    /// `debug.disconnect` — the SAFE-1 case, and the one whose name most implies it is safe.
    Disconnect,
    /// `debug.resume_thread` (SAFE-11) — the per-thread door. Run against the VM-wide freezes too, on
    /// purpose: it is a resume path, and a resume path that quietly fails to un-freeze the *thread the
    /// caller named* is the same bug as one that quietly fails to un-freeze the VM.
    ///
    /// Named for what it resumes rather than for the tool, because `ResumeThread` inside an enum called
    /// `Resume` is the stutter `clippy::enum_variant_names` exists to catch.
    OneThread,
}

impl Freeze {
    const ALL: [Self; 8] = [
        Self::Breakpoint,
        Self::Pause,
        Self::BreakpointThenPause,
        Self::BreakpointDrained,
        Self::Step,
        Self::ConditionEscalated,
        Self::ThreadSuspend,
        Self::ThreadSuspendTwice,
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

    // WatchProbe ticks from `main`, so `main` is BOTH the thread the per-thread cases freeze and the
    // witness every case is judged by. Resolved once, before anything is suspended, because
    // `debug.list_threads` on a frozen VM is a different question.
    let ticking = thread_id_named(&mut server, "main");

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
        Freeze::ThreadSuspend | Freeze::ThreadSuspendTwice => {
            let times = if matches!(freeze, Freeze::ThreadSuspendTwice) { 2 } else { 1 };
            for _ in 0..times {
                let said = server.call("debug.suspend_thread", serde_json::json!({"thread_id": &ticking}));
                assert!(
                    said.contains("Suspended thread"),
                    "{freeze:?}: could not suspend the ticking thread, so the state under test was never \
                     reached: {said}"
                );
            }
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
        Resume::OneThread => server.call("debug.resume_thread", serde_json::json!({"thread_id": &ticking})),
        // Nothing to call: the watchdog's whole point is that it acts without being asked.
        Resume::Watchdog => String::new(),
    };

    // --- the invariant ---
    //
    // **A path that has already SAID it left the debuggee suspended does not need the full
    // `EVENT_TIMEOUT` to prove the probe is not ticking**, and waiting it out is what made this matrix
    // the slowest thing in the suite. `WatchProbe` ticks every 150 ms, so `frozen_at + 2` is ~450 ms of
    // ticking away; `STUCK_CONFIRM` is still an order of magnitude more than that, while 25 s is two.
    //
    // Read off `reply` and not off the fuller `said` gathered below, deliberately. `reply` is the
    // resume path's own inline account, and the shortening must not depend on anything that only
    // appears *after* the wait — `Resume::Watchdog` has an empty `reply` by construction (its point is
    // that it acts unasked), so it keeps the full timeout and cannot be shortened by this at all.
    //
    // The cry-wolf half of the invariant survives: ~20 ticks fit in `STUCK_CONFIRM`, so a path that
    // claims "STILL suspended" while the VM is plainly running is still caught by the assertion below.
    let wait = if reply.contains("STILL suspended") { STUCK_CONFIRM } else { EVENT_TIMEOUT };
    let advanced = probe.wait_for_line(wait, |l| tick_index(l).is_some_and(|n| n > frozen_at + 2)).is_some();

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

/// One `#[test]` per (suspended state, resume path) cell of the honesty matrix — 40 of them.
///
/// **They used to be five tests looping over `Freeze::ALL` in-process, and that made them the suite's
/// floor** (TEST-35). Each cell launches its own probe JVM and its own server, so a loop of eight ran
/// eight JVMs *sequentially* inside one libtest thread while the other fifteen sat idle. Measured: the
/// watchdog matrix took **35 s**, and per-cell instrumentation put only ~2 s of each cell in the wait the
/// assertion actually needs — the rest is launch, attach and arm.
///
/// **A single test cannot be split by `scripts/shard-plan.py`**, so those five were a hard floor under
/// every shard count. That floor is why ADR-0025 stopped at two shards, and — after TEST-30 took the
/// other three down — why a third shard still bought only 3.6 s. Splitting them removes it rather than
/// working around it.
///
/// Spelled out per cell rather than generated from a nested macro, because the cell name is what a
/// failure prints and what `timings.tsv` keys on: `the_watchdog_is_honest_from_a_step` says which
/// combination broke without anyone opening the file. The loop version had to panic with
/// `{resume:?} from {freeze:?}` to say the same thing.
/// The matrix is complete, and this is what says so now that the cells are spelled out.
///
/// `Freeze::ALL` used to *be* the guarantee — five loops over it meant a new suspended state was covered
/// by every resume path the moment it was added to the array. Spelling the cells out (TEST-35) trades
/// that for schedulability, and the trade is only safe if the loss is caught: without this, adding a
/// ninth `Freeze` variant would silently get zero tests and every existing one would still pass.
///
/// So it is asserted rather than derived. A count is a weak check — swap two variants and it holds — but
/// the failure mode being guarded is *addition*, which a count catches exactly, and it costs no JVM.
#[test]
fn the_resume_honesty_matrix_covers_every_suspended_state() {
    assert_eq!(
        Freeze::ALL.len(),
        8,
        "a suspended state was added to or removed from Freeze::ALL. The honesty matrix is now one \
         `resume_honesty_case!` line per cell rather than a loop, so it does NOT pick the change up on \
         its own: add (or remove) one line in EACH of the five groups below — continue, panic, watchdog, \
         disconnect, resume_thread — and update this count. Five uncovered cells is what this exists to \
         stop, because every one of them would still pass."
    );
}

macro_rules! resume_honesty_case {
    ($name:ident, $resume:ident, $freeze:ident) => {
        #[test]
        #[ignore = "needs a JDK and a live JVM; run with --ignored"]
        fn $name() {
            let Some(jdk) = jdk_or_skip(stringify!($name)) else { return };
            assert_resume_is_honest(&jdk, Freeze::$freeze, Resume::$resume);
        }
    };
}

// Invariant: `debug.continue` either resumes the VM or says it didn't — from every suspended state.
resume_honesty_case!(continue_is_honest_from_a_breakpoint, Continue, Breakpoint);
resume_honesty_case!(continue_is_honest_from_a_pause, Continue, Pause);
resume_honesty_case!(continue_is_honest_from_a_pause_on_top_of_a_breakpoint, Continue, BreakpointThenPause);
resume_honesty_case!(continue_is_honest_from_a_drained_breakpoint, Continue, BreakpointDrained);
resume_honesty_case!(continue_is_honest_from_a_step, Continue, Step);
resume_honesty_case!(continue_is_honest_from_an_escalated_condition, Continue, ConditionEscalated);
resume_honesty_case!(continue_is_honest_from_a_suspended_thread, Continue, ThreadSuspend);
resume_honesty_case!(continue_is_honest_from_a_twice_suspended_thread, Continue, ThreadSuspendTwice);

// Invariant: `debug.panic` — the escape hatch — either resumes the VM or says it didn't.
resume_honesty_case!(panic_is_honest_from_a_breakpoint, Panic, Breakpoint);
resume_honesty_case!(panic_is_honest_from_a_pause, Panic, Pause);
resume_honesty_case!(panic_is_honest_from_a_pause_on_top_of_a_breakpoint, Panic, BreakpointThenPause);
resume_honesty_case!(panic_is_honest_from_a_drained_breakpoint, Panic, BreakpointDrained);
resume_honesty_case!(panic_is_honest_from_a_step, Panic, Step);
resume_honesty_case!(panic_is_honest_from_an_escalated_condition, Panic, ConditionEscalated);
resume_honesty_case!(panic_is_honest_from_a_suspended_thread, Panic, ThreadSuspend);
resume_honesty_case!(panic_is_honest_from_a_twice_suspended_thread, Panic, ThreadSuspendTwice);

// Invariant: the watchdog either resumes the VM or says it didn't. This is the one that matters most —
// it acts while nobody is watching, so a false success is invisible until the JVM is found frozen.
resume_honesty_case!(the_watchdog_is_honest_from_a_breakpoint, Watchdog, Breakpoint);
resume_honesty_case!(the_watchdog_is_honest_from_a_pause, Watchdog, Pause);
resume_honesty_case!(
    the_watchdog_is_honest_from_a_pause_on_top_of_a_breakpoint,
    Watchdog,
    BreakpointThenPause
);
resume_honesty_case!(the_watchdog_is_honest_from_a_drained_breakpoint, Watchdog, BreakpointDrained);
resume_honesty_case!(the_watchdog_is_honest_from_a_step, Watchdog, Step);
resume_honesty_case!(the_watchdog_is_honest_from_an_escalated_condition, Watchdog, ConditionEscalated);
resume_honesty_case!(the_watchdog_is_honest_from_a_suspended_thread, Watchdog, ThreadSuspend);
resume_honesty_case!(the_watchdog_is_honest_from_a_twice_suspended_thread, Watchdog, ThreadSuspendTwice);

// Invariant: `debug.disconnect` leaves the VM running from every suspended state. This is SAFE-1's bug
// generalised — walking away used to freeze the JVM permanently, and it is the tool whose name most
// suggests it is the safe way out.
resume_honesty_case!(disconnect_is_honest_from_a_breakpoint, Disconnect, Breakpoint);
resume_honesty_case!(disconnect_is_honest_from_a_pause, Disconnect, Pause);
resume_honesty_case!(
    disconnect_is_honest_from_a_pause_on_top_of_a_breakpoint,
    Disconnect,
    BreakpointThenPause
);
resume_honesty_case!(disconnect_is_honest_from_a_drained_breakpoint, Disconnect, BreakpointDrained);
resume_honesty_case!(disconnect_is_honest_from_a_step, Disconnect, Step);
resume_honesty_case!(disconnect_is_honest_from_an_escalated_condition, Disconnect, ConditionEscalated);
resume_honesty_case!(disconnect_is_honest_from_a_suspended_thread, Disconnect, ThreadSuspend);
resume_honesty_case!(disconnect_is_honest_from_a_twice_suspended_thread, Disconnect, ThreadSuspendTwice);

// Invariant: `debug.resume_thread` either gives the named thread back or says it didn't (SAFE-11).
//
// The per-thread door is new, so every state it can be asked from is new too — including the ones it
// cannot fix. `…_from_a_twice_suspended_thread` is the interesting cell: one call decrements one
// suspend, so the thread stays stopped, and the whole question is whether the reply admits it.
// `…_from_a_step` is the other: a step armed on the thread would re-stop it on the very next line, at
// `SuspendPolicy::All`, so releasing one worker would have frozen the entire VM.
resume_honesty_case!(resume_thread_is_honest_from_a_breakpoint, OneThread, Breakpoint);
resume_honesty_case!(resume_thread_is_honest_from_a_pause, OneThread, Pause);
resume_honesty_case!(
    resume_thread_is_honest_from_a_pause_on_top_of_a_breakpoint,
    OneThread,
    BreakpointThenPause
);
resume_honesty_case!(resume_thread_is_honest_from_a_drained_breakpoint, OneThread, BreakpointDrained);
resume_honesty_case!(resume_thread_is_honest_from_a_step, OneThread, Step);
resume_honesty_case!(resume_thread_is_honest_from_an_escalated_condition, OneThread, ConditionEscalated);
resume_honesty_case!(resume_thread_is_honest_from_a_suspended_thread, OneThread, ThreadSuspend);
resume_honesty_case!(resume_thread_is_honest_from_a_twice_suspended_thread, OneThread, ThreadSuspendTwice);

// ---------------------------------------------------------------------------------------------
// SAFE-11 — a per-thread suspend, asserted against the probe's OWN per-thread ticks
//
// `SuspendProbe` prints a SEPARATE counter per worker, which is the only thing that can tell "we froze
// one thread" apart from "we froze the JVM". Every tool here reports success either way.
// ---------------------------------------------------------------------------------------------

/// The `<n>` of a `SuspendProbe` worker's own `<who> tick <n> <layout>` line.
fn worker_tick(line: &str, who: &str) -> Option<i64> {
    line.strip_prefix(who)?.trim_start().strip_prefix("tick ")?.split_whitespace().next()?.parse().ok()
}

/// The highest tick one named worker has printed so far.
fn highest_worker_tick(probe: &Probe, who: &str) -> Option<i64> {
    probe.output().iter().filter_map(|l| worker_tick(l, who)).max()
}

/// The id of the single thread named exactly `name`, read from `debug.list_threads`' own output.
///
/// Exact rather than substring: `name_filter` is a `contains`, and picking the first match would happily
/// return `main` for a filter of `ai` on a JVM with a `Common-Cleaner`. Asserting there is exactly one
/// keeps a probe rename from silently pointing a test at another thread.
fn thread_id_named(server: &mut Server, name: &str) -> String {
    let listed = server.call("debug.list_threads", serde_json::json!({"name_filter": name, "limit": 200}));
    let mut found: Vec<String> = Vec::new();
    for line in listed.lines() {
        let mut parts = line.split_whitespace();
        let (Some(id), Some(got)) = (parts.next(), parts.next()) else { continue };
        if id.starts_with("0x") && got == name {
            found.push(id.to_string());
        }
    }
    assert_eq!(found.len(), 1, "expected exactly one thread named {name}, got {found:?} from:\n{listed}");
    found.remove(0)
}

/// Settle time before reading a probe's tick high-water mark after a suspend.
///
/// JDWP's Suspend reply means the thread is stopped, but a line it printed a moment earlier may still be
/// in the pipe — and reading the mark too early would credit the freeze with a tick it never prevented.
/// How long to keep watching a probe that a resume path has already reported it left frozen.
///
/// This is a bound on observing an **absence**, which is why it is its own constant and much smaller
/// than [`EVENT_TIMEOUT`]: the positive it is ruling out — `WatchProbe` advancing two ticks at 150 ms
/// each — takes ~450 ms, so 3 s is a 6x margin on the thing that would falsify the claim. The 25 s
/// `EVENT_TIMEOUT` is sized for a JVM that has to *do* something before the event can appear, which is
/// the opposite situation.
const STUCK_CONFIRM: std::time::Duration = std::time::Duration::from_secs(3);

const PIPE_SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// SAFE-11's headline claim, and the only assertion that can prove it: suspending ONE thread stops ITS
/// ticks while the others keep printing.
///
/// A tick is the only evidence a thread is running (`CONTEXT.md`), and `debug.suspend_thread` reports
/// success whether it froze one thread, all of them, or none — so the probe's stdout is the witness and
/// the reply is not.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn suspending_one_thread_stops_its_ticks_while_the_others_keep_running() {
    let Some(jdk) = jdk_or_skip("suspending_one_thread_stops_its_ticks_while_the_others_keep_running") else {
        return;
    };
    // The watchdog off, or it would release the thread underneath the assertion and the test would be
    // measuring the rescue rather than the suspend.
    let mut probe = Probe::launch(&jdk, "SuspendProbe").expect("launch SuspendProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    probe.attach(&mut server);
    for who in ["worker-a", "worker-b", "worker-c"] {
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| worker_tick(l, who).is_some())
            .unwrap_or_else(|| panic!("{who} never ticked, so there was nothing to freeze"));
    }

    let tid = thread_id_named(&mut server, "worker-b");
    let said = server.call("debug.suspend_thread", serde_json::json!({"thread_id": &tid}));
    assert_contains_all(
        "suspend_thread reply",
        &said,
        &["Suspended thread", "ONLY this thread", "Suspend depth 1", "debug.resume_thread"],
    );

    std::thread::sleep(PIPE_SETTLE);
    let frozen_b = highest_worker_tick(&probe, "worker-b").unwrap_or(-1);
    let base_a = highest_worker_tick(&probe, "worker-a").unwrap_or(-1);
    let base_c = highest_worker_tick(&probe, "worker-c").unwrap_or(-1);

    // Half one: the OTHERS keep running. Without this the test would pass on a whole-VM freeze.
    for (who, base) in [("worker-a", base_a), ("worker-c", base_c)] {
        assert!(
            probe
                .wait_for_line(EVENT_TIMEOUT, |l| worker_tick(l, who).is_some_and(|n| n > base + 2))
                .is_some(),
            "{who} stopped ticking after worker-b was suspended — a per-thread suspend froze more than \
             one thread.\n  probe output: {:?}",
            probe.output()
        );
    }

    // Half two: the SUSPENDED one did not. Read after the others have advanced three ticks, so a
    // still-running worker-b has had ample opportunity to print.
    let now_b = highest_worker_tick(&probe, "worker-b").unwrap_or(-1);
    assert_eq!(
        now_b,
        frozen_b,
        "worker-b kept ticking ({frozen_b} → {now_b}) while debug.suspend_thread reported it \
         suspended.\n  said: {said}\n  probe output: {:?}",
        probe.output()
    );

    // And the suspension is VISIBLE while it lasts — an invisible one is the kind that gets forgotten.
    let threads =
        server.call("debug.list_threads", serde_json::json!({"name_filter": "worker", "limit": 50}));
    assert_contains_all("list_threads marks the held thread", &threads, &["worker-b", "SUSPENDED BY YOU"]);
    assert!(
        !threads.lines().any(|l| l.contains("worker-a") && l.contains("SUSPENDED BY YOU")),
        "only the held thread may be marked:\n{threads}"
    );
    let sessions = server.call("debug.list_sessions", serde_json::json!({}));
    assert_contains_all(
        "list_sessions shows the held thread",
        &sessions,
        &["1 thread(s) suspended by you", "worker-b"],
    );
    assert!(
        !sessions.contains("SUSPENDED"),
        "the VM is running — only the worker is held, and calling the session SUSPENDED would send a \
         caller looking for a freeze that is not there:\n{sessions}"
    );

    // Resuming brings it back, which is the other half of the claim.
    let back = server.call("debug.resume_thread", serde_json::json!({"thread_id": tid}));
    assert_contains_all("resume_thread reply", &back, &["Resumed thread", "running again"]);
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| worker_tick(l, "worker-b").is_some_and(|n| n > frozen_b))
            .is_some(),
        "worker-b never ticked again after debug.resume_thread said it was running.\n  said: {back}\n  \
         probe output: {:?}",
        probe.output()
    );
}

/// The payoff, and the limit — measured rather than assumed (SAFE-11, issue #90).
///
/// The issue asked for "`evaluate` with a method invocation works against a per-thread-suspended frame"
/// and called it the payoff. **It does not, on `HotSpot`,** and this test is what establishes that rather
/// than a reading of the spec: the same thread id that answers `ThreadReference.Frames` with a full
/// stack of readable locals answers `INVALID_THREAD` to `ClassType.InvokeMethod`. JDWP permits an
/// invocation only on a thread suspended **by an event**, and `debug.pause` does not qualify either —
/// which means the expensive remedy the old refusals named ("pause one or hit a breakpoint first") was
/// half wrong before this issue existed.
///
/// So the payoff is real but narrower than the issue supposed, and this asserts both halves, because a
/// test that only proved the good half would let the tool go on advertising an invocation it cannot do:
///
/// - **reads work** — the whole stack with locals, a local by name, and `expand_objects`, which walks a
///   `LinkedHashMap`'s own fields and reaches the entries without invoking anything;
/// - **a write works**, proved against the probe's own stdout rather than the reply;
/// - **an invoke is refused, and the refusal explains itself** instead of passing `INVALID_THREAD` out.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_per_thread_suspended_frame_reads_and_writes_but_cannot_invoke() {
    let Some(jdk) = jdk_or_skip("a_per_thread_suspended_frame_reads_and_writes_but_cannot_invoke") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "SuspendProbe").expect("launch SuspendProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    probe.attach(&mut server);
    // The map is touched on every pass, so a worker that has ticked has loaded and populated it —
    // evaluating before that would answer a different question (TEST-17).
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| worker_tick(l, "worker-a").is_some())
        .expect("worker-a never ticked");

    let tid = thread_id_named(&mut server, "worker-a");
    let said = server.call("debug.suspend_thread", serde_json::json!({"thread_id": &tid}));
    assert!(said.contains("Suspended thread"), "could not suspend worker-a: {said}");
    // The reply must not promise the invoke it cannot deliver — that is the failure this whole file
    // exists to prevent, and it would be a NEW one rather than an inherited one.
    assert_contains_all(
        "the suspend reply states the invocation limit",
        &said,
        &["NOT UNLOCKED", "INVALID_THREAD", "suspended by an EVENT"],
    );

    // --- reads ---
    let stack = server.call("debug.get_stack", serde_json::json!({"thread_id": &tid, "max_frames": 6}));
    assert_contains_all(
        "a per-thread-suspended thread's stack is readable, locals and all",
        &stack,
        &["SuspendProbe.runWorker", "who = \"worker-a\"", "layout = \"layout-adturismo\""],
    );

    // A local by name. `frame_index` matters and is not incidental: the thread is parked in
    // Thread.sleep, so frame 0 is native and has no variable table at all. The index is READ OFF THE
    // STACK rather than written down, because it is version-locked — JDK 21 splits the sleep into
    // `Thread.sleep` over a native `Thread.sleep0`, so the Java frame is #2 there and #1 on JDK 17, and
    // a hardcoded 2 passed on 21 and failed on 17.
    let frame = stack
        .lines()
        .find(|l| l.contains("SuspendProbe.runWorker"))
        .and_then(|l| l.trim().strip_prefix('#'))
        .and_then(|l| l.split_whitespace().next())
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or_else(|| panic!("no SuspendProbe.runWorker frame in:\n{stack}"));
    let local = server.call(
        "debug.evaluate",
        serde_json::json!({"expression": "who", "thread_id": &tid, "frame_index": frame}),
    );
    assert!(local.contains("worker-a"), "a local was not readable off the suspended frame: {local}");

    // expand_objects reaches the map's CONTENTS by walking fields — no invocation anywhere.
    let expanded = server.call(
        "debug.evaluate",
        serde_json::json!({
            "expression": "SuspendProbe.layoutLoginMap",
            "thread_id": &tid,
            "expand_objects": true,
            "max_depth": 3,
        }),
    );
    assert_contains_all(
        "expand_objects reads fields, so it works where an invoke does not",
        &expanded,
        &["layout-adturismo", "ADTURISMO"],
    );

    // --- a Map subscript, which EVAL-10 (#92) turned from an invoke into a field walk ---
    //
    // This assertion is inverted from the one SAFE-11 originally shipped, and the inversion is the
    // point. That version asserted the subscript could NOT be read here, because `map["k"]` invoked
    // `get()` and JDWP refuses an invoke on a thread suspended this way — and it said in its own words
    // that "if this ever starts working the refusal below is now a lie and both must be revisited".
    // #92 landed in a parallel branch and made exactly that happen: the subscript now walks
    // `LinkedHashMap`'s own fields and invokes nothing, so it works on a per-thread-suspended frame.
    // Both branches were right about their own tree; only the merge can see that together they close
    // the gap the refusal was describing.
    let sub = server.call(
        "debug.evaluate",
        serde_json::json!({
            "expression": "SuspendProbe.layoutLoginMap[\"ADTURISMO\"]",
            "thread_id": &tid,
        }),
    );
    assert_contains_all(
        "a structural subscript needs no invocation, so a per-thread-suspended frame can read it",
        &sub,
        &["layout-adturismo", "read structurally"],
    );

    // --- something that genuinely INVOKES, and its refusal ---
    //
    // A method call is the case that has no structural route, so it is what still proves the JDWP rule:
    // invocation is granted only to a thread suspended BY AN EVENT.
    let called = server.call(
        "debug.evaluate",
        serde_json::json!({
            "expression": "SuspendProbe.layoutLoginMap.size()",
            "thread_id": &tid,
        }),
    );
    assert_contains_all(
        "the refusal explains itself rather than passing INVALID_THREAD out",
        &called,
        &["INVALID_THREAD", "suspended BY AN EVENT", "debug.pause"],
    );

    // --- a write, proved by the probe rather than by the reply ---
    let wrote = server.call(
        "debug.set_value",
        serde_json::json!({"target": "n", "value": "4242", "thread_id": &tid, "frame_index": frame}),
    );
    assert!(wrote.contains("Set local n"), "writing a local off a suspended frame failed: {wrote}");
    server.call("debug.resume_thread", serde_json::json!({"thread_id": tid}));
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| worker_tick(l, "worker-a").is_some_and(|n| n == 4243))
            .is_some(),
        "the write reported success but the worker never printed the value it should have produced — \
         which is exactly the shape of failure this repo keeps finding.\n  probe output: {:?}",
        probe.output()
    );
}

/// Suspends are counted, so two suspends need two resumes — and the reply must say so rather than
/// report a success it did not achieve (ADR-0003, arriving at the per-thread door).
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn suspending_twice_then_resuming_once_says_the_thread_is_still_suspended() {
    let Some(jdk) = jdk_or_skip("suspending_twice_then_resuming_once_says_the_thread_is_still_suspended")
    else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "SuspendProbe").expect("launch SuspendProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    probe.attach(&mut server);
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| worker_tick(l, "worker-c").is_some())
        .expect("worker-c never ticked");

    let tid = thread_id_named(&mut server, "worker-c");
    server.call("debug.suspend_thread", serde_json::json!({"thread_id": &tid}));
    let twice = server.call("debug.suspend_thread", serde_json::json!({"thread_id": &tid}));
    assert_contains_all(
        "a second suspend reports the depth it built",
        &twice,
        &["Suspend depth 2", "2 resumes"],
    );

    std::thread::sleep(PIPE_SETTLE);
    let frozen = highest_worker_tick(&probe, "worker-c").unwrap_or(-1);

    let once = server.call("debug.resume_thread", serde_json::json!({"thread_id": &tid}));
    assert!(
        once.contains("STILL suspended"),
        "one resume against a depth of 2 leaves the thread stopped, and claiming success is the SAFE-7 \
         bug through a new door: {once}"
    );

    // The probe agrees with the reply, which is the point — a tool that admits it failed is only useful
    // if the admission is true. The probe's stdout stays the primary witness and the suspended listing does
    // NOT replace it: `debug.suspend_thread` reports success whether it froze one thread, all of them or
    // none, and so does any other reply of ours.
    //
    // TEST-43 (#128). This used to OR two `wait_for_line`s that ran IN SEQUENCE, each bounded by
    // `EVENT_TIMEOUT` — so the failing path cost ~50 s, which made this the slowest test in the run whenever
    // it failed, against a 2.99 s entry in `timings.tsv`. Both workers are now watched inside ONE window.
    //
    // The window is still `EVENT_TIMEOUT` rather than something derived from the 120 ms tick interval, and
    // that is deliberate: shortening the PASS condition on a box that starves a probe JVM would convert a
    // rare flake into a frequent one, and nobody has measured how long these workers actually go quiet under
    // 40 threads on 4 cores. What is fast instead is detecting the BUG — the listing is read each pass, and a
    // second suspended thread fails immediately rather than after the window.
    let bases: Vec<(&str, i64)> =
        ["worker-a", "worker-b"].iter().map(|w| (*w, highest_worker_tick(&probe, w).unwrap_or(-1))).collect();
    let deadline = std::time::Instant::now() + EVENT_TIMEOUT;
    let mut others_moved = false;
    let mut listing = String::new();
    while !others_moved {
        others_moved =
            bases.iter().any(|(who, base)| highest_worker_tick(&probe, who).is_some_and(|n| n > base + 1));
        if others_moved {
            break;
        }
        // The one reading that separates "we froze more than we said" from "this JVM got no CPU". SAFE-11's
        // claim is that exactly ONE thread is held, so a second name here is the bug itself and there is no
        // reason to keep waiting for it.
        listing = server.call("debug.list_threads", serde_json::json!({"only_suspended": true}));
        assert!(
            listing.starts_with("0/") || listing.starts_with("1/"),
            "the debugger is holding more than the one thread it was asked to, which is SAFE-11's headline \
             claim breaking — and the other workers' silence is a consequence rather than the finding:\n  \
             {listing}"
        );
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        others_moved,
        "neither worker-a nor worker-b advanced two ticks in {EVENT_TIMEOUT:?}. What that does NOT establish \
         is a VM-wide freeze: the suspended listing below says exactly one thread is held, so the debugger \
         held only what it was asked to and these two stopped for some other reason — a starved probe JVM \
         and a dead one both look like this. The tick numbers and the tail decide which, and they are here \
         rather than left to a second sighting. Ticks at the start were {bases:?}, the probe has printed {} \
         line(s) in all.\n  suspended: {listing}\n  probe's last 8 lines: {:?}",
        probe.output().len(),
        probe.output().iter().rev().take(8).collect::<Vec<_>>(),
    );
    assert_eq!(
        highest_worker_tick(&probe, "worker-c").unwrap_or(-1),
        frozen,
        "worker-c ran after one resume of a depth of 2, so the honest-looking reply was wrong"
    );

    // The second resume is the one that frees it.
    let second = server.call("debug.resume_thread", serde_json::json!({"thread_id": tid}));
    assert_contains_all("the second resume frees it", &second, &["Resumed thread", "running again"]);
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| worker_tick(l, "worker-c").is_some_and(|n| n > frozen))
            .is_some(),
        "worker-c never ran again after both suspends were cleared.\n  probe output: {:?}",
        probe.output()
    );
}

/// **Finished** and **vanished** are different findings (`CONTEXT.md`), and DUMP-4 (#47) is what happened
/// when a reply confused them. Neither may come back as a bare JDWP error.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_finished_thread_and_a_vanished_thread_get_different_readings() {
    let Some(jdk) = jdk_or_skip("a_finished_thread_and_a_vanished_thread_get_different_readings") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "SuspendProbe").expect("launch SuspendProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    probe.attach(&mut server);
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("ephemeral-worker ready"))
        .expect("the ephemeral worker never announced itself");

    // Its id must be taken while it is alive — that is what makes it FINISHED rather than VANISHED
    // afterwards, and `main` holding a reference is what keeps the Thread object from being collected.
    let doomed = thread_id_named(&mut server, "ephemeral-worker");
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("ephemeral-worker done"))
        .expect("the ephemeral worker never ended");
    // The line is printed by the thread's last statement, so give the JVM a moment to actually retire it.
    std::thread::sleep(std::time::Duration::from_millis(500));

    let finished = server.call("debug.suspend_thread", serde_json::json!({"thread_id": doomed}));
    assert_contains_all(
        "a finished thread is refused as finished",
        &finished,
        &["FINISHED", "ZOMBIE", "never suspended"],
    );
    assert!(
        !finished.contains("VANISHED"),
        "a finished thread is still a row the debugger can read — calling it vanished is DUMP-4's \
         confusion: {finished}"
    );

    // An id the JVM never handed out is the vanished shape: valid syntax, no object behind it.
    let vanished =
        server.call("debug.suspend_thread", serde_json::json!({"thread_id": "0xdeadbeefdeadbee0"}));
    assert_contains_all(
        "an unknown id reads as vanished",
        &vanished,
        &["VANISHED", "weak reference", "debug.list_threads"],
    );
    assert!(
        !vanished.contains("FINISHED"),
        "a vanished thread has no identity left to describe, so it is not the finished reading: \
         {vanished}"
    );

    // And an id that is not an id at all is a third answer again, not either of those.
    let nonsense = server.call("debug.suspend_thread", serde_json::json!({"thread_id": "worker-a"}));
    assert_contains_all("a non-id says so", &nonsense, &["not a thread id", "debug.list_threads"]);
}

/// `debug.panic` is the escape hatch, so it must release per-thread suspends too — and SAY it did. A
/// VM-wide resume stops as soon as the thread it probes reaches zero, so a held worker can survive a
/// panic that reported "resumed all threads".
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn panic_releases_a_per_thread_suspend_and_names_it() {
    let Some(jdk) = jdk_or_skip("panic_releases_a_per_thread_suspend_and_names_it") else { return };
    let mut probe = Probe::launch(&jdk, "SuspendProbe").expect("launch SuspendProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    probe.attach(&mut server);
    probe.wait_for_line(EVENT_TIMEOUT, |l| worker_tick(l, "worker-a").is_some()).expect("no tick");

    let tid = thread_id_named(&mut server, "worker-a");
    // Twice, so a single VM-wide resume could not have cleared it by accident.
    server.call("debug.suspend_thread", serde_json::json!({"thread_id": &tid}));
    server.call("debug.suspend_thread", serde_json::json!({"thread_id": tid}));
    std::thread::sleep(PIPE_SETTLE);
    let frozen = highest_worker_tick(&probe, "worker-a").unwrap_or(-1);

    let said = server.call("debug.panic", serde_json::json!({}));
    assert_contains_all(
        "panic names what it released",
        &said,
        &["Also released", "worker-a", "debug.suspend_thread"],
    );
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| worker_tick(l, "worker-a").is_some_and(|n| n > frozen))
            .is_some(),
        "worker-a never ticked again after debug.panic said it had released it — a rescue that reports \
         success it did not achieve is the exact shape of every safety bug here.\n  said: {said}\n  \
         probe output: {:?}",
        probe.output()
    );
    // And the claim expires: nothing is held any more, so nothing is advertised as held.
    let sessions = server.call("debug.list_sessions", serde_json::json!({}));
    assert!(
        !sessions.contains("suspended by you"),
        "panic released the thread, so the listing must stop saying it holds one:\n{sessions}"
    );
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
    let mut probe = Probe::launch(&jdk, "ReloadProbe").expect("launch ReloadProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = eval_probe_running(&jdk);
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = eval_probe_running(&jdk);
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
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
    let mut probe = Probe::launch_delayed(&jdk, "EvalProbe", std::time::Duration::from_secs(8))
        .expect("launch EvalProbe");

    // The trap in three lines: attaching succeeds against a debuggee that has run no code at all, and the
    // discovery tool then answers correctly and uselessly. This is what the three tests were asserting on.
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
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
    assert_contains_all("unfetched class", &missing, &["is not loaded", "debug.list_classes"]);

    vec![m, twice, own, chain, missing]
}

/// DISC-5 (#53): the other half of the same question — what state a type HOLDS, for a caller who has the
/// type and no instance to expand.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn list_fields_renders_java_declarations_and_marks_static() {
    let Some(jdk) = jdk_or_skip("list_fields_renders_java_declarations_and_marks_static") else { return };
    let mut probe = eval_probe_running(&jdk);
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
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
    assert!(!own.contains("not loaded"), "a class that resolved must never be called unfetched: {own}");

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
    assert_contains_all("unfetched class", &missing, &["is not loaded", "debug.list_classes"]);

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
    let mut probe = eval_probe_running(&jdk);
    // `examples/probes` is a source root of exactly the shape the tool expects: EvalProbe is in the
    // default package, so its file sits directly in the root with no package directories between.
    let root = probe_source_path("EvalProbe").parent().expect("probe source has a parent").to_path_buf();
    let root_str = root.to_string_lossy().into_owned();
    let mut server = Server::start_with_env(&[("JDWP_SOURCE_ROOTS", &root_str)]).expect("start server");
    probe.attach(&mut server);

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
    let unfetched = server.call("debug.source", serde_json::json!({"class_name": "com.example.NoSuchThing"}));
    assert_contains_all("unfetched class", &unfetched, &["is not loaded", "debug.list_classes"]);

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

/// TEST-28 (#105): the probe-compile cache must key on the **debug-info flavour**, and this is what says so.
///
/// The cache compiles each `(javac, flag, source)` once per run. Its most dangerous possible bug is serving a
/// `-g` build to a caller that asked for `-g:none`, because the one test that cares about the difference —
/// [`source_says_when_a_loaded_class_carries_no_source_file_attribute`] below — would then assert the
/// *absence* of a line table against a class file that has one, and **pass**.
///
/// This test exists because that hazard turned out to be undefended. Removing `debug_info` from the cache key
/// on purpose left the whole suite green: `StrippedProbe` is the only source ever compiled `-g:none`, and the
/// one place it is *also* compiled `-g` (`a_class_with_no_debug_info_cannot_be_checked_and_says_so`) hands the
/// result to `debug.check_stale`, whose answer does not depend on what is in it. So the property was correct
/// and argued rather than enforced, and an argument does not fail a build.
///
/// Asserted on the class files directly rather than through a JVM, because the claim is about javac's output
/// and nothing else: two compilations of one source differ, the `-g` one carries `LineNumberTable`, and the
/// `-g:none` one does not.
#[test]
#[ignore = "needs a JDK; run with --ignored"]
fn the_probe_compile_cache_keeps_stripped_and_debug_builds_apart() {
    let Some(jdk) = jdk_or_skip("the_probe_compile_cache_keeps_stripped_and_debug_builds_apart") else {
        return;
    };
    let with_debug = tempfile::tempdir().expect("tempdir");
    let stripped = tempfile::tempdir().expect("tempdir");

    // Order matters: stripped FIRST, so a cache that ignored the flavour would have the `-g:none` entry
    // already warm and would serve it to the `-g` request below. That is the failing direction, and asking
    // for them the other way round would let the bug hide.
    jdk.compile_probe_stripped("StrippedProbe", stripped.path()).expect("compile -g:none");
    jdk.compile_probe("StrippedProbe", with_debug.path()).expect("compile -g");

    let stripped_bytes = std::fs::read(stripped.path().join("StrippedProbe.class")).expect("read stripped");
    let debug_bytes = std::fs::read(with_debug.path().join("StrippedProbe.class")).expect("read -g");

    // `LineNumberTable` is a constant-pool UTF-8 entry, so it is findable in the raw bytes without decoding
    // the class file — and its presence/absence is exactly what `-g:none` is being asked for here.
    //
    // Asserted BEFORE the byte comparison on purpose: when the flavour is missing from the cache key all
    // three assertions here fail, and the one that fires first should name the cause rather than a symptom.
    // With stripped compiled first, the failing direction is the `-g` request being served the stripped
    // entry — verified by removing `debug_info` from the key and reading which assertion fires.
    //
    // `assert!` rather than `assert_ne!` for the byte check: `assert_ne!` on two `Vec<u8>` prints both of
    // them, which is 1.4 kB of decimal noise in place of a sentence.
    let names_line_table = |bytes: &[u8]| bytes.windows(15).any(|w| w == b"LineNumberTable");
    assert!(
        names_line_table(&debug_bytes),
        "the `-g` build of StrippedProbe has no LineNumberTable. Since `-g:none` was compiled first, the \
         likely cause is the compile cache serving the stripped entry to a `-g` request — i.e. the \
         debug-info flavour is not part of its key"
    );
    assert!(
        !names_line_table(&stripped_bytes),
        "the `-g:none` build carries a LineNumberTable, so it is not stripped — either javac ignored the flag \
         or the cache served a `-g` build for it, and the absent-line-table path TEST-14 (#39) exists for is \
         unreachable again"
    );
    assert!(
        stripped_bytes != debug_bytes,
        "`-g:none` and `-g` of the same source came out byte-identical ({} bytes each), so the cache served \
         one build for both flavours",
        stripped_bytes.len()
    );
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
    let mut probe = Probe::launch_stripped(&jdk, "StrippedProbe").expect("launch StrippedProbe");
    // Wait for it to actually be RUNNING, not merely accepting a JDWP connection. The agent listens
    // before the main class is loaded, and this test's entire premise is the difference between "loaded
    // but stripped" and "not loaded" — so racing the class load turns the assertion into a coin flip that
    // reports the wrong finding when it loses. It lost on CI's JDK 11 leg while passing everywhere else.
    // The launch is not `launch_running` only because the class files are built differently here; the wait
    // is the same one, and so is the failure text (TEST-17, #49).
    probe.wait_until_running(EVENT_TIMEOUT, |l| tick_index(l).is_some()).unwrap_or_else(|e| panic!("{e}"));
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let stripped =
        server.call("debug.source", serde_json::json!({"class_name": "StrippedProbe", "source_roots": []}));
    assert_contains_all(
        "no SourceFile attribute",
        &stripped,
        &["NO source file", "-g:none", "debug.list_methods"],
    );
    // Not the unfetched answer. The class is right there and the attribute is what is missing, so a reply
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
    let mut probe = Probe::launch_with_smap(&jdk, "SmapProbe").expect("launch SmapProbe");
    // Same race as the stripped probe above: the agent listens before the classes are loaded. Waiting for
    // a tick settles both at once — the heartbeat prints `Neighbour.touched`, so a tick proves the control
    // class is loaded too, and the control is the whole reason a passing SMAP assertion means anything.
    probe.wait_until_running(EVENT_TIMEOUT, |l| tick_index(l).is_some()).unwrap_or_else(|e| panic!("{e}"));
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "ChurnProbe").expect("launch ChurnProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "ContendedProbe").expect("launch ContendedProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "SyntheticProbe").expect("launch SyntheticProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "SyntheticProbe").expect("launch SyntheticProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
         calling it unfetched is wrong about a class the debugger is looking straight at:\n{methods}"
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
         call it unfetched:\n{fields}"
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
    let mut probe = Probe::launch(&jdk, "PrimitiveProbe").expect("launch PrimitiveProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    //
    // `byte[]` and `char[]` are the two that no longer render as elements: EVAL-7 (#81) made them decode
    // to text, because a bag of signed integers is what made `WSIntegradorLog.dsRequest` unreadable. They
    // stay in this table because the property under test is unchanged — the same value read three ways
    // must render the same way — and because the marking survived the move: `1`/`-2`/`127` are a NUL-ish
    // control, an octet that is not valid UTF-8, and DEL, and all three are shown as the octets they are
    // rather than as replacement characters. `PrimitiveProbe.sBytes#raw` is the way back to the element
    // list, and is asserted below.
    let arrays = [
        ("bs", "sBytes", "bs", "byte[3] UTF-8 \"\\x01\\xfe\\x7f\""),
        ("ss", "sShorts", "ss", "short[3]{(short) -300, (short) 0, (short) 300}"),
        ("cs", "sChars", "cs", "char[3] UTF-16 \"aZ\\uD800\""),
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
    //
    // The `char[]` now renders as text (EVAL-7, #81), so the property is asserted in the form the text
    // render takes — `\uD800`, not `?` — and then again on the element itself, which is the route that
    // still reaches `format_char` directly and is where the wording lives.
    assert!(
        !stack.contains("\"aZ?\""),
        "a lone surrogate must not render as a literal '?', which is a value the debuggee could really \
         hold:\n{stack}"
    );
    assert!(
        stack.contains("char[3] UTF-16 \"aZ\\uD800\""),
        "it renders as the code unit it is, escaped rather than replaced:\n{stack}"
    );
    assert_eq!(
        evaluated(&server.evaluate("PrimitiveProbe.sChars[2]")),
        "(char) '\\uD800' (unpaired surrogate, not a character)",
        "read one element at a time it still says what a lone surrogate IS, which the text render has no \
         room for"
    );
    // The way back to the octets, for the byte[] that is genuinely not text (EVAL-7).
    assert_eq!(
        evaluated(&server.evaluate("PrimitiveProbe.sBytes#raw")),
        "byte[3]{(byte) 1, (byte) -2, (byte) 127}",
        "`#raw` renders the elements, because a byte[] really can be a hash or a serialised object"
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

/// The tick number out of a line that prints it AFTER other text — `SwapProbe`'s `answer 1 tick 12`,
/// `TenantProbe`'s `handled infotravel tick 12`.
///
/// Separate from [`tick_index`], which keys on a line *starting* with `tick `, because these probes put
/// their payload first — and a test that cannot read the tick cannot tell "the change did nothing" from
/// "the JVM never resumed".
fn trailing_tick(line: &str) -> Option<i64> {
    let (_, after) = line.split_once(" tick ")?;
    after.split_whitespace().next()?.parse().ok()
}

/// The highest tick a probe printing `… tick <n>` has reached. [`highest_tick`]'s counterpart for the
/// probes whose tick is not at the start of the line.
///
/// Needed separately because reaching for `highest_tick` here does not fail loudly in a useful way: it
/// returns `None`, the wait it feeds can never match, and the test fails after the full timeout claiming
/// the probe froze while the output plainly shows it ticking. Costly to read, so `SwapProbe` and
/// `TenantProbe` use this one.
fn trailing_tick_max(probe: &Probe) -> Option<i64> {
    probe.output().iter().filter_map(|l| trailing_tick(l)).max()
}

/// The answer `SwapProbe` last printed, and the tick it printed it on.
fn last_answer(probe: &Probe) -> Option<(i64, i64)> {
    probe.output().iter().rev().find_map(|l| {
        let answer = l.strip_prefix("answer ")?.split_whitespace().next()?.parse().ok()?;
        Some((answer, trailing_tick(l)?))
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
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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

/// DUMP-6 (#88): a pool parked at one site is ONE entry with a count, not N rows.
///
/// Pool exhaustion with zero log output is the highest single-incident cost in the target stack — neither
/// payment service sets any HTTP timeout, all 15 `ClientBuilder.newClient()` sites use Jersey's infinite
/// defaults, `client.close()` appears zero times — and `thread_dump` is the only instrument that can
/// explain it. 200 threads in `socketRead0` beneath one call site is one fact, and printing it 200 times
/// spent the limit hiding the finding.
///
/// `ManyThreadsProbe` parks 60 workers three frames deep at one `GATE.wait()`, all named `worker-N`, so
/// they are one name family with one identical stack — exactly the shape.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_pool_parked_at_one_site_collapses_into_one_counted_entry() {
    let Some(jdk) = jdk_or_skip("a_pool_parked_at_one_site_collapses_into_one_counted_entry") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "ManyThreadsProbe").expect("launch ManyThreadsProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let base = highest_tick(&probe).expect("no tick to count from");

    // Room for the whole JVM, and the budget out of the way, so this test is about the grouping and not
    // about either of the truncations.
    let dump = server.call(
        "debug.thread_dump",
        serde_json::json!({"suspend": true, "limit": 200, "max_suspend_ms": 120_000}),
    );

    assert!(
        dump.contains("×60 \"worker-#\""),
        "60 identical stacks must be one counted entry, not 60 rows:\n{}",
        head_of(&dump)
    );
    assert_contains_all(
        "the entry says what it is and which threads it stands for",
        &dump,
        &["IDENTICAL stack", "ids: 0x"],
    );
    assert_contains_all(
        "and collapsed is kept apart from the three shortfalls that mean something IS missing",
        &dump,
        &["NOT OMITTED, TRUNCATED OR VANISHED", "monitor are never collapsed"],
    );

    // Criterion 2: `main` is at a different site and keeps its own row. Grouping must not merge the JVM.
    assert!(dump.contains("\"main\""), "threads at other sites stay separate:\n{}", head_of(&dump));

    // The stack is printed once for the group rather than 60 times, which is the whole saving.
    assert_eq!(
        dump.matches("ManyThreadsProbe.level3").count(),
        1,
        "the shared stack must appear exactly once:\n{}",
        head_of(&dump)
    );

    // Criterion 3, as a packet count rather than a duration. Grouping is presentation over rows already
    // collected — `dump_groups` takes `&[DumpRow]` and no connection, so it cannot send anything — and the
    // observable form of that is the same per-thread bound an ungrouped dump has to meet.
    let (read, total) = dump_thread_counts(&dump).expect("no thread count in the dump header");
    let packets = dump_packet_cost(&dump).expect("no packet cost in the dump");
    assert!(read >= 60, "expected the whole pool, got {read}/{total}:\n{}", head_of(&dump));
    let per_thread = packets / read;
    assert!(
        per_thread <= 20,
        "a grouped dump cost {per_thread} packets per thread ({packets} for {read} threads) — grouping \
         must add no round trips, and the bound is the one an ungrouped dump already meets"
    );

    // ADR-0003: the VM really was released, which only the probe's own output can show.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2)).is_some(),
        "the probe stopped ticking after a grouped dump — it was not resumed\n  output: {:?}",
        probe.output(),
    );
}

/// A group's count is over the threads the dump **read**, and the reply has to say so.
///
/// Selection happens before any stack is fetched (ADR-0013), so whether the threads the limit withheld
/// share the stack is not knowable without reading them — which is the cost grouping is not allowed to
/// add. Saying "×40" while 20 more sit unread would be a count that reads as a population.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_truncated_grouped_dump_says_its_count_is_over_what_it_read() {
    let Some(jdk) = jdk_or_skip("a_truncated_grouped_dump_says_its_count_is_over_what_it_read") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "ManyThreadsProbe").expect("launch ManyThreadsProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");

    // The default limit against 60 workers plus the JVM's own threads: the limit binds.
    let dump =
        server.call("debug.thread_dump", serde_json::json!({"suspend": true, "max_suspend_ms": 120_000}));

    let (read, total) = dump_thread_counts(&dump).expect("no thread count in the dump header");
    assert!(read < total, "this test needs the limit to bind, got {read}/{total}:\n{}", head_of(&dump));
    assert!(dump.contains('×'), "the workers it did read must still collapse:\n{}", head_of(&dump));
    assert_contains_all(
        "the count is scoped to what was read, and the way to widen it is named",
        &dump,
        &["over the threads this dump READ", "whether THOSE share the stack is unknown", "Raise limit"],
    );
    assert!(
        dump.contains("more thread(s) (raise limit"),
        "the withheld footer is still its own separate fact:\n{}",
        head_of(&dump)
    );
}

/// TRACE-11 (#93): two expressions on one traced stop point, so a **disagreement** is one snapshot.
///
/// The issue's own case: the schema a thread is really serving lives in a static `ThreadLocal` whose unset
/// value silently resolves to a default, and the session carries the schema it believes it is using. There
/// is no correlation id anywhere in the target codebase, so two stop points on the line — which do both
/// record (BP-6) — leave two independently budgeted streams to join by hand. One stop point with two
/// expressions answers it directly.
///
/// The probe disagrees on two hits in every three rather than always, which is the point of asserting both
/// outcomes: a probe where the values always differed would pass even if the second expression were
/// secretly reading the first.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn two_trace_expressions_record_both_values_in_one_snapshot() {
    let Some(jdk) = jdk_or_skip("two_trace_expressions_record_both_values_in_one_snapshot") else { return };
    let mut probe = Probe::launch_running(&jdk, "TenantProbe", |l| l.starts_with("handled "))
        .expect("launch TenantProbe");
    let line = probe_line(&probe_source("TenantProbe"), "TRACE_LINE");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let armed = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "TenantProbe", "line": line, "trace": true,
            "trace_expr": ["schema", "sessao.getNmSchema()"],
        }),
    );
    assert!(armed.contains("bp_"), "the logpoint must arm: {armed}");
    assert_contains_all(
        "the arm reply numbers the expressions, so they can be matched to the snapshot's slots",
        &armed,
        &["Trace expr[0]: schema", "Trace expr[1]: sessao.getNmSchema()"],
    );
    assert!(!armed.contains("clamped"), "two is well under the cap: {armed}");

    // `infotravel` can only appear in slot 0, and only on a hit where the ThreadLocal was unset — which is
    // the disagreement this whole feature exists to make visible.
    let traces = server
        .wait_for_traces("infotravel", EVENT_TIMEOUT)
        .expect("the logpoint never recorded a hit whose schema fell through to the default");
    let disagreeing = traces
        .lines()
        .find(|l| l.contains("TenantProbe.handle:") && l.contains("infotravel"))
        .unwrap_or_else(|| panic!("no hit line carrying the default schema in:\n{traces}"));

    // BOTH slots on the SAME line. This is the assertion the feature is for: one snapshot, two values, and
    // they do not match — which no pair of separately budgeted streams could have shown without a join.
    assert!(
        disagreeing.contains("schema => \"infotravel\""),
        "slot 0 must carry the local, labelled with its own expression: {disagreeing}"
    );
    assert!(
        disagreeing.contains("sessao.getNmSchema() => \"orinter\""),
        "slot 1 must carry the getter's result in the same snapshot: {disagreeing}"
    );

    // And the agreeing hits are recorded too — the stop point is not filtering, it is reporting.
    let agreeing = traces
        .lines()
        .find(|l| l.contains("TenantProbe.handle:") && l.contains("schema => \"orinter\""))
        .or_else(|| {
            server
                .wait_for_traces("schema => \"orinter\"", EVENT_TIMEOUT)
                .as_deref()
                .and_then(|t| t.lines().find(|l| l.contains("schema => \"orinter\"")).map(str::to_string))
                .map(|_| "found")
        });
    assert!(agreeing.is_some(), "hits where the two agree must be recorded as well:\n{traces}");

    // TRACE-2's discipline: two evaluations per hit must still resume the thread. Only the probe says so.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| trailing_tick(l).is_some_and(|n| n > 4)).is_some(),
        "probe stopped ticking under a two-expression logpoint — a hit left it suspended\n  output: {:?}",
        probe.output(),
    );
}

/// One expression must keep the reply and the rendering it had before TRACE-11, and an element that fails
/// must not take the others with it.
///
/// The second half is the normal case rather than an edge: a chain goes null on some hits and not others,
/// which is the same reasoning that makes a batched arming reply per-pattern instead of one verdict.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn one_expression_is_unnumbered_and_a_failing_element_keeps_the_others() {
    let Some(jdk) = jdk_or_skip("one_expression_is_unnumbered_and_a_failing_element_keeps_the_others") else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "TenantProbe", |l| l.starts_with("handled "))
        .expect("launch TenantProbe");
    let line = probe_line(&probe_source("TenantProbe"), "TRACE_LINE");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // A single string: unnumbered label, and the rendering every earlier trace test asserts against.
    let single = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "TenantProbe", "line": line, "trace": true, "trace_expr": "schema",
        }),
    );
    assert!(single.contains("Trace expr: schema"), "one expression stays unnumbered: {single}");
    assert!(!single.contains("Trace expr[0]"), "numbering one of one would be new noise: {single}");
    let single_bp = single
        .lines()
        .find_map(|l| l.strip_prefix("   Stop-point ID: "))
        .expect("no stop-point id in the reply")
        .to_string();
    server.wait_for_traces("schema => ", EVENT_TIMEOUT).expect("the single-expression logpoint never fired");
    server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": single_bp}));

    // Now three, the middle one naming something that is not in scope at all.
    let mixed = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "TenantProbe", "line": line, "trace": true,
            "trace_expr": ["schema", "noSuchThing.atAll", "sessao.getNmSchema()"],
        }),
    );
    assert!(mixed.contains("bp_"), "a list containing a bad expression must still arm: {mixed}");

    let traces = server
        .wait_for_traces("noSuchThing.atAll => ", EVENT_TIMEOUT)
        .expect("the failing element never got a slot of its own");
    let hit = traces
        .lines()
        .find(|l| l.contains("noSuchThing.atAll => "))
        .unwrap_or_else(|| panic!("no hit line in:\n{traces}"));

    assert!(hit.contains("noSuchThing.atAll => <error:"), "the failure lands in its own slot: {hit}");
    assert!(hit.contains("schema => \""), "and the element before it survives: {hit}");
    assert!(hit.contains("sessao.getNmSchema() => \""), "as does the one after it: {hit}");
}

/// The ceiling, and that it is reported rather than silently applied. A caller who asked for six values and
/// read four would otherwise conclude the two missing ones evaluated to nothing.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn too_many_trace_expressions_are_clamped_and_the_reply_names_what_it_dropped() {
    let Some(jdk) = jdk_or_skip("too_many_trace_expressions_are_clamped_and_the_reply_names_what_it_dropped")
    else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "TenantProbe", |l| l.starts_with("handled "))
        .expect("launch TenantProbe");
    let line = probe_line(&probe_source("TenantProbe"), "TRACE_LINE");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let armed = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "TenantProbe", "line": line, "trace": true,
            "trace_expr": ["schema", "i", "sessao", "sessao.getNmSchema()", "schema", "i"],
        }),
    );

    assert!(armed.contains("bp_"), "a clamped request still arms: {armed}");
    assert_contains_all(
        "the clamp says what was asked, what was kept and what was dropped",
        &armed,
        &["6 expressions", "cap", "DROPPED", "capture window"],
    );
    assert!(armed.contains("Trace expr[3]"), "four are kept: {armed}");
    assert!(!armed.contains("Trace expr[4]"), "and the fifth is not listed as armed: {armed}");
}

/// TRACE-12 (#117): a suspending stop point converts every traced stop point on the same line into a
/// VM-freezing one, and until this nothing said so.
///
/// **Asserted against the probe's own stdout**, which the issue makes a criterion, and rightly: the whole
/// bug is that every *reply* looked correct. `debug.list_stop_points` printed an unqualified `(trace)` for
/// a stop point that was freezing the VM on every hit, so a test that only read the debugger's words would
/// have passed before the fix and after it.
///
/// The order here is the one a caller stumbles into: trace first — proven cheap by the probe still
/// ticking — then a suspending stop point on the same line, after which the probe must stop.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_suspending_stop_point_escalates_a_trace_on_the_same_line_and_says_so() {
    let Some(jdk) = jdk_or_skip("a_suspending_stop_point_escalates_a_trace_on_the_same_line_and_says_so")
    else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let traced = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "SwapProbe", "line": 39, "trace": true}),
    );
    assert!(traced.contains("Stop-point ID"), "the traced stop point must arm: {traced}");
    assert!(
        !traced.contains("FREEZE THE VM ANYWAY"),
        "nothing suspends here yet, so there is nothing to warn about:\n{traced}"
    );

    // The premise: a lone traced stop point really does leave the probe running. Without this the freeze
    // below could be anything.
    let before = trailing_tick_max(&probe).unwrap_or(0);
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| trailing_tick(l).is_some_and(|n| n > before + 2)).is_some(),
        "a traced stop point alone must not freeze the probe\n  output: {:?}",
        probe.output(),
    );

    // Now the same line, suspending. The reply has to name the stop point whose behaviour just changed.
    let suspending =
        server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "SwapProbe", "line": 39}));
    assert_contains_all(
        "the arm reports that it just escalated somebody else's stop point",
        &suspending,
        &["MAKES 1 TRACED STOP POINT(S) FREEZE THE VM", "bp_1", "event set", "Clearing this stop point"],
    );

    // The claim, from the debuggee. `STUCK_CONFIRM` rather than `EVENT_TIMEOUT`: ~20 ticks fit in it, so
    // ruling out progress does not need the full wait — a negative observation costs what the positive
    // would have (TEST-30).
    let frozen_at = trailing_tick_max(&probe).unwrap_or(0);
    let advanced =
        probe.wait_for_line(STUCK_CONFIRM, |l| trailing_tick(l).is_some_and(|n| n > frozen_at + 2)).is_some();
    assert!(
        !advanced,
        "the probe kept ticking, so the traced stop point was NOT escalated and this test is no longer \
         testing the thing it was written for\n  output: {:?}",
        probe.output(),
    );

    // And the listing, which is where somebody asking "why did the VM freeze?" actually looks.
    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert!(
        listed.contains("(trace — SUSPEND POLICY OVERRIDDEN)"),
        "an unqualified (trace) here is the most misleading thing this listing could print:\n{listed}"
    );
    assert_contains_all(
        "and it explains itself rather than just flagging",
        &listed,
        &["DOES freeze the VM on every hit", "bp_2"],
    );

    // Leave the probe running for the harness's reaping.
    server.panic_reset();
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| trailing_tick(l).is_some_and(|n| n > frozen_at + 2)).is_some(),
        "the probe never recovered after the stop points were cleared\n  output: {:?}",
        probe.output(),
    );
}

/// The other order, which the issue calls the likelier one: a suspending stop point is already on the line
/// and somebody adds a `trace_expr` expecting the cheap thing.
///
/// The VM is frozen at the suspending breakpoint while the trace is armed, which is exactly the state a
/// caller is in when they reach for a trace — they are looking at a suspended thread and want the next hit
/// recorded without one.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn arming_a_trace_onto_an_already_suspending_line_warns_it_will_not_be_cheap() {
    let Some(jdk) = jdk_or_skip("arming_a_trace_onto_an_already_suspending_line_warns_it_will_not_be_cheap")
    else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let suspending =
        server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "SwapProbe", "line": 39}));
    assert!(suspending.contains("Stop-point ID"), "the suspending stop point must arm: {suspending}");
    assert!(
        !suspending.contains("FREEZE THE VM"),
        "nothing is traced here yet, so there is nothing to warn about:\n{suspending}"
    );

    let traced = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "SwapProbe", "line": 39, "trace": true, "trace_expr": "v"}),
    );
    assert!(traced.contains("Stop-point ID"), "the trace is accepted, not refused: {traced}");
    assert_contains_all(
        "and it says the trace will freeze the VM anyway, naming what is responsible",
        &traced,
        &["THIS TRACE WILL FREEZE THE VM ANYWAY", "bp_1", "does NOT make this cheap", "event set"],
    );

    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert!(
        listed.contains("(trace — SUSPEND POLICY OVERRIDDEN)"),
        "the listing must not claim this one snapshots and resumes:\n{listed}"
    );

    server.panic_reset();
}

/// The control, and the one that decides whether the two warnings above are worth having: two traced stop
/// points on one line are `EventThread` plus `EventThread`, which is still `EventThread`. Nothing is
/// escalated, the probe keeps running, and neither reply may mention freezing.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn two_traced_stop_points_on_one_line_do_not_warn_and_do_not_freeze() {
    let Some(jdk) = jdk_or_skip("two_traced_stop_points_on_one_line_do_not_warn_and_do_not_freeze") else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let args = serde_json::json!({"class_pattern": "SwapProbe", "line": 39, "trace": true});
    let first = server.call("debug.set_line_stop", args.clone());
    let second = server.call("debug.set_line_stop", args);

    for (which, reply) in [("first", &first), ("second", &second)] {
        assert!(reply.contains("Stop-point ID"), "the {which} trace must arm: {reply}");
        assert!(!reply.contains("FREEZE THE VM"), "false escalation warning on the {which}:\n{reply}");
    }

    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert!(listed.contains("(trace)"), "both must still read as plain traces:\n{listed}");
    assert!(!listed.contains("OVERRIDDEN"), "nothing was overridden:\n{listed}");

    let before = trailing_tick_max(&probe).unwrap_or(0);
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| trailing_tick(l).is_some_and(|n| n > before + 2)).is_some(),
        "two traced stop points on one line must not freeze the probe\n  output: {:?}",
        probe.output(),
    );
    server.panic_reset();
}

/// The JVMTI error code out of a `debug.reload_class` refusal — `… SCHEMA_CHANGE_NOT_IMPLEMENTED (64).`
fn refused_code(reply: &str) -> Option<u16> {
    let (_, after) = reply.split_once("_NOT_IMPLEMENTED (")?;
    after.split(')').next()?.parse().ok()
}

/// One DISC-13 case: an edit to `SwapProbe`, and the refusal codes a structural diff should predict.
struct ForecastCase {
    what: &'static str,
    edit: SourceEdit,
    /// Every code the forecast must name. The JVM answers with **one** of them — it stops at the first
    /// restriction it reaches — so agreement means "what the JVM said is among what we predicted".
    predicted: &'static [u16],
}

/// DISC-13 (#97): the forecast agrees with the JVM, which is the only way it earns any trust.
///
/// The pre-flight is worth more than the attempt because the attempt fails about half the time — of the
/// 300 most recent `.java`-touching commits in the target repo, 151 were structural and 149 body-only,
/// and the churn concentrates in the classes where a redefine is most awkward. But a forecast that
/// disagrees with the JVM is worse than none, so every prediction here is checked against a real
/// `RedefineClasses`.
///
/// All three refusal cases share one probe on purpose: a refused redefinition changes **nothing** (the
/// command is all-or-nothing), and this test asserts that too by continuing to use the JVM afterwards.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn each_predicted_refusal_matches_the_code_the_jvm_answers() {
    let Some(jdk) = jdk_or_skip("each_predicted_refusal_matches_the_code_the_jvm_answers") else { return };
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let cases = [
        ForecastCase {
            what: "an added field",
            edit: Box::new(|src: String| {
                src.replace(
                    "    static int answer() {",
                    "    static int extra = 7;\n\n    static int answer() {",
                )
            }),
            predicted: &[64],
        },
        ForecastCase {
            what: "an added method",
            edit: Box::new(|src: String| {
                src.replace(
                    "    static int answer() {",
                    "    static void audit() { }\n\n    static int answer() {",
                )
            }),
            predicted: &[63],
        },
        ForecastCase {
            // Not a "changed method": one member gone, another arrived, so both codes are predicted and
            // the JVM picks whichever restriction it reaches first.
            what: "a changed method signature",
            edit: Box::new(|src: String| {
                src.replace("    static int answer() {", "    static long answer() {")
                    .replace("int v = 1; // SWAP_VALUE", "long v = 1; // SWAP_VALUE")
            }),
            predicted: &[63, 67],
        },
    ];

    for case in cases {
        let dir = tempfile::tempdir().expect("tempdir for the variant");
        jdk.compile_probe_variant("SwapProbe", dir.path(), case.edit)
            .unwrap_or_else(|e| panic!("compile the variant for {}: {e}", case.what));
        let root = serde_json::json!([dir.path().display().to_string()]);

        let forecast = server
            .call("debug.check_stale", serde_json::json!({"class_name": "SwapProbe", "class_roots": root}));
        assert!(
            forecast.contains("WILL BE REFUSED"),
            "{} must be forecast as a refusal:\n{forecast}",
            case.what
        );
        for code in case.predicted {
            assert!(
                forecast.contains(&format!("({code})")),
                "{} must predict code {code}:\n{forecast}",
                case.what
            );
        }

        let attempted = server
            .call("debug.reload_class", serde_json::json!({"class_name": "SwapProbe", "class_roots": root}));
        let actual = refused_code(&attempted)
            .unwrap_or_else(|| panic!("{} was not refused by the JVM at all:\n{attempted}", case.what));
        assert!(
            case.predicted.contains(&actual),
            "the JVM answered {actual} for {}, which the forecast did not predict ({:?}). A prediction \
             that disagrees with the real outcome is the one thing this feature must not do:\n{attempted}",
            case.what,
            case.predicted,
        );
        // A refusal is all-or-nothing, and the reply now says the caller could have known in advance.
        assert!(
            attempted.contains("predictable from the class file"),
            "a foreseeable refusal must point at the pre-flight:\n{attempted}"
        );
    }

    // The probe survived three refused redefinitions, still running the original bytecode.
    assert_eq!(
        last_answer(&probe).map(|(answer, _)| answer),
        Some(1),
        "three refusals must have changed nothing\n  output: {:?}",
        probe.output(),
    );
}

/// The other half, and the case the forecast must NOT cry refusal on: a body-only edit installs.
///
/// The positive verdict is deliberately weaker than the negative — "no structural change detected", not
/// a promise — so this asserts both that the forecast is clean AND that the swap really works, which is
/// what makes the wording honest rather than merely cautious.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_body_only_change_is_forecast_clean_and_the_jvm_installs_it() {
    let Some(jdk) = jdk_or_skip("a_body_only_change_is_forecast_clean_and_the_jvm_installs_it") else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let edited = swap_probe_returning(&jdk, 7);
    let root = serde_json::json!([edited.path().display().to_string()]);

    let forecast =
        server.call("debug.check_stale", serde_json::json!({"class_name": "SwapProbe", "class_roots": root}));
    assert_contains_all(
        "a body-only change is forecast clean, and the wording promises nothing",
        &forecast,
        &["NO STRUCTURAL CHANGE DETECTED", "NOT a promise", "dry_run"],
    );
    assert!(!forecast.contains("WILL BE REFUSED"), "false refusal on a body-only edit:\n{forecast}");

    let done = server
        .call("debug.reload_class", serde_json::json!({"class_name": "SwapProbe", "class_roots": root}));
    assert!(done.contains("Reloaded"), "the JVM must accept a body-only change:\n{done}");

    // The forecast said it would install; the probe's own stdout is what proves it did.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("answer 7 tick ")).is_some(),
        "the swap did not take effect\n  output: {:?}",
        probe.output(),
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

/// Call `debug.source` for `SwapProbe` with both kinds of root named explicitly.
///
/// Both are passed per-call rather than at attach because the two roots are what each of these tests
/// varies, and a session default would make it ambiguous which one an assertion is about.
fn probe_source_window(
    server: &mut Server,
    source_root: &std::path::Path,
    class_root: Option<&std::path::Path>,
) -> String {
    let class_roots: Vec<String> = class_root.map(|r| r.display().to_string()).into_iter().collect();
    server.call(
        "debug.source",
        serde_json::json!({
            "class_name": "SwapProbe",
            "line": 39,
            "context": 3,
            "source_roots": [source_root.display().to_string()],
            "class_roots": class_roots,
        }),
    )
}

/// DISC-11 (#87): the source window says when it does not match the bytecode the JVM loaded.
///
/// The axis this covers is the JVM against the build. `debug.check_stale` answers it when asked, and the
/// caller this ruins is the one reading code and not suspecting anything — so it is reported here, on the
/// reply that actually shows them the wrong lines.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_source_window_over_stale_bytecode_says_so() {
    let Some(jdk) = jdk_or_skip("a_source_window_over_stale_bytecode_says_so") else { return };
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    // The JVM runs the checked-in probe; the build on disk has two lines inserted above `answer()`, so
    // every line number in it moved. The source under `src` matches that build, which keeps this test
    // about the build-versus-JVM axis alone.
    let shifted = swap_probe_with_shifted_lines(&jdk);
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let out = probe_source_window(&mut server, &shifted.path().join("src"), Some(shifted.path()));

    assert_contains_all(
        "the window reports drift it was not asked about, and names both the file and the remedy",
        &out,
        &["STALE BYTECODE", "SwapProbe.class", "reload_class", "check_stale"],
    );
    // A warning, not a refusal: the lines the caller asked for must still be there.
    assert!(out.contains("SwapProbe.java"), "the source window itself must survive the warning:\n{out}");
}

/// The acceptance criterion the issue is most explicit about: **a matching build adds nothing**. An
/// unsolicited aside that fires on a correct reply is how a reader learns to skip the asides, and this
/// tool's reply is read on almost every call.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_source_window_over_the_running_build_is_unchanged() {
    let Some(jdk) = jdk_or_skip("a_source_window_over_the_running_build_is_unchanged") else { return };
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    // `compile_probe_variant` refuses a no-op edit, so the change is a comment: it moves no line and
    // alters no bytecode, and it writes the `.class` after the `.java` — the normal order.
    let current = tempfile::tempdir().expect("tempdir");
    jdk.compile_probe_variant("SwapProbe", current.path(), |src| {
        src.replace("// SWAP_VALUE", "// SWAP_VALUE (control build)")
    })
    .expect("recompile the unmodified probe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let out = probe_source_window(&mut server, &current.path().join("src"), Some(current.path()));

    assert!(out.contains("SwapProbe.java"), "the window must still render:\n{out}");
    for noise in ["STALE", "NOT CHECKED", "NOT WHAT THIS BYTECODE", "SOURCE IS NEWER"] {
        assert!(!out.contains(noise), "false positive {noise:?} on the running build:\n{out}");
    }
}

/// The proof on the other axis, and the one the issue was actually filed about: the build matches the
/// JVM, and the **source** does not match either. In the environment it was measured in the class roots
/// were byte-identical to the deployed jars and two commits behind `src/main/java`, so the JVM-versus-
/// build comparison is clean and the caller is still reading the wrong statement.
///
/// A truncated file is the case that can be *proved* without compiling anything: the JVM's line table
/// names a line the file does not have, and a file cannot be missing lines the compiler emitted from it.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_source_file_too_short_for_the_running_line_table_is_reported() {
    let Some(jdk) = jdk_or_skip("a_source_file_too_short_for_the_running_line_table_is_reported") else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    // A class root holding the very build the JVM is running, so axis one is clean by construction.
    let matching = tempfile::tempdir().expect("tempdir for the matching build");
    jdk.compile_probe("SwapProbe", matching.path()).expect("compile the unmodified probe");
    // A source root holding a SwapProbe.java far too short to be what that build was compiled from.
    let short = tempfile::tempdir().expect("tempdir for the truncated source");
    let full = std::fs::read_to_string(probe_source_path("SwapProbe")).expect("read the probe source");
    let head: Vec<&str> = full.lines().take(8).collect();
    std::fs::write(short.path().join("SwapProbe.java"), head.join("\n")).expect("write the short source");

    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let out = probe_source_window(&mut server, short.path(), Some(matching.path()));

    assert_contains_all(
        "the source axis is reported as a proof, with both numbers",
        &out,
        &["NOT WHAT THIS BYTECODE WAS COMPILED FROM", "8 line(s)", "recompile"],
    );
    assert!(
        !out.contains("STALE BYTECODE"),
        "the deployed build matches the JVM and must not be blamed for the source being wrong:\n{out}"
    );
}

/// The third distinct answer the issue requires: with nothing to compare against, the reply says the
/// check did not run. Silence here would be read as a clean bill of health, and in the target
/// environment it is the common case — the toolkit configures neither kind of root.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_source_window_with_no_class_root_says_it_could_not_check() {
    let Some(jdk) = jdk_or_skip("a_source_window_with_no_class_root_says_it_could_not_check") else {
        return;
    };
    let probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let probes = probe_source_path("SwapProbe");
    let probes = probes.parent().expect("examples/probes");
    let mut server = Server::start().expect("start server");
    attach_with_class_roots(&mut server, probe.port, None);

    let out = probe_source_window(&mut server, probes, None);

    assert!(out.contains("SwapProbe.java"), "the window must still render:\n{out}");
    assert_contains_all(
        "not checked is stated, and is not allowed to read as a pass",
        &out,
        &["NOT CHECKED", "not the same as checked and fine", "class_roots"],
    );
    assert!(!out.contains("STALE"), "nothing was compared, so nothing may be called stale:\n{out}");
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
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
            probe.wait_for_line(EVENT_TIMEOUT, |l| trailing_tick(l).is_some_and(|t| t > before.1)).is_some(),
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
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start_with_env(&[("JDWP_READONLY", "1")]).expect("start server");
    probe.attach(&mut server);

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
        probe.wait_for_line(EVENT_TIMEOUT, |l| trailing_tick(l).is_some_and(|t| t > tick)).is_some(),
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
    let mut probe = Probe::launch_running(&jdk, "SwapProbe", |l| l.starts_with("answer 1 tick "))
        .expect("launch SwapProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch_stripped(&jdk, "StrippedProbe").expect("launch StrippedProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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
    let mut probe = Probe::launch(&jdk, "ChainProbe").expect("launch ChainProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

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

/// BP-4 (#78): one source line, several bytecode copies — arm all of them.
///
/// `javac` inlines a `finally` body once per exit path, so the line is in the line table twice. The
/// resolver used to take the first match, which is the normal-completion copy in ascending code-index
/// order, and the stop point then reported the calls that SUCCEEDED and stayed silent on the throw.
/// Silence that is indistinguishable from "the code never ran", on the one site — request and response
/// both still in scope on both paths — that makes a `finally` the idiomatic logpoint.
///
/// Both halves are asserted on purpose. Checking only that the exception-path copy fires would also pass
/// on a resolver that armed the *last* location instead of the first, which is the same bug rotated.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_finally_line_arms_every_copy_javac_emitted() {
    let Some(jdk) = jdk_or_skip("a_finally_line_arms_every_copy_javac_emitted") else { return };
    let mut probe = Probe::launch(&jdk, "FinallyProbe").expect("launch FinallyProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // The class must already be LOADED, or the arm legitimately defers and returns a different reply.
    // A class loads on first use, so one `finally` line means `call()` has run and `FinallyProbe` is in
    // the JVM. Without this the test passes on an idle box and fails roughly one run in eight under the
    // full suite, where the probe is slower to get going than the server is to arm.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("finally rq="))
        .expect("probe never reached the finally block, so its class never loaded");

    let src = probe_source("FinallyProbe");
    let line = probe_line(&src, "// BP1");

    // Trace mode, so the probe keeps driving both paths while this watches. `rs` is what separates the
    // copies: "OK" on normal completion, still null when the throw carried us into the same line.
    let set = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "FinallyProbe", "line": line, "trace": true, "trace_expr": "rs",
        }),
    );
    assert_contains_all(
        "a finally line reports that it armed more than one location",
        &set,
        &["bp_", "Armed at 2 locations", "finally"],
    );

    // The probe's own stdout first: without this the trace assertions below could pass vacuously on a
    // run where the throwing call never happened. The debugger reports success either way.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("finally ") && l.contains("rs=OK"))
        .expect("probe never completed call() normally");
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("finally ") && l.contains("rs=null"))
        .expect("probe never drove call() down the throwing path");

    // And now the debugger has to have seen BOTH. Pre-fix, only the normal-completion copy is armed, so
    // every record reads rs="OK" and this is the assertion that fails.
    // Wait for EACH copy's record, not for "a record". `wait_for_traces` returns on the first match, and
    // the probe drives the two paths in sequence — so waiting once reads the buffer between the normal
    // hit and the throwing one and fails on whichever has not landed yet. That is a race in the test,
    // not a flake in the debugger, and it cost a full-suite run on JDK 21 to see.
    server
        .wait_for_traces("rs => \"OK\"", EVENT_TIMEOUT)
        .expect("no traced hit from the normal-completion copy of the finally line");
    server
        .wait_for_traces("rs => null", EVENT_TIMEOUT)
        .expect("no traced hit from the exception-path copy of the finally line — this is BP-4");
    let traces = server.call("debug.get_traces", serde_json::json!({}));
    let rows: Vec<&str> = traces.lines().filter(|l| l.contains("FinallyProbe.call")).collect();
    assert!(
        rows.iter().any(|l| l.contains("\"OK\"")),
        "no traced hit from the normal-completion copy of the finally line: {rows:?}"
    );
    assert!(
        rows.iter().any(|l| l.contains("rs=null")),
        "no traced hit from the exception-path copy of the finally line — this is BP-4: the stop point \
         armed the path that worked and went silent on the one being debugged: {rows:?}"
    );

    // ADR-0005 survives the change: one caller-facing id over two armed requests, so the stop point is
    // listed once and cleared once.
    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_eq!(
        listed.matches("bp_1").count(),
        1,
        "a two-location stop point must be listed once, not per armed request: {listed}"
    );
    assert!(
        listed.contains("Armed at 2 locations"),
        "the listing must say the stop point covers more than one location: {listed}"
    );

    let cleared = server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": "bp_1"}));
    assert_contains_all("one clear removes every armed location", &cleared, &["✅", "bp_1"]);
    let after = server.call("debug.list_stop_points", serde_json::json!({}));
    assert!(after.contains("No breakpoints set"), "clear left something armed: {after}");

    server.panic_reset();
}

/// BP-4 (#78), the half that is easy to get wrong in the other direction: a stop point that owns two
/// armed requests must charge its trace budget **once per hit**, not once per armed location.
///
/// Charging per location would halve the budget's meaning silently — the caller asks for 6 hits, the
/// buffer holds 3, and nothing anywhere says why. Asserted through the auto-disarm rather than by
/// reading a counter, because the disarm is the observable the caller actually has: it is what stops
/// the recording.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_multi_location_stop_point_charges_its_budget_once_per_hit() {
    let Some(jdk) = jdk_or_skip("a_multi_location_stop_point_charges_its_budget_once_per_hit") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "FinallyProbe").expect("launch FinallyProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // The class must already be LOADED, or the arm legitimately defers and returns a different reply.
    // A class loads on first use, so one `finally` line means `call()` has run and `FinallyProbe` is in
    // the JVM. Without this the test passes on an idle box and fails roughly one run in eight under the
    // full suite, where the probe is slower to get going than the server is to arm.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("finally rq="))
        .expect("probe never reached the finally block, so its class never loaded");

    let src = probe_source("FinallyProbe");
    let line = probe_line(&src, "// BP1");

    // 6 rather than 2: the probe drives both copies once per tick, so a per-location charge would
    // disarm after three ticks with three records, and a per-hit charge after six hits.
    let set = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "FinallyProbe", "line": line, "trace": true, "trace_expr": "rs",
            "trace_max_hits": 6,
        }),
    );
    assert_contains_all(
        "armed with a small budget",
        &set,
        &["bp_1", "Armed at 2 locations", "Auto-disarms after 6"],
    );

    // Wait for the DISARM, not for a tick count. Capture is serialised, so under load the probe can be
    // ten ticks in while the sixth hit is still being recorded, and an exact-count assertion then reads
    // a buffer that is still filling. The disarm notice is the event that means "the budget is spent",
    // which is the precondition this assertion actually needs.
    server
        .wait_for_traces("reached its trace-hit budget", EVENT_TIMEOUT)
        .expect("the traced stop point never ran its budget out");

    let traces = server.call("debug.get_traces", serde_json::json!({}));
    let rows = traces.lines().filter(|l| l.contains("FinallyProbe.call")).count();
    assert_eq!(
        rows, 6,
        "a budget of 6 must buy 6 recorded hits across both armed locations, not 3 — per-location \
         charging halves the budget with nothing in any reply to explain it:\n{traces}"
    );

    server.panic_reset();
}

/// FILT-10 (#110): `list_stop_points` reports how many times each stop point has actually fired.
///
/// The field existed and was never written. Both construction sites set it to `0`, nothing anywhere
/// incremented it, and the render was behind `if hit_count > 0` — so the `Hits:` line had never once
/// printed, and a stop point that had fired four hundred times listed **identically** to one that had
/// never fired. That is the failure `debug.check_stale`'s description, DISC-8's drift warning and BP-4's
/// `Armed at N locations` note all exist to prevent, with the counter simply absent: silence reading as
/// an answer.
///
/// Three things are asserted here and each would pass without the other two:
///
///  - **A fired stop point reports its count, once per hit and not once per armed location.** The
///    `finally` line owns two JDWP requests (BP-4), so a per-location tally would report 12 where 6 is
///    right — the same rotation of the same bug the trace budget was fixed for. Anchored on the
///    auto-disarm rather than a tick count, for the reason the budget test gives: capture is serialised,
///    so an exact count read while the buffer is still filling is a race in the test.
///  - **A different kind counts too**, on a number chosen not to collide with the first, since both are
///    read out of one listing.
///  - **An armed stop point that has never fired reports `Hits: 0`, printed rather than omitted.** This
///    is the half that makes the other two mean anything. If zero were suppressed, a caller could not
///    tell "this build counts and the answer is none" from "this build does not count" — which is
///    exactly the reading the old code left them with, and no assertion on a non-zero count can catch
///    it.
///
/// The probe's own stdout is checked first in each case. The debugger reports a plausible tally either
/// way, and only the probe knows how many times its code really ran.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_listing_says_how_many_times_each_stop_point_has_fired() {
    let Some(jdk) = jdk_or_skip("a_listing_says_how_many_times_each_stop_point_has_fired") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "FinallyProbe").expect("launch FinallyProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // The class must already be LOADED, or the arm legitimately defers and returns a different reply.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("finally rq="))
        .expect("probe never reached the finally block, so its class never loaded");

    let src = probe_source("FinallyProbe");
    let line = probe_line(&src, "// BP1");

    // 6, on a line javac emitted twice. A per-location tally reports 12.
    server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "FinallyProbe", "line": line, "trace": true, "trace_expr": "rs",
            "trace_max_hits": 6,
        }),
    );
    // 4, and deliberately not 6: both tallies land in one listing, so equal numbers would let a
    // rendering that printed the same stop point twice pass.
    server.call(
        "debug.set_exception_stop",
        serde_json::json!({
            "class_pattern": "FinallyProbe$GatewayException", "trace": true, "trace_max_hits": 4,
        }),
    );
    // Never thrown by this probe, so this one is the never-fired case. `java.lang.ArithmeticException`
    // is loaded in every JVM, so the arm resolves and is real rather than deferred.
    let never = server.call(
        "debug.set_exception_stop",
        serde_json::json!({"class_pattern": "java.lang.ArithmeticException", "trace": true}),
    );
    assert!(never.contains("exc_"), "the never-fired exception stop did not arm: {never}");

    // The probe's own account first: without it every assertion below could pass vacuously on a run
    // where the probe never got going.
    //
    // TEST-33: the failure has to say HOW FAR the probe got, because that is the whole diagnosis and the
    // original message did not carry it. This test arms **two self-disarming** traced stop points (budgets
    // of 6 and 4) on a probe whose forward progress is the assertion — the only place in the suite that
    // combines those — so a stall here is the in-flight-hit window TRACE-8 (#72) is about: the budget
    // reaches zero, the request is disarmed, and a hit the JVM had already generated arrives for a request
    // that is gone. If that hit is not resumed, the probe's only thread stays suspended forever.
    //
    // Reached tick 0 or nothing => stalled, and that is a product bug worth chasing.
    // Reached tick 1-2 => merely slow, and this budget is too tight for the concurrency TEST-32 introduced.
    // The two want opposite fixes, so the message names which it was rather than leaving it to be guessed.
    if probe.wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("tick 3")).is_none() {
        let out = probe.output();
        let ticks: Vec<&String> = out.iter().filter(|l| l.starts_with("tick ")).collect();
        let reached = ticks.len();
        let verdict = if reached == 0 {
            "the probe printed NO tick at all — it is stalled, not slow. Suspect a traced hit that was \
             never resumed (TRACE-8's in-flight window, which the two self-disarming budgets here open)."
        } else if reached <= 2 {
            "the probe was still ticking but had not reached tick 3 — this is SLOWNESS, not a stall, so \
             the timeout is too tight for the capture cost under this concurrency rather than anything \
             being frozen."
        } else {
            "the probe passed tick 3 but the matching line was not seen — look at the predicate, not the \
             debuggee."
        };
        panic!(
            "probe never reached its fourth tick, so the finally line ran fewer than 8 times.\n  \
             DIAGNOSIS: {verdict}\n  ticks printed: {reached} (last few: {:?})\n  full probe output: {:?}",
            ticks.iter().rev().take(3).rev().collect::<Vec<_>>(),
            out,
        );
    }

    // Both budgets have to be spent before the listing is read, or a tally is caught mid-fill.
    server
        .wait_for_traces("reached its trace-hit budget", EVENT_TIMEOUT)
        .expect("no traced stop point ran its budget out");

    // Both stop points, not just the one that tripped the notice above: the two disarm independently and
    // `wait_for_traces` returns on the first match, so reading the listing straight after it catches
    // whichever budget was still filling.
    let deadline = std::time::Instant::now() + EVENT_TIMEOUT;
    let mut listed = String::new();
    while std::time::Instant::now() < deadline {
        listed = server.call("debug.list_stop_points", serde_json::json!({}));
        if hits_for(&listed, "bp_1") == Some(6) && hits_for(&listed, "exc_2").is_some_and(|n| n >= 4) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }

    assert_eq!(
        hits_for(&listed, "bp_1"),
        Some(6),
        "a stop point armed at 2 locations and hit 6 times must report 6, not 12 — a per-location tally \
         is BP-4's bug rotated, and nothing in any reply would explain the doubling:\n{listed}"
    );
    // Deliberately not an exact number, and the reason is the point rather than a weakened assertion.
    // EXC-3 folds a rethrow of an instance already captured: it is NOT charged to the trace budget, but
    // it IS a throw the JVM reported for this request, so the tally counts it and the two numbers
    // legitimately differ. How many times one `GatewayException` is re-reported as it leaves a `finally`
    // is the JVM's business and moves between JDKs. What must hold is that the tally is at least the
    // budget the stop point spent — a tally that merely tracked captures would be an alias for a number
    // already on the line above it.
    let exc_hits = hits_for(&listed, "exc_2")
        .unwrap_or_else(|| panic!("the exception stop point reported no tally at all:\n{listed}"));
    assert!(
        exc_hits >= 4,
        "an exception stop point that spent a budget of 4 must have been hit at least 4 times, got \
         {exc_hits}:\n{listed}"
    );
    assert_eq!(
        hits_for(&listed, "exc_3"),
        Some(0),
        "an armed stop point that has never fired must report `Hits: 0` rather than nothing — a missing \
         line cannot be told apart from a build that does not count, which is the whole of FILT-10:\
         \n{listed}"
    );

    server.panic_reset();
}

/// The `Hits:` tally `debug.list_stop_points` printed for one stop-point id, or `None` if that stop
/// point had no tally line at all.
///
/// Reads *within* the stop point's own block rather than grepping the whole listing, because several
/// stop points in one listing can legitimately share a number and a bare `contains("Hits: 6")` would
/// then pass on a renderer that printed one stop point's tally against another's id.
fn hits_for(listing: &str, id: &str) -> Option<u32> {
    let marker = format!("[{id}]");
    listing
        .lines()
        .skip_while(|l| !l.contains(&marker))
        .skip(1)
        .take_while(|l| !l.contains(" ["))
        .find_map(|l| l.trim().strip_prefix("Hits: ").and_then(|n| n.trim().parse().ok()))
}

/// EVAL-11 (#98): an enum constant or a `public static final` field works in ARGUMENT position, as a
/// `Map` subscript, and is scored on its runtime type when overloads compete.
///
/// **This is a regression test for a capability that already worked, not a test for new code**, and that
/// is worth stating rather than leaving for someone to infer from a thin diff. #98 was filed on the
/// premise that "a dotted static path in argument position does not resolve". Measured against a live
/// JVM before writing anything, every one of its acceptance criteria already passed — because an
/// argument that is not a literal is parsed as a full expression and resolved through the *same* head
/// resolver `debug.evaluate` uses, which has handled static paths since DISC-1. The brief's own advice
/// ("reuse it rather than writing a second resolver") had already been followed by the shape of the
/// code.
///
/// What was genuinely missing is this test. Nothing in the suite passed a static reference as an
/// argument, so the capability held by accident of a shared resolver and could have stopped holding the
/// same way — silently, in the one place a caller would read as "the debugger cannot express this".
///
/// The overload pair is the part that could regress without any error appearing. `describe` exists for
/// `SupplierKey` and for `Object`; a resolver that handed the invoke a reference without its runtime
/// type would pick `describe(Object)` and return a plausible string. The two return different prefixes
/// so the assertion can tell which method actually ran.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn an_enum_constant_and_a_static_field_work_as_call_arguments() {
    let Some(jdk) = jdk_or_skip("an_enum_constant_and_a_static_field_work_as_call_arguments") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "EnumArgProbe").expect("launch EnumArgProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // A suspended frame, because invoking anything needs a thread suspended BY AN EVENT.
    let line = probe_line(&probe_source("EnumArgProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "EnumArgProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in EnumArgProbe.main never fired");

    let eval = |server: &mut Server, expr: &str| {
        server.call("debug.evaluate", serde_json::json!({"expression": expr}))
    };

    // A SIMPLE-name enum constant as an argument, and the overload scored on its runtime type.
    let simple = eval(&mut server, "EnumArgProbe.describe(SupplierKey.OMNIBEES)");
    assert!(
        simple.contains("\"enum:OMNIBEES\""),
        "a simple-name enum constant must resolve in argument position AND be scored as its own type — \
         `object:` here means it landed on the describe(Object) overload, which looks like success:\
         \n{simple}"
    );

    // Fully qualified, which is what a caller writes when the simple name is ambiguous.
    let fq = eval(&mut server, "EnumArgProbe.describe(EnumArgProbe.MARKER)");
    assert!(
        fq.contains("\"object:static-marker\""),
        "a `public static final` field must resolve in argument position; a String has no more specific \
         overload here, so describe(Object) is the correct answer:\n{fq}"
    );

    // An enum constant as a Map subscript — the two-level session-pool read #98 names.
    let sub = eval(&mut server, "EnumArgProbe.pool[SupplierKey.OMNIBEES]");
    assert!(sub.contains("\"omnibees-pool\""), "an enum constant must work as a Map subscript:\n{sub}");

    // A wrong constant names the missing CONSTANT rather than failing to parse. The likely case on an
    // enum with hundreds of values, which is why #98 called it out.
    let wrong = eval(&mut server, "EnumArgProbe.describe(SupplierKey.NOPE)");
    assert!(
        wrong.contains("has no static field 'NOPE'"),
        "a typo'd constant must say which field is missing from which class, not report a parse error:\
         \n{wrong}"
    );

    server.panic_reset();
}

/// STEP-1 (#94): `debug.step_into` skips framework and JDK frames by default, and the old behaviour is
/// one argument away.
///
/// Before this, a step request carried a `Step` modifier and **nothing else** — no `ClassExclude`, no
/// `ClassOnly`. On the target stack that made `step_into` close to unusable: a `JAX-RS` request on `WildFly`
/// arrives through dozens of framework frames, and the tool's own description conceded that a mis-step
/// "can cost several more steps to escape".
///
/// `StepFilterProbe` is built for exactly this and nothing else. Its marked line calls
/// `List.sort(...)` — real JDK code with real line numbers — and the comparator is a method of the probe
/// itself, so execution goes *ours → java.util → ours*. That shape is what makes the two arms below
/// distinguishable, and it is also what stops a blunter implementation passing: something that merely
/// stepped OUT to the caller would skip the callback into application code as well.
///
/// Both arms run against the same probe in one test on purpose. Asserting only the filtered arm would
/// pass on a JVM that never steps into `java.util` at all, which would make the whole feature untested
/// while looking green.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn step_into_skips_the_jdk_by_default_and_steps_into_it_when_asked() {
    let Some(jdk) = jdk_or_skip("step_into_skips_the_jdk_by_default_and_steps_into_it_when_asked") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "StepFilterProbe").expect("launch StepFilterProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("tick 1 "))
        .expect("probe never reached its second tick, so StepFilterProbe never loaded");

    let src = probe_source("StepFilterProbe");
    let sort_line = probe_line(&src, "// BP1");
    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "StepFilterProbe", "line": sort_line}),
    );

    // ARM 1 — no filter at all, which is what every step did before STEP-1. The step from the `sort`
    // call has to land inside the JDK, or the second arm below proves nothing.
    server
        .wait_for_event(&format!("\"line\":{sort_line}"), EVENT_TIMEOUT)
        .expect("breakpoint on the sort line never fired");
    let unfiltered_reply = server.call("debug.step_into", serde_json::json!({"exclude_classes": []}));
    assert!(
        unfiltered_reply.contains("No class filter"),
        "an explicitly empty exclusion list must say it is stepping into everything: {unfiltered_reply}"
    );
    server.wait_for_event("\"event\":\"step\"", EVENT_TIMEOUT).expect("unfiltered step never reported");
    let unfiltered = server.last_event();
    assert!(
        unfiltered.contains("java."),
        "with no exclusion a step_into at a `List.sort` call must land in the JDK — if it does not, this \
         probe is not exercising the thing STEP-1 changed and the filtered arm below is vacuous:\
         \n{unfiltered}"
    );

    // ARM 2 — the default. Same line, same call, and it must land back in the probe's own code.
    server.call("debug.continue", serde_json::json!({}));
    server
        .wait_for_event(&format!("\"line\":{sort_line}"), EVENT_TIMEOUT)
        .expect("breakpoint on the sort line never fired a second time");
    let filtered_reply = server.call("debug.step_into", serde_json::json!({}));
    assert_contains_all(
        "the default filter is stated in the reply, with the way to turn it off",
        &filtered_reply,
        &["Stepping OVER", "java.*", "the default set", "exclude_classes:[]"],
    );
    server.wait_for_event("\"event\":\"step\"", EVENT_TIMEOUT).expect("filtered step never reported");
    let filtered = server.last_event();
    assert!(
        filtered.contains("StepFilterProbe"),
        "with the default exclusions a step_into at a `List.sort` call must land in the probe's own \
         code — either the comparator the JDK calls back into, or the next line — and not inside \
         java.util:\n{filtered}"
    );

    server.panic_reset();
}

/// TRACE-8 (#72) on the path a caller actually drives: **clearing a traced stop point must not leave the
/// hit thread frozen.**
///
/// The original fix put the in-flight bookkeeping in `disarm_request`, under a comment saying it therefore
/// covered "the watchdog and a manual `clear_stop_point`". It did not: `clear_stop_point` never calls
/// `disarm_request` — it clears the JDWP requests directly — so the one path a caller drives deliberately
/// was the one still open. TEST-31 (#114) caught it as a probe whose only worker was **frozen for the life
/// of the JVM**, and the heartbeat that found it is why it stopped being a mystery.
///
/// **Nothing else rescues this.** A traced hit suspends only the hit thread (`EventThread` policy) and
/// never calls `mark_suspended`, so the watchdog — which acts on a VM-wide suspension — has no reason to
/// look, and by then the stop point is gone so there would be nothing for it to disarm. That makes it
/// strictly worse than the budget-disarm case the original fix was written against.
///
/// **This test does NOT reproduce that window, and saying so is the point.** It arms a traced stop point on
/// a line the probe runs constantly, waits for a hit, clears it, and asserts the probe is still ticking —
/// twenty times over. Run against the *unfixed* server it still passes, so it is a **guard, not a proof**:
/// it would catch a gross regression in the arm/clear path but it does not land a clear inside the window
/// where the JVM has generated a hit the server has not finished with. Waiting for a recorded trace is
/// precisely what steps past that window, and arming/clearing back to back without waiting did not close
/// on it either.
///
/// So the fix it accompanies rests on **inspection plus a field sighting**, not on this test:
/// `note_disarmed_traced` was reachable only from `disarm_request`, which `clear_stop_point` and
/// `clear_pattern_family` do not call, and #114 observed the resulting frozen worker. A test that
/// genuinely reproduces it needs a way to hold a hit mid-flight — a fault-injection point rather than a
/// timing coincidence — and that does not exist here yet. Recorded rather than papered over, because a
/// test named after a bug it cannot catch is worse than no test.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn clearing_a_traced_stop_point_repeatedly_leaves_the_probe_running() {
    let Some(jdk) = jdk_or_skip("clearing_a_traced_stop_point_repeatedly_leaves_the_probe_running") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some())
        .expect("probe never ticked, so it was never a witness");

    // `bumpCounter` runs on every pass of the worker's loop, so a stop point on it is guaranteed to be in
    // flight often — which is the condition this test needs rather than a coincidence it hopes for.
    for round in 1..=20 {
        let set = server.call(
            "debug.set_line_stop",
            serde_json::json!({
                "class_pattern": "WatchProbe", "method": "bumpCounter", "trace": true,
            }),
        );
        let Some(id) = set
            .split_whitespace()
            .find(|t| t.starts_with("bp_"))
            .map(|t| t.trim_end_matches(|c: char| !c.is_alphanumeric()).to_string())
        else {
            panic!("round {round}: no bp_ id in the arm reply: {set}")
        };

        // Wait until it has really fired, so the clear below lands in the window rather than before it.
        server
            .wait_for_traces("WatchProbe", EVENT_TIMEOUT)
            .unwrap_or_else(|| panic!("round {round}: the traced stop point never recorded a hit"));

        let before = probe.output().iter().filter_map(|l| tick_index(l)).max().unwrap_or(0);
        server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": id}));
        server.call("debug.get_traces", serde_json::json!({"clear": true}));

        // The whole assertion: the probe must keep going. Pre-fix, one of these rounds strands a hit and
        // the only worker never ticks again — and nothing in any reply says so.
        assert!(
            probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > before + 1)).is_some(),
            "round {round}: the probe stopped ticking after its traced stop point was cleared — a hit that \
             was in flight when the request went away was never resumed, so the worker is frozen and no \
             watchdog is watching a per-thread suspend. Last tick before the clear was {before}; probe \
             output: {:?}",
            probe.output(),
        );
    }

    server.panic_reset();
}

/// `MONITOR_WAIT`, JDWP event kind 45 — named here so [`EventFault::DelayKind`] can target the one
/// composite this test needs held, rather than whichever event happens to arrive first. `DuplicateKind`'s
/// own doc records the CI failure that taught this suite not to be positional about event kinds.
const EVENT_KIND_MONITOR_WAIT: u8 = 45;

/// TRACE-8 (#72) on the third path that clears traced requests: `debug.panic`. **A characterisation test,
/// not a bug reproduction** — and the distinction is the most useful thing in it.
///
/// The ledger that stops a traced hit's thread being stranded is `note_disarmed_traced`. TRACE-8 reached it
/// from `disarm_request`; TEST-31 (#114) found `clear_stop_point` never calls that and added
/// `note_traced_in_flight`. `disarm_everything` — panic's clearing half — bypassed both, so a traced hit the
/// JVM had already generated arrives disowned by `try_record_trace`, which returns without resuming it.
///
/// **This test opens that window on purpose and proves the thread survives anyway.** `EventFault::DelayKind`
/// holds the composite for 2500 ms while the panic runs, and `relay.duplicated()` is asserted to be 1 so a
/// green run cannot mean "the window never opened". With `note_every_traced_request_in_flight` removed from
/// `disarm_everything` the waiter *still* keeps advancing: `handle_panic` calls `resume_and_verify` right
/// after the drain, and a VM-wide resume decrements every thread's suspend count — including one nothing was
/// tracking. So the rescue happens, by side effect, from a call made for another reason.
///
/// That is worth pinning down rather than leaving implicit. What this test now guards is the *coupling*: if
/// panic ever stops issuing a VM-wide resume, or issues it conditionally, the disowned hit has nothing left
/// to save it and this test is where that shows up.
///
/// **Unlike TEST-31's guard, the window here is genuinely staged.** #114's test says in its own doc that it
/// cannot land inside it, because the gap between "the JVM generated the hit and suspended the thread" and
/// "the debugger dequeued the composite" is sub-millisecond — measured here as **0 hits in 40 rounds** of
/// arm-then-panic against the real binary. Holding the packet makes the window as wide as the test asks. Only
/// the debuggee→debugger direction stalls, so the panic still reaches the JVM and is answered normally.
///
/// **What this does NOT explain**, said plainly because the test was written while chasing it: a 24-run soak
/// failed `an_invoking_trace_expr_is_refused_on_the_half_that_does_not_own_the_lock` with `1/8 wedge-waiter
/// [running]`. That sighting is real and its cause is still unknown. This test rules out one candidate
/// mechanism; it does not close that issue.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn panic_resumes_a_traced_hit_it_disowned_instead_of_freezing_the_thread() {
    let Some(jdk) = jdk_or_skip("panic_resumes_a_traced_hit_it_disowned_instead_of_freezing_the_thread")
    else {
        return;
    };
    let probe = Probe::launch_running(&jdk, "WedgeProbe", wedge_probe_ready).expect("launch WedgeProbe");
    // The relay sits between the server and the probe and holds the FIRST monitor-wait composite. 2500 ms
    // is not tuned to anything delicate: it only has to outlast the panic below, and the assertion waits
    // for it to elapse rather than racing it.
    let relay = FaultRelay::start_with_events(
        probe.port,
        vec![],
        Some(EventFault::DelayKind { kind: EVENT_KIND_MONITOR_WAIT, ms: 2500, times: 1 }),
    )
    .expect("start the fault relay");
    let mut server = Server::start().expect("start server");
    server.attach(relay.port);

    // A FIELD READ, not an invocation. DUMP-8's hazard is an invoking `trace_expr` on a monitor it does not
    // own, and this test must not be able to fail for that reason — what is under test is the disowning,
    // so the expression has to be one that always resolves cheaply.
    let armed = server.call(
        "debug.set_monitor_stop",
        serde_json::json!({"kinds": ["wait"], "trace": true, "trace_expr": "WAITED_ON.name",
                           "trace_max_hits": 50, "trace_frames": 0}),
    );
    assert!(!armed.contains("Refused"), "the traced monitor stop point was refused: {armed}");

    // `waitOut` runs every ~40 ms plus the loop gap, so a hit is generated almost at once — and the relay
    // is now holding its composite with the waiter suspended by the JVM. Waiting for the probe's own
    // counter to move first would be waiting for the thing this test is about to freeze.
    std::thread::sleep(std::time::Duration::from_millis(700));
    // The relay counts what it held. Without this the test could pass having staged NOTHING — a green run
    // that proves the fix works against a window it never opened, which is the failure mode ADR-0034 and
    // this suite's own history keep pointing at.
    assert_eq!(
        relay.duplicated(),
        1,
        "the relay never held a monitor-wait composite, so the window was never opened and this test \
         proves nothing"
    );
    let waits_before = wedge_waits(&probe).unwrap_or(-1);

    // Panic INSIDE the window. It drains the monitor request without the JVM having told the debugger about
    // the hit, and resumes only what it tracks — which does not include an `EventThread` suspension.
    let panicked = server.call("debug.panic", serde_json::json!({}));
    assert!(
        panicked.contains("Panic"),
        "panic did not report itself, so this test never staged what it claims: {panicked}"
    );

    // Let the held composite arrive and be processed, plus a margin for the disowned-hit resume.
    std::thread::sleep(std::time::Duration::from_secs(3));

    // The debugger's own account first, then the probe's — which is the half no reply of ours could fake.
    let suspended = server.call("debug.list_threads", serde_json::json!({"only_suspended": true}));
    assert!(
        suspended.starts_with("0/"),
        "panic disowned a traced hit and left its thread suspended:\n{suspended}\n  probe output tail: \
         {:?}",
        probe.output().iter().rev().take(3).collect::<Vec<_>>(),
    );
    let advanced = (0..40).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(150));
        wedge_waits(&probe).is_some_and(|now| now > waits_before)
    });
    assert!(
        advanced,
        "the waiter never completed another wait, so it is frozen. waits was {waits_before} and is {:?} \
         now; nothing else would have rescued it — a traced hit's suspension is neither the VM-wide depth \
         nor a counted per-thread hold, so the watchdog has no reason to look",
        wedge_waits(&probe),
    );

    server.panic_reset();
}

/// FILT-8 (#99): a `hit_count` stop point fires on the Nth occurrence, ONCE, and is then **spent** —
/// deleted by the JVM, not by us — and everything downstream has to say so.
///
/// The exactness comes from [`Probe::launch_delayed`]: the JVM is up and answering JDWP before the probe
/// class has run at all, so the stop point arms (deferred, on `CLASS_PREPARE`) with nothing yet executed
/// and "the 3rd hit" is unambiguously the 3rd time the line ran. Arming against a probe already in its
/// loop could only assert a window, because writes land between the reply and the next call.
///
/// Four things, and the last two are the ones that were broken before this change rather than missing:
///
///  - **It fires on the 3rd and not before.** `trace_expr: "i"` records the loop counter, and the one
///    record has to read `i => 2`. The probe's own stdout is the witness that it reached that iteration.
///  - **`trace_max_hits: 200` buys ONE snapshot, not 200.** The two counters sound alike and compose the
///    other way round from how they read; the arm reply says so and the buffer has to agree.
///  - **`list_stop_points` reports it SPENT, not armed.** Nothing tracked that at all before FILT-8: a
///    counted stop point that had fired was listed as armed indefinitely.
///  - **Clearing it sends nothing to the debuggee, and says so.** The alternative — always send the
///    `Clear` and ignore the error — is what most debuggers do and is wrong here, because JDWP request
///    ids are allocated by the debuggee and recur (`CONTEXT.md` § **Request id**), so a `Clear` naming a
///    long-deleted id can land on whatever holds it now.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_counted_stop_point_fires_on_the_nth_hit_and_is_then_spent() {
    let Some(jdk) = jdk_or_skip("a_counted_stop_point_fires_on_the_nth_hit_and_is_then_spent") else {
        return;
    };
    // The JVM answers JDWP for 8s before the probe's first instruction, so the arm happens with nothing
    // executed and the count starts from zero occurrences.
    let probe = Probe::launch_delayed(&jdk, "FinallyProbe", std::time::Duration::from_secs(8));
    let mut probe = probe.expect("launch FinallyProbe delayed");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let src = probe_source("FinallyProbe");
    let line = probe_line(&src, "// BP2");

    let set = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "FinallyProbe", "line": line, "hit_count": 3,
            "trace": true, "trace_expr": "i", "trace_max_hits": 200,
        }),
    );
    assert!(set.contains("bp_1"), "the counted stop point did not arm: {set}");

    // The probe has to actually reach its third iteration, or "one record" below is vacuous.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("tick 4"))
        .expect("probe never reached its fifth tick");

    let traces = server
        .wait_for_traces("i => (int) 2", EVENT_TIMEOUT)
        .expect("the counted stop point never recorded the 3rd hit — Count was not passed through");
    let rows: Vec<&str> = traces.lines().filter(|l| l.contains("FinallyProbe.main")).collect();
    assert_eq!(
        rows.len(),
        1,
        "hit_count:3 with trace_max_hits:200 must yield exactly ONE snapshot — the stop point is spent \
         after its single hit, so the budget never gets a chance to apply:\n{traces}"
    );
    assert!(
        rows[0].contains("i => (int) 2"),
        "the one recorded hit must be the 3rd occurrence (i == 2 on a 0-based loop), not the first:\
         \n{traces}"
    );

    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert!(
        listed.contains("SPENT"),
        "a stop point whose hit_count has fired must be reported SPENT rather than armed — the JVM \
         deleted the request itself and nothing tracked that before FILT-8:\n{listed}"
    );
    assert!(
        !listed.contains("DISABLED"),
        "spent is not the BP-1 toggle: reporting it as DISABLED tells the caller they switched \
         something off that they never touched:\n{listed}"
    );
    assert_eq!(
        hits_for(&listed, "bp_1"),
        Some(1),
        "a counted stop point fires exactly once, so its tally is 1:\n{listed}"
    );

    let cleared = server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": "bp_1"}));
    assert_contains_all(
        "clearing a spent stop point is a no-op that says it was one",
        &cleared,
        &["✅", "already SPENT", "nothing", "was sent to the debuggee"],
    );
    let after = server.call("debug.list_stop_points", serde_json::json!({}));
    assert!(after.contains("No breakpoints set"), "clear left something listed: {after}");

    server.panic_reset();
}

/// FILT-8 (#99): the three kinds that never accepted `hit_count` at all now do, and an exception stop is
/// the one the target stack actually wanted — a supplier `consulta` retried after a sleep, where the
/// SECOND attempt is the interesting one and the first failing is expected.
///
/// Asserted on an exception request rather than all three because the pass-through is one shared shape
/// (the `*_ex` client methods already took `count`, only the call sites passed `None`), while what is
/// worth a live JVM is the part that is NOT shared: that the JVM really does delete the request, that one
/// hit is all you get, and that the arm reply says both of those before it happens rather than after.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn an_exception_stop_takes_a_hit_count_and_says_what_it_buys() {
    let Some(jdk) = jdk_or_skip("an_exception_stop_takes_a_hit_count_and_says_what_it_buys") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "ExcProbe").expect("launch ExcProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // An exception request needs a concrete reference type, so the class must be loaded: no deferring.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("tick 0 "))
        .expect("probe never threw once, so its exception class never loaded");

    let set = server.call(
        "debug.set_exception_stop",
        serde_json::json!({
            "class_pattern": "ExcProbe$SwallowedException", "trace": true, "hit_count": 3,
            "trace_max_hits": 200,
        }),
    );
    assert_contains_all(
        "the arm reply states what a Count buys before it fires, not after",
        &set,
        &["Stops on hit #3", "SPENT", "trace_max_hits: 200 cannot apply"],
    );

    // Far enough past the third throw that "one record" means "it stopped", not "it has not caught up".
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("attempts=12"))
        .expect("probe never reached its twelfth throw");

    let traces = server.call("debug.get_traces", serde_json::json!({}));
    let rows = traces.lines().filter(|l| l.contains("ExcProbe")).count();
    assert_eq!(
        rows, 1,
        "a Count of 3 must produce exactly one record even with a budget of 200, and even after twelve \
         throws — the JVM deletes the request after reporting the 3rd:\n{traces}"
    );

    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert!(
        listed.contains("SPENT"),
        "the exception stop must be reported SPENT once its Count has fired:\n{listed}"
    );
    assert_eq!(hits_for(&listed, "exc_1"), Some(1), "a counted stop point fires once:\n{listed}");

    let cleared = server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": "exc_1"}));
    assert!(
        cleared.contains("already SPENT"),
        "clearing a self-deleted request must say nothing was sent to the debuggee, because sending a \
         Clear for a recurring id is how you clear somebody else's request: {cleared}"
    );

    server.panic_reset();
}

/// FILT-10 (#110), the half the obvious implementation gets wrong: a method-exit stop point counts exits
/// of the method the caller **asked for**, not every exit the JDWP request received.
///
/// JDWP has no method-name modifier. A `mexit_` request narrowed to `classify` is registered as a
/// `ClassMatch` and the JVM reports every method of `ReturnProbe` returning — `other()`, and `classify`
/// on both of its two `return` statements. METH-1 already drops the wrong ones downstream, so the reply
/// a caller reads is correctly filtered; a tally charged in the event pump *before* that filter would
/// nonetheless report several times the real number, on a stop point whose own reply said otherwise.
///
/// Asserted by the exact number rather than by an inequality: the probe calls `other()` once per tick and
/// `classify` twice, so an unfiltered tally would already be past 6 by the time the sixth `classify` exit
/// lands, and "greater than zero" would pass on the bug.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_method_exit_tally_counts_the_asked_for_method_only() {
    let Some(jdk) = jdk_or_skip("a_method_exit_tally_counts_the_asked_for_method_only") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "ReturnProbe").expect("launch ReturnProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // Loaded before arming, and the probe's own `calls=` counter is the witness that `classify` really
    // ran more than six times by the end.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("tick 0 "))
        .expect("probe never printed its first tick, so ReturnProbe never loaded");

    server.call(
        "debug.set_method_exit_stop",
        serde_json::json!({
            "class_pattern": "ReturnProbe", "method": "classify", "trace": true, "trace_max_hits": 6,
        }),
    );
    server
        .wait_for_traces("reached its trace-hit budget", EVENT_TIMEOUT)
        .expect("the traced method-exit stop point never ran its budget out");

    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert!(
        listed.contains("Hits: 6"),
        "a method-exit stop point filtered to `classify` must count 6 exits of `classify`, not every \
         method of the class that returned while it was armed — `other()` returns once per tick and \
         `main` returns too:\n{listed}"
    );

    server.panic_reset();
}

/// EVAL-7 (#81): a `byte[]` reads as text under the charset the caller names, and `array.length`
/// resolves.
///
/// Two gaps that combined into one blocked investigation. Every supplier round trip on this stack is
/// recorded as `WSIntegradorLog.dsRequest` / `dsResponse`, both `byte[]`, so reaching either meant
/// reading a bag of signed integers — with no `new String(bytes)` to express (no constructors, no casts)
/// and no `.length`, because that read routed through the field lookup and a JDWP array type has no
/// field table.
///
/// **The charset is the half most likely to be got wrong, so it is asserted in both directions.**
/// `it-common`'s `Utils` pins the shared JAXB marshaller to `ISO-8859-1`, so Latin-1 payloads are
/// genuinely in circulation; a UTF-8-only decode would corrupt `São Paulo` into something a reader
/// would diagnose as a *supplier* bug. Both payloads are read under both charsets here: the right
/// decode has to produce the text, and the wrong one has to be visibly wrong rather than plausible.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
// One probe, one arrangement, every route into the same renderer — static field, local, chained field,
// array element, deep expansion. The comparison between them IS the test; split up, no half could claim
// the other's setup.
#[allow(clippy::too_many_lines)]
fn byte_arrays_render_as_text_under_the_charset_the_caller_names() {
    let Some(jdk) = jdk_or_skip("byte_arrays_render_as_text_under_the_charset_the_caller_names") else {
        return;
    };
    // `launch_running`, not `launch`: a class loads on first use, so reading loaded state before the
    // probe has executed anything gets a correct "not loaded" and asserts a wrong finding (TEST-17, #49).
    let mut probe =
        Probe::launch_running(&jdk, "BytesProbe", |l| l.starts_with("tick ")).expect("launch BytesProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // --- the charset, both ways round, on the two payloads actually in circulation ---
    //
    // `latin1City` is `São Paulo` as ISO-8859-1: nine octets, and the `ã` is a bare `0xE3`, which is not
    // valid UTF-8 at all. That octet is what the issue is about.
    assert_eq!(
        evaluated(&server.evaluate("BytesProbe.latin1City")),
        r#"byte[9] UTF-8 "S\xe3o Paulo""#,
        "the default decode names UTF-8 and MARKS the octet it could not decode — a lossy decode would \
         have put a U+FFFD there, indistinguishable from one the debuggee really held"
    );
    assert_eq!(
        evaluated(&server.evaluate("BytesProbe.latin1City#ISO-8859-1")),
        r#"byte[9] ISO-8859-1 "São Paulo""#,
        "named the right charset, the same nine octets are the text they always were"
    );
    assert_eq!(
        evaluated(&server.evaluate("BytesProbe.utf8City")),
        r#"byte[10] UTF-8 "São Paulo""#,
        "a UTF-8 payload under the default"
    );
    assert_eq!(
        evaluated(&server.evaluate("BytesProbe.utf8City#latin1")),
        r#"byte[10] ISO-8859-1 "SÃ£o Paulo""#,
        "the wrong charset produces mojibake a reader can recognise, and the reply names the charset it \
         used, so there is something to correct"
    );

    // --- a whole envelope, newlines and all ---
    let envelope = evaluated(&server.evaluate("BytesProbe.log.dsRequest#ISO-8859-1")).to_string();
    assert_contains_all(
        "the WSIntegradorLog shape reads as the envelope it is",
        &envelope,
        &["byte[73] ISO-8859-1", "<?xml version=", "<cidade>São Paulo</cidade>"],
    );
    assert!(
        !envelope.contains('\n'),
        "a decoded payload must stay ONE line — a trace record is one line, and a raw newline would break \
         the record apart: {envelope}"
    );
    assert!(
        envelope.contains("\\n<Envelope>"),
        "the newlines are escaped rather than dropped, so nothing is lost: {envelope}"
    );

    // --- a byte[] that is not text at all, and the way back to the octets ---
    assert_eq!(
        evaluated(&server.evaluate("BytesProbe.blob")),
        r#"byte[4] UTF-8 "\x00\x01\xfe\x7f""#,
        "a blob decodes to nothing but marked octets, which is itself the answer: this is not text"
    );
    assert_eq!(
        evaluated(&server.evaluate("BytesProbe.blob#raw")),
        "byte[4]{(byte) 0, (byte) 1, (byte) -2, (byte) 127}",
        "`#raw` is the way back, because for a hash or a serialised object the octets ARE the answer"
    );

    // --- char[], which carries no charset question: a Java char is already a UTF-16 code unit ---
    assert_eq!(
        evaluated(&server.evaluate("BytesProbe.chars")),
        r#"char[3] UTF-16 "ol\uD800""#,
        "a lone surrogate is not a character, and is escaped rather than replaced (TYPE-1, #48)"
    );

    // --- array.length, on all three array kinds ---
    assert_eq!(
        evaluated(&server.evaluate("BytesProbe.latin1City.length")),
        "(int) 9",
        "a primitive byte[] — the case that used to fail with `No field 'length' found on the object`"
    );
    assert_eq!(evaluated(&server.evaluate("BytesProbe.words.length")), "(int) 3", "an object array");
    assert_eq!(evaluated(&server.evaluate("BytesProbe.numbers.length")), "(int) 5", "a primitive int[]");

    // Chaining still works after an INDEX, and `.length` is terminal because it is a number.
    assert_eq!(
        evaluated(&server.evaluate("BytesProbe.pages[0].length")),
        "(int) 9",
        "an index narrows to one value, so a `.length` still chains off it"
    );
    assert_eq!(
        evaluated(&server.evaluate("BytesProbe.pages[1]")),
        r#"byte[10] UTF-8 "São Paulo""#,
        "an element of a byte[][] is a byte[], and reads as text for the same reason the outer one would"
    );
    assert_contains_all(
        "a chain after `.length` says what it hit rather than inventing a member on an int",
        &server.evaluate("BytesProbe.numbers.length.foo"),
        &["primitive"],
    );

    // An unrecognised selector is an error, not a silent fall back to the default — a caller who typed a
    // charset this tool does not have must not be handed a UTF-8 answer as though it were theirs.
    assert_contains_all(
        "an unknown `#…` selector is refused and names what is accepted",
        &server.evaluate("BytesProbe.latin1City#utf9"),
        &["not a render selector", "ISO-8859-1", "raw"],
    );

    // --- the same renderer reached through a suspended frame's locals ---
    let source = probe_source("BytesProbe");
    let line = probe_line(&source, "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "BytesProbe", "line": line}));
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .unwrap_or_else(|| panic!("the breakpoint in BytesProbe.work never fired"));

    assert_eq!(
        evaluated(&server.evaluate("req#ISO-8859-1")),
        r#"byte[9] ISO-8859-1 "São Paulo""#,
        "a local resolves to the same array the static field did, and renders identically"
    );
    assert_eq!(evaluated(&server.evaluate("req.length")), "(int) 9", "`.length` on a local byte[]");
    assert_contains_all(
        "a byte[] reached through a field of a local object — the `integrador.getIntegradorLogList()` shape",
        &server.evaluate("entry.dsResponse"),
        &["byte[74] UTF-8", "<cidade>São Paulo</cidade>"],
    );
    // A deep expansion must treat a byte[] as a leaf too: expanded elementwise, one payload would be
    // seventy numbered lines where a caller asked to read an envelope.
    let expanded = server.call(
        "debug.evaluate",
        serde_json::json!({"expression": "entry#ISO-8859-1", "expand_objects": true}),
    );
    assert_contains_all(
        "an expanded object shows its byte[] fields as text, not as a numbered element block",
        &expanded,
        &["dsRequest = byte[73] ISO-8859-1", "<cidade>São Paulo</cidade>"],
    );

    server.panic_reset();
}

/// BP-5 (#79): one class name, two classloaders — arm both copies.
///
/// `classes_by_signature` returns one entry per classloader that has loaded a name, and the arming path
/// took `.first()`. So a stop point on a class that two deployments each pack into their own
/// `WEB-INF/lib` reported "armed" and then watched the copy the request never runs through. Silence that
/// is indistinguishable from a wrong hypothesis about the code path.
///
/// The probe defines the same class through two parent-less loaders, so the two copies are genuinely
/// different reference types with their own statics — `calls` counts independently in each, which is the
/// `Utils.tpAmbiente` shape that makes reading the wrong copy an actively wrong answer.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn an_exact_class_name_arms_every_classloaders_copy() {
    let Some(jdk) = jdk_or_skip("an_exact_class_name_arms_every_classloaders_copy") else { return };
    let mut probe = Probe::launch(&jdk, "TwinLoaderProbe").expect("launch TwinLoaderProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // The probe's own sanity line first. Without it a green test could mean the JVM collapsed the two
    // loaders into one type, in which case there is no multiplicity to arm and nothing was proven.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("loaded twice=true"))
        .expect("probe did not load the class twice — nothing for this test to assert about");
    // Both copies must have RUN before arming: a class the JVM has not linked yet is not in
    // classes_by_signature, so arming early would legitimately find one copy.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("ran beta:"))
        .expect("probe never exercised the second copy");

    let src = probe_source("TwinLoaderProbe");
    let line = probe_line(&src, "// BP1");

    let set = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "TwinLoaderProbe$Widget", "line": line,
            "trace": true, "trace_expr": "this.owner",
        }),
    );
    assert_contains_all(
        "an exact class name reports that it armed on more than one classloader",
        &set,
        &["bp_", "Armed on 2 classloaders"],
    );

    // Both copies have to report. Pre-fix only one does, and which one is up to the JVM's ordering.
    // One wait per copy, for the same reason the BP-4 test does: a single wait returns on whichever
    // landed first and reads the buffer before the other one arrives.
    server
        .wait_for_traces("this.owner => \"alpha\"", EVENT_TIMEOUT)
        .expect("no traced hit from the first classloader's copy");
    server
        .wait_for_traces("this.owner => \"beta\"", EVENT_TIMEOUT)
        .expect("no traced hit from the second classloader's copy — this is BP-5");
    let traces = server.call("debug.get_traces", serde_json::json!({}));
    let rows: Vec<&str> = traces.lines().filter(|l| l.contains("[bp_1]")).collect();
    assert!(
        rows.iter().any(|l| l.contains("\"alpha\"")),
        "no traced hit from the first classloader's copy: {rows:?}"
    );
    assert!(
        rows.iter().any(|l| l.contains("\"beta\"")),
        "no traced hit from the second classloader's copy — this is BP-5: the stop point armed one \
         deployment's copy and reported armed: {rows:?}"
    );

    // Still one stop point, per ADR-0005, and the listing has to make the copies distinguishable —
    // otherwise "armed on 2 classloaders" is a fact a caller can read and not act on.
    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_eq!(
        listed.matches("bp_1").count(),
        1,
        "two classloaders' copies must be listed as one stop point: {listed}"
    );
    assert_contains_all(
        "the listing names the loaders and how to select one",
        &listed,
        &["Armed on 2 classloaders", "#0 ", "#1 ", "TwinLoader"],
    );

    // A read that had to choose says so, and names the copies it did not read.
    let fields =
        server.call("debug.list_fields", serde_json::json!({"class_name": "TwinLoaderProbe$Widget"}));
    assert_contains_all(
        "a read path that had to choose between copies says so",
        &fields,
        &["calls", "is loaded by 2 classloaders", "Target a specific one with"],
    );

    // And the caller can pin it. The selector is the 0x… the note just printed.
    let after = listed.split("#1 ").nth(1).expect("no second loader in the listing");
    let at = after.find("@0x").expect("the listing must print the loader's objectID") + 1;
    let hex_len = after[at + 2..].find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(after.len() - at - 2);
    let loader = after[at..at + 2 + hex_len].to_string();
    let pinned = server.call(
        "debug.list_fields",
        serde_json::json!({"class_name": format!("TwinLoaderProbe$Widget@{loader}")}),
    );
    assert!(
        pinned.contains("calls") && !pinned.contains("is loaded by 2 classloaders"),
        "pinning a read to one loader must resolve unambiguously and drop the caveat: {pinned}"
    );

    // A loader id that is not one of them is an error, never a quiet fall back to the first copy.
    let wrong = server
        .call("debug.list_fields", serde_json::json!({"class_name": "TwinLoaderProbe$Widget@0xdeadbeef"}));
    assert!(
        wrong.contains("none by classloader"),
        "an unmatched loader selector must refuse rather than answer from another copy: {wrong}"
    );

    let cleared = server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": "bp_1"}));
    assert_contains_all("one clear removes both copies", &cleared, &["✅", "bp_1"]);
    let after = server.call("debug.list_stop_points", serde_json::json!({}));
    assert!(after.contains("No breakpoints set"), "clear left something armed: {after}");

    server.panic_reset();
}

/// BP-7 (#115): a stop point armed before a redeploy fires after it, with no re-arm.
///
/// The sequence this covers contains no re-arm, which is why BP-4's explicit re-resolve (#9) and BP-5's
/// arm-every-copy (#79) both miss it: `set_line_stop` → *the class loads again under a new classloader* →
/// the request that reaches the line. A deferred stop point held a `CLASS_PREPARE` watch and
/// `session.rs` cleared it the moment it armed, and a stop point that armed immediately never registered
/// one at all — so an exact name watched for its class exactly **once, ever**, and a redeploy is precisely
/// "this class loads again".
///
/// **The failure it replaces is silent**, which is what makes it expensive: the stop point stays listed,
/// stays enabled, and `get_traces` is empty — indistinguishable from the predicted code path not being the
/// one running, so the reader goes back to the source instead of re-arming.
///
/// The second copy loads on a **cue**, not a timer: the arming has to happen while only `v1` exists, and
/// a timer racing the arm would make a green run mean nothing.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_stop_point_armed_before_a_redeploy_arms_the_new_classloaders_copy_too() {
    let Some(jdk) = jdk_or_skip("a_stop_point_armed_before_a_redeploy_arms_the_new_classloaders_copy_too")
    else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "RedeployProbe").expect("launch RedeployProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    probe.wait_for_line(EVENT_TIMEOUT, |l| l == "deployed v1").expect("probe never deployed v1");
    probe.wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("ran v1:")).expect("v1's copy never ran");

    // Armed while exactly ONE copy is loaded. That is the premise: two copies at arm time is #79's case
    // and would arm both without any of this.
    let line = probe_line(&probe_source("RedeployProbe"), "// BP1");
    let set = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "RedeployProbe$Widget", "line": line,
            "trace": true, "trace_expr": "this.owner",
        }),
    );
    assert!(set.contains("bp_"), "the stop point did not arm on the only loaded copy: {set}");
    assert!(
        !set.contains("Armed on 2 classloaders"),
        "the second copy is not supposed to exist yet — this test's premise is gone: {set}"
    );
    server
        .wait_for_traces("this.owner => \"v1\"", EVENT_TIMEOUT)
        .expect("no traced hit from the copy that WAS loaded when the stop point armed");

    // The redeploy. Nothing is re-armed, on purpose — that is the whole issue.
    probe.send_line("redeploy").expect("send the redeploy cue");
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l == "second copy is a different type=true")
        .expect("the second load collapsed into the first type — nothing for this test to assert about");
    probe.wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("ran v2:")).expect("v2's copy never ran");

    // The assertion. Pre-fix this is where it hangs out the full EVENT_TIMEOUT and reports nothing,
    // which is exactly how the bug presents in a real session.
    server.wait_for_traces("this.owner => \"v2\"", EVENT_TIMEOUT).unwrap_or_else(|| {
        panic!(
            "the copy loaded by the SECOND classloader never reported, so the stop point is still \
             watching only the retired deployment's copy — this is BP-7, and note that nothing about the \
             listing says so on its own.\n  stop points: {}",
            server.call("debug.list_stop_points", serde_json::json!({}))
        )
    });

    // Still ONE stop point (ADR-0005), and the listing has to separate what is armed NOW from what will
    // be armed later — "Armed on 2 classloaders" alone cannot tell a redeploy from a library packed into
    // two wars, and only one of those means a copy may have arrived since you last looked.
    let listed = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_eq!(
        listed.matches("bp_1").count(),
        1,
        "a copy armed by the class-load watch must join the existing stop point, not become a second: \
         {listed}"
    );
    assert_contains_all(
        "the listing separates the copies armed now from the standing watch",
        &listed,
        &["Armed on 2 classloaders", "DeployLoader", "loaded SINCE it was armed", "Watching for more"],
    );

    // And the watch is not left behind. A watch outliving its stop point would go on arming copies for an
    // id the caller has been told is gone — FILT-3's mistake in a new place.
    let cleared = server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": "bp_1"}));
    assert_contains_all("one clear removes the stop point and its watch", &cleared, &["✅", "bp_1"]);
    let after = server.call("debug.list_stop_points", serde_json::json!({}));
    assert!(after.contains("No breakpoints set"), "clear left something armed: {after}");

    server.panic_reset();
}

/// EVAL-13 (#116): a member that exists on only ONE copy of a twice-loaded class name resolves, and the
/// reply says which copy answered.
///
/// The failure this replaces is loud but misdirecting. `resolve_class_by_dotted` took the first copy,
/// `find_method_for_args` came back empty, and the error said the class "has no static method … accepting
/// 4 argument(s) of these types" — so the reader goes and re-checks their arity and their argument types,
/// both of which are fine. It is not an argument-type problem and cannot be one: `score_param` matches by
/// JNI signature string, which is identical for the same FQN under every loader, so two copies can never
/// make an argument unassignable. The only way multiple copies produce that error is the one here — the
/// chosen copy genuinely lacks the member.
///
/// **Both directions are asserted because `classes_by_signature` promises no order.** `markerAAA` exists
/// only on the unpatched copy and `markerBBB` only on the patched one, so whichever the resolver reaches
/// first, exactly one of the two reads has to survive by trying the other copy — and exactly one of the two
/// replies carries the caveat. Asserting "the caveat appeared" against a single read would pass for the
/// wrong reason half the time and be a coin flip the rest.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_member_on_only_one_classloaders_copy_resolves_and_the_reply_names_the_copy() {
    let Some(jdk) =
        jdk_or_skip("a_member_on_only_one_classloaders_copy_resolves_and_the_reply_names_the_copy")
    else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "TwinMemberProbe").expect("launch TwinMemberProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // The probe's own three premises, stated by the JVM rather than assumed by the test: the name loaded
    // twice, and each copy is missing the other's member. Without these a green run could mean the loaders
    // collapsed into one type, in which case there is no multiplicity and nothing was proven.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("loaded twice=true"))
        .expect("probe did not load the class twice — nothing for this test to assert about");
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l == "alpha lacks senseBBB=true")
        .expect("the unpatched copy has the patched copy's member — the byte surgery did not take");
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l == "beta lacks senseAAA=true")
        .expect("the patched copy still has the original member — the byte surgery did not take");
    // Both copies must have RUN before reading: a class the JVM has not linked yet is not in
    // classes_by_signature, so reading early would legitimately find one copy.
    probe.wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("ran beta:")).expect("second copy never ran");

    // Static FIELDS first, because they need no suspended thread at all — so this half of the assertion
    // costs the debuggee nothing and would hold on the shared 8180.
    let aaa = server.evaluate("TwinMemberProbe$Widget.markerAAA");
    let bbb = server.evaluate("TwinMemberProbe$Widget.markerBBB");
    assert_contains_all("the member on the unpatched copy resolves", &aaa, &["AAA-value"]);
    assert_contains_all("the member on the patched copy resolves", &bbb, &["BBB-value"]);

    let caveat = "is loaded by 2 classloaders and";
    let carried: Vec<&str> = [("markerAAA", &aaa), ("markerBBB", &bbb)]
        .iter()
        .filter(|(_, r)| r.contains(caveat))
        .map(|(n, _)| *n)
        .collect();
    assert_eq!(
        carried.len(),
        1,
        "exactly one of the two reads must have been answered by a copy that was not tried first, and say \
         so — one is on each copy, so the ordering decides which. Got {carried:?}.\n  markerAAA: {aaa}\n  \
         markerBBB: {bbb}"
    );
    let noted = if carried[0] == "markerAAA" { &aaa } else { &bbb };
    assert_contains_all(
        "the caveat names the copy that answered and how to pin one",
        noted,
        &["TwinLoader", "Pin a specific copy with"],
    );

    // A genuine typo still fails — and the message says how many copies were searched, which is what turns
    // "your signature is wrong" from a guess into a statement. Absent from EVERY copy rules the stale-copy
    // reading OUT, and that is worth more to the reader than the arity sentence it replaces.
    let typo = server.evaluate("TwinMemberProbe$Widget.markerCCC");
    assert_contains_all(
        "a member on no copy at all says how many were searched",
        &typo,
        &["is loaded 2 times", "NONE of the 2 copies", "All 2 were searched"],
    );

    // And the invoke path, which is where #116 was actually seen. It needs a suspended thread, so this
    // half costs a breakpoint.
    let line = probe_line(&probe_source("TwinMemberProbe"), "// BP1");
    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "TwinMemberProbe$Widget", "line": line}),
    );
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in TwinMemberProbe$Widget.work never fired");
    let call_aaa = server.evaluate("TwinMemberProbe$Widget.senseAAA()");
    let call_bbb = server.evaluate("TwinMemberProbe$Widget.senseBBB()");
    assert_contains_all("the static call on the unpatched copy resolves", &call_aaa, &["sense AAA-value"]);
    assert_contains_all("the static call on the patched copy resolves", &call_bbb, &["sense BBB-value"]);

    server.panic_reset();
}

/// EVAL-7 (#81), the half that decides whether any of it is usable on the shared 8180: a decoded
/// `byte[]` and an `array.length` both have to be reachable from a `trace_expr`, which is the only way
/// to read a value on an instance other people are using without freezing it.
///
/// Two stop points rather than two assertions on one, because a `trace_expr` is one expression per stop
/// point — and each is waited for by its OWN needle, since `wait_for_traces` returns on the FIRST
/// matching record and a single wait would read the buffer between the two.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_trace_expr_decodes_a_byte_array_and_reads_its_length_without_suspending() {
    let Some(jdk) = jdk_or_skip("a_trace_expr_decodes_a_byte_array_and_reads_its_length_without_suspending")
    else {
        return;
    };
    let mut probe =
        Probe::launch_running(&jdk, "BytesProbe", |l| l.starts_with("tick ")).expect("launch BytesProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
    let base = highest_tick(&probe).expect("no tick to count from");

    let source = probe_source("BytesProbe");
    // One expression per stop point, so the two questions are two stop points — on two adjacent lines
    // of the same method, which the probe carries markers for.
    for (marker, expr) in [("// BP1", "entry.dsRequest#ISO-8859-1"), ("// BP2", "req.length")] {
        let armed = server.call(
            "debug.set_line_stop",
            serde_json::json!({
                "class_pattern": "BytesProbe",
                "line": probe_line(&source, marker),
                "trace": true,
                "trace_expr": expr,
            }),
        );
        assert_contains_all("armed as a trace", &armed, &["bp_", "trace (non-suspending)"]);
    }

    // The whole point of trace mode: the debuggee never stops. A suspending breakpoint here would hold
    // the JVM on every call and the ticks would end.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2)).is_some(),
        "the probe stopped ticking, so a traced hit left it suspended\n  output: {:?}",
        probe.output(),
    );

    let decoded = server
        .wait_for_traces("entry.dsRequest#ISO-8859-1 =>", EVENT_TIMEOUT)
        .expect("no trace record carried the decoded payload");
    assert_contains_all(
        "the trace record carries the decoded envelope, on one line",
        &decoded,
        &["entry.dsRequest#ISO-8859-1 => byte[73] ISO-8859-1", "<cidade>São Paulo</cidade>"],
    );

    let length = server
        .wait_for_traces("req.length =>", EVENT_TIMEOUT)
        .expect("no trace record carried the array length");
    assert_contains_all(
        "and `.length` answers inside a trace_expr, where there is no schema to extend",
        &length,
        &["req.length => (int) 9"],
    );

    server.panic_reset();
}

/// The `<n>` of `HeapProbe`'s `tick … gap=<n>ms` line — the probe's own measurement of how long it was
/// held. A tick is the only evidence an application thread is running, so the pause a stop-the-world
/// heap walk imposes shows up here and nowhere else; the debugger reports success either way.
fn tick_gap_ms(line: &str) -> Option<i64> {
    let at = line.find("gap=")? + "gap=".len();
    line.get(at..)?.strip_suffix("ms")?.parse().ok()
}

/// The largest gap the probe has printed since `from` lines of output.
fn worst_gap_since(probe: &Probe, from: usize) -> i64 {
    probe.output().iter().skip(from).filter_map(|l| tick_gap_ms(l)).max().unwrap_or(0)
}

/// DISC-10: the heap query ships, and it reports what it cost — measured on both sides.
///
/// Four things are being pinned here, and the third and fourth are the ones the maintainer's decision
/// on #84 turns on. That `Instances` is **exact type** — `Widget` answers 7 with two live `SubWidget`s
/// in the heap, which is `CONTEXT.md`'s `Loaded` trap in a new costume and would otherwise be
/// discovered. That the handles it returns are **usable**, which is the whole reason #84 was blocked on
/// #85. That the reply states its own **held duration** rather than refusing or demanding an
/// acknowledgement (ADR-0010's precedent, ADR-0023's decision). And that the duration is real: the
/// probe's own tick gaps have to widen while the walks run, because the debugger reports success either
/// way and its own number would otherwise be unfalsifiable.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
#[allow(clippy::too_many_lines)] // one linear script against one shaped heap; splitting it would mean
                                 // allocating the ballast twice for halves that only mean anything together
fn a_heap_query_answers_by_exact_type_and_reports_the_pause_it_imposed() {
    let Some(jdk) = jdk_or_skip("a_heap_query_answers_by_exact_type_and_reports_the_pause_it_imposed") else {
        return;
    };
    // `launch_running`: the ballast takes a moment to allocate and nothing is asked about a class that
    // has not been instantiated yet.
    let mut probe = Probe::launch_running(&jdk, "HeapProbe", |l| l.starts_with("ready")).expect("launch");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // A quiet stretch first, so the pause below is measured against this probe on this box rather than
    // against the 50ms the probe intends to sleep for.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let quiet_from = probe.output().len();
    std::thread::sleep(std::time::Duration::from_secs(2));
    let quiet_worst = worst_gap_since(&probe, quiet_from);
    let busy_from = probe.output().len();

    let types = serde_json::json!(["HeapProbe$Widget", "HeapProbe$SubWidget", "HeapProbe$Nothing"]);

    // 1. Exact-type counts, in one call, over one batch.
    let listed =
        server.call("debug.list_instances", serde_json::json!({"class_names": types, "max_instances": 3}));
    assert_contains_all(
        "exact-type counts, with the semantic stated rather than left to be discovered",
        &listed,
        &[
            "HeapProbe$Widget — 7 live instance(s)",
            "HeapProbe$SubWidget — 2 live instance(s)",
            "HeapProbe$Nothing — 0 live instance(s)",
            "EXACT TYPE, NOT SUBTYPE-INCLUSIVE",
            "HELD APPLICATION THREADS FOR ~",
        ],
    );
    // 7, not 9: the two SubWidgets are live and are NOT counted as Widgets. And not 10 either — the
    // three unreachable Widgets are never reported.
    assert!(!listed.contains("Widget — 9 live"), "Instances is exact-type, not subtype-inclusive: {listed}");
    // The clamp bounds the handles and not the count, so a clamped listing still says how many exist.
    assert_contains_all(
        "max_instances clamps what is shown, never what is reported",
        &listed,
        &["showing 3", "… +4 more"],
    );

    // 2. The handles work. This is the whole reason #84 was blocked on #85 — without it the tool
    //    returns identifiers nothing can dereference.
    let handle = listed
        .lines()
        .filter(|l| l.starts_with("  ") && l.contains("HeapProbe$Widget"))
        .find_map(|l| l.split_whitespace().find(|w| w.starts_with("@0x")))
        .unwrap_or_else(|| panic!("no @0x… handle in:\n{listed}"))
        .to_string();
    let payload = server.evaluate(&format!("{handle}.payload"));
    assert!(
        payload.contains("widget-"),
        "a handle from list_instances must be an expression head that reaches that object: {payload}"
    );

    // 3. counts_only is one walk for the whole batch and returns no handles.
    let counted =
        server.call("debug.list_instances", serde_json::json!({"class_names": types, "counts_only": true}));
    assert_contains_all(
        "counts_only answers the cheap half in a single walk",
        &counted,
        &["1 live-heap walk(s)", "HeapProbe$Widget — 7 live instance(s)"],
    );
    assert!(!counted.contains("@0x"), "counts_only returns no handles: {counted}");

    // 4. A negative clamp is refused without spending a walk to learn it.
    let bad = server.call(
        "debug.list_instances",
        serde_json::json!({"class_names": ["HeapProbe$Widget"], "max_instances": -1}),
    );
    assert_contains_all("a negative max_instances is refused here, not by the JVM", &bad, &["max_instances"]);
    assert!(!bad.contains("live-heap walk"), "nothing should have been walked: {bad}");

    // A name that does not resolve is reported beside the answers rather than failing the call — the
    // same partial-success rule a batch of class patterns follows.
    let mixed = server.call(
        "debug.list_instances",
        serde_json::json!({"class_names": ["HeapProbe$Widget", "no.such.Type"], "counts_only": true}),
    );
    assert_contains_all(
        "an unresolvable name costs nothing and does not lose the others",
        &mixed,
        &["HeapProbe$Widget — 7 live instance(s)", "no.such.Type — not resolved"],
    );

    // 5. THE MEASUREMENT, from the debuggee. Nothing was suspended by the debugger, and the reply says
    //    it was still held — so the probe's ticks must show it. This is the only unfalsifiable half.
    let busy_worst = worst_gap_since(&probe, busy_from);
    // Printed rather than only asserted: the two numbers ARE the finding, and a run log that names them
    // is what lets someone reproduce this on another box and another heap size.
    println!("HeapProbe tick gaps: worst quiet={quiet_worst}ms, worst during the heap walks={busy_worst}ms");
    println!("debugger's own figure for one walk: {}", counted.lines().next().unwrap_or("(missing)"));
    assert!(
        busy_worst > quiet_worst,
        "a stop-the-world walk must show up as a widened tick gap — quiet {quiet_worst}ms vs busy \
         {busy_worst}ms\n  output: {:?}",
        probe.output(),
    );
    // And the probe is still running: the walk holds threads, it does not leave them suspended.
    let after = probe.output().len();
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_gap_ms(l).is_some()).is_some() || after > busy_from,
        "the probe must still be ticking after the walks\n  output: {:?}",
        probe.output(),
    );

    server.panic_reset();
}

/// Pull the `@0x…` handle that follows `<name>=` on one `debug.get_traces` line (TRACE-10).
///
/// Reads the handle the reply printed rather than reconstructing one, which is the point of the two
/// tests below: a handle is only useful if the exact text a snapshot shows can be pasted back in.
fn traced_handle(line: &str, name: &str) -> Option<String> {
    let after = line.split_once(&format!("{name}="))?.1;
    let at = after.find("@0x")?;
    let rest = &after[at..];
    let end = rest[3..].find(|c: char| !c.is_ascii_hexdigit()).map_or(rest.len(), |i| i + 3);
    Some(rest[..end].to_string())
}

/// TRACE-10 half two: a snapshot inside an ANONYMOUS class shows the enclosing method's captures.
///
/// The captures are synthetic `val$…` fields plus `this$0`, and none of them are in `call()`'s local
/// variable table — so before this the whole causal chain across the thread boundary was invisible and
/// the snapshot showed a single `this`. `plain()` is traced in the same run as the control, because "no
/// captured section here" has to be measured against a live ordinary site rather than against silence.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_snapshot_inside_an_anonymous_class_shows_the_enclosing_captures() {
    let Some(jdk) = jdk_or_skip("a_snapshot_inside_an_anonymous_class_shows_the_enclosing_captures") else {
        return;
    };
    // `launch_running`, not `launch`: the anonymous class does not exist until the warmup task has run
    // it, and arming before that would legitimately DEFER and prove nothing (TEST-17, #49).
    let mut probe = Probe::launch_running(&jdk, "CapturedProbe", |l| l.starts_with("ready")).expect("launch");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let base = highest_tick(&probe).unwrap_or(-1);
    let src = probe_source("CapturedProbe");

    let anon = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "CapturedProbe$1", "line": probe_line(&src, "// BP1"), "trace": true,
        }),
    );
    assert!(anon.contains("bp_"), "the anonymous class's call() must arm: {anon}");
    let plain = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "CapturedProbe", "line": probe_line(&src, "// BP2"), "trace": true,
        }),
    );
    assert!(plain.contains("bp_"), "the control site must arm: {plain}");

    // Reading four extra fields per hit must still leave nothing suspended, and only the probe's own
    // output can say so.
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2)).is_some(),
        "probe stopped ticking after the captured-field read — a hit left it suspended\n  output: {:?}",
        probe.output(),
    );

    let traces = server
        .wait_for_traces("supplier-A", EVENT_TIMEOUT)
        .expect("no traced hit inside the anonymous class");
    let hit = traces
        .lines()
        .find(|l| l.contains("supplier-A"))
        .unwrap_or_else(|| panic!("no supplier-A trace line in:\n{traces}"));

    // Both captured locals BY NAME, plus the enclosing instance — the acceptance criterion.
    assert_contains_all(
        "the enclosing method's captures are on the snapshot, by name",
        hit,
        &[" captured{", "val$supplier=\"supplier-A\"", "val$attempt=(int) ", "val$request=", "this$0="],
    );

    // The control. `plain()` is an ordinary static method on an ordinary class, hit in the same run.
    let plain_hit =
        server.wait_for_traces("CapturedProbe.plain:", EVENT_TIMEOUT).expect("the control site never fired");
    for line in plain_hit.lines().filter(|l| l.contains("CapturedProbe.plain:")) {
        assert!(!line.contains("captured{"), "an ordinary class has no captures to show: {line}");
    }

    // Side-effect free, measured rather than asserted: `Request.toString()` counts its own calls, and
    // rendering four fields (one of them a Request) must not have run a line of debuggee code.
    let calls = server.evaluate("CapturedProbe.toStringCalls");
    assert!(
        calls.contains("(int) 0"),
        "capturing the enclosing values must invoke nothing in the debuggee: {calls}"
    );

    server.panic_reset();
}

/// TRACE-10 half one: a handle a snapshot kept dereferences later, and says **vanished** when it cannot.
///
/// Three readings, all from the same run: a handle whose object is pinned in a `static final` reads a
/// field long after the hit; an id this JVM never issued is refused as vanished rather than as a raw
/// JDWP error; and an object deliberately dropped and collected mid-session becomes the same reading —
/// which is the one that matters, because on a pool that retires workers it is the ordinary case.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn an_object_handle_outlives_its_snapshot_and_reports_when_it_has_not() {
    let Some(jdk) = jdk_or_skip("an_object_handle_outlives_its_snapshot_and_reports_when_it_has_not") else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "CapturedProbe", |l| l.starts_with("ready")).expect("launch");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let src = probe_source("CapturedProbe");
    server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "CapturedProbe$1", "line": probe_line(&src, "// BP1"), "trace": true,
        }),
    );

    // ONE wait, for the LATER of the two hits. `wait_for_traces` returns on the first matching record,
    // so waiting separately for each would read the buffer between them; the probe submits supplier-A
    // before supplier-B in every iteration, so a buffer containing B already contains an A.
    let traces =
        server.wait_for_traces("supplier-B", EVENT_TIMEOUT).expect("the doomed request was never traced");
    let pinned_line = traces
        .lines()
        .find(|l| l.contains("supplier-A"))
        .unwrap_or_else(|| panic!("no supplier-A line in:\n{traces}"));
    let doomed_line = traces
        .lines()
        .find(|l| l.contains("supplier-B"))
        .unwrap_or_else(|| panic!("no supplier-B line in:\n{traces}"));

    let pinned = traced_handle(pinned_line, "val$request")
        .unwrap_or_else(|| panic!("no handle beside val$request in: {pinned_line}"));
    let doomed = traced_handle(doomed_line, "val$request")
        .unwrap_or_else(|| panic!("no handle beside val$request in: {doomed_line}"));
    assert_ne!(pinned, doomed, "the two requests are different objects: {traces}");

    // 1. The handle reads a field of the same object, with nothing suspended and long after the hit.
    let read = server.evaluate(&format!("{pinned}.id"));
    assert!(read.contains("pinned-req"), "a retained handle must still read its object's field: {read}");
    let doomed_read = server.evaluate(&format!("{doomed}.id"));
    assert!(doomed_read.contains("doomed-req"), "the second handle names the other object: {doomed_read}");

    // 2. An id this JVM never issued. Deterministic, and it exercises the same reading the collected
    //    case reaches by the other route — the JVM answering INVALID_OBJECT rather than IsCollected.
    let never = server.evaluate("@0xdeadbeefdeadbeef.id");
    assert_contains_all(
        "an id the JVM has no record of is a vanished reading, not a raw JDWP error",
        &never,
        &["Vanished", "WEAK reference"],
    );

    // 3. Forced collection: the acceptance criterion says to test this deliberately rather than to wait
    //    for a pool to do it. The probe drops its last strong reference and runs two collections.
    probe.send_line("drop").expect("send drop cue");
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("dropped"))
        .unwrap_or_else(|| panic!("the probe never dropped the request\n  output: {:?}", probe.output()));

    let deadline = std::time::Instant::now() + EVENT_TIMEOUT;
    let mut last;
    loop {
        last = server.evaluate(&format!("{doomed}.id"));
        if last.contains("Vanished") || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    assert_contains_all(
        "a collected object's handle reads as vanished, in the debugger's own words",
        &last,
        &["Vanished", &doomed],
    );
    // The pinned one is unaffected: this is a fact about one object, not the handle mechanism failing.
    let still = server.evaluate(&format!("{pinned}.id"));
    assert!(still.contains("pinned-req"), "a live object's handle must survive the other's death: {still}");

    server.panic_reset();
}

/// Pull the `@0x…` handle for one named `InstProbe` instance out of a `debug.list_instances` reply.
///
/// By identity rather than by position: the two ids come back in whichever order the heap walk found
/// them, and which one is `X` differs between JDKs — measured, 17 answers `@0x3` and 21 answers `@0x4`
/// for the same source. A test that indexed the list would pass on one and fail on the other.
fn inst_probe_handle(server: &mut Server, listed: &str, want: &str) -> String {
    let handles: Vec<String> = listed
        .lines()
        .filter(|l| l.starts_with("  ") && l.contains("InstProbe @0x"))
        .filter_map(|l| l.split_whitespace().find(|w| w.starts_with("@0x")))
        .map(str::to_string)
        .collect();
    assert!(handles.len() >= 2, "expected two live InstProbe instances, got {handles:?} in:\n{listed}");
    for h in &handles {
        if server.evaluate(&format!("{h}.name")).contains(&format!("\"{want}\"")) {
            return h.clone();
        }
    }
    panic!("no InstProbe instance named {want} among {handles:?}");
}

/// FILT-9 (#101): a stop point can be scoped to ONE object, and every shape where that silently would
/// not work is refused rather than armed.
///
/// The refusals are the substance here, not the feature. `InstanceOnly` is a **filter** the debuggee
/// applies (`CONTEXT.md`), and `HotSpot` accepts the modifier on three shapes where it then ignores it —
/// no error, no warning, and a reply saying the stop point is scoped when it fires for every instance.
/// `CONTEXT.md` calls that state **inert**, and the rule it yields is *acceptance is not application*.
/// So this test asserts the negative space: what the tool refuses, and that it names why.
///
/// The positive half is measured against a **twin**. Both instances do the same work in the same loop,
/// so "the filter worked" can only be established by the twin's records being absent — not by the
/// filtered instance's being present, which an unfiltered stop point would also produce. That is what
/// caught the method-exit case: it looked correct until someone asked what the other object was doing.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
#[allow(clippy::too_many_lines)]
fn a_stop_point_scoped_to_one_object_ignores_its_twin_and_refuses_where_it_could_not() {
    let Some(jdk) =
        jdk_or_skip("a_stop_point_scoped_to_one_object_ignores_its_twin_and_refuses_where_it_could_not")
    else {
        return;
    };
    let mut probe =
        Probe::launch_running(&jdk, "InstProbe", |l| l.starts_with("ready")).expect("launch InstProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // `Boom` has to be loaded before an exception request can pin a ref type to it, and both instances
    // have to have run for the twin comparison to mean anything.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("work Y 2"))
        .unwrap_or_else(|| panic!("the probe never worked twice\n  output: {:?}", probe.output()));

    let listed = server
        .call("debug.list_instances", serde_json::json!({"class_names": ["InstProbe"], "max_instances": 5}));
    let x = inst_probe_handle(&mut server, &listed, "X");
    let y = inst_probe_handle(&mut server, &listed, "Y");
    assert_ne!(x, y, "the two instances must be distinct objects");

    // 1. A line stop in an INSTANCE method, scoped to X. The reply says so, and the records agree.
    let set = server.call(
        "debug.set_line_stop",
        serde_json::json!({
            "class_pattern": "InstProbe", "method": "work", "trace": true, "instance_id": x,
        }),
    );
    assert_contains_all(
        "a scoped line stop says what it is scoped to",
        &set,
        &[&format!("Instance filter: {x}")],
    );

    // 2. An EXCEPTION stop, scoped to X. This is the combination FILT-9 stopped on: HotSpot accepts the
    //    modifier on every kind, and this is the only one besides an instance line stop and an instance
    //    field watch where it is actually applied. Measured on Temurin 17/21/25 before it was allowed.
    let exc = server.call(
        "debug.set_exception_stop",
        serde_json::json!({
            "class_pattern": "InstProbe$Boom", "trace": true, "instance_id": x,
        }),
    );
    assert_contains_all(
        "a scoped exception stop says what it is scoped to",
        &exc,
        &[&format!("Instance filter: {x}")],
    );

    // 3. A field watch on an INSTANCE field, scoped to X.
    let watch = server.call(
        "debug.set_field_stop",
        serde_json::json!({
            "class_name": "InstProbe", "field_name": "touched", "trace": true, "instance_id": x,
        }),
    );
    assert_contains_all(
        "a scoped field watch says what it is scoped to",
        &watch,
        &[&format!("Instance filter: {x}")],
    );

    // Let all three collect a useful number of hits. Y is doing the identical work throughout.
    let base = probe.output().len();
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("alive "))
        .expect("probe stopped ticking under three traced stop points");
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert!(probe.output().len() > base, "the probe must keep running — every stop point here is traced");

    let traces = server.call("debug.get_traces", serde_json::json!({}));
    let records: Vec<&str> = traces.lines().filter(|l| l.starts_with('#')).collect();
    assert!(
        records.len() >= 3,
        "expected records from all three stop points, got {}:\n{traces}",
        records.len()
    );

    // THE assertion. Not "X appears" — an unfiltered stop point would give that too — but "Y never
    // does", across every kind at once.
    // By whole token, not substring: `@0x3` is a prefix of `@0x30`, and the `Boom` object a record
    // carries landed there — which made a correctly-filtered run report itself as a leak. The first
    // version of this assertion was wrong in the direction that fails loudly, which is the good
    // direction, but a `contains` over hex handles is a trap worth naming rather than just fixing.
    let leaked: Vec<&&str> = records
        .iter()
        .filter(|l| l.split(|c: char| !(c.is_ascii_alphanumeric() || c == 'x' || c == '@')).any(|w| w == y))
        .collect();
    assert!(
        leaked.is_empty(),
        "a stop point scoped to {x} recorded the twin {y} — the filter was accepted and NOT applied, \
         which is the inert case this feature exists to refuse.\n  {} leaked record(s):\n{}",
        leaked.len(),
        leaked.iter().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n"),
    );
    assert!(
        records.iter().any(|l| l.contains(&x)),
        "no record names the filtered instance {x} at all, so the run proves nothing:\n{traces}"
    );

    // 4. The listing carries the filter, so a later reader can tell why a quiet stop point is quiet.
    let listing = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_contains_all("the listing repeats the scope", &listing, &[&format!("Instance filter: {x}")]);

    // 5. The refusals, each naming the JDWP fact rather than the argument. A static method and a static
    //    field have no `this`; a method exit HAS one, which is what makes its silence the worst of the
    //    three and why the refusal has to be explicit rather than left to look like an oversight.
    let static_line = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "InstProbe", "method": "stat", "trace": true, "instance_id": x}),
    );
    assert_contains_all(
        "a static method is refused, with the reason",
        &static_line,
        &["a static method has no `this`", "ACCEPTS an InstanceOnly modifier here"],
    );

    let static_field = server.call(
        "debug.set_field_stop",
        serde_json::json!({"class_name": "InstProbe", "field_name": "statics", "trace": true, "instance_id": x}),
    );
    assert_contains_all(
        "a static field is refused, with the reason",
        &static_field,
        &["a static field has no `this`", "ACCEPTS an InstanceOnly modifier here"],
    );

    let mexit = server.call(
        "debug.set_method_exit_stop",
        serde_json::json!({"class_pattern": "InstProbe", "method": "work", "instance_id": x}),
    );
    assert_contains_all(
        "method exit is refused outright, and says the reply would have looked correct",
        &mexit,
        &["not supported on debug.set_method_exit_stop", "records both instances"],
    );

    let deferred = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "NoSuchClassHere", "line": 1, "instance_id": x}),
    );
    assert_contains_all(
        "an unfetched class is refused, because no instance of it can exist",
        &deferred,
        &["not loaded yet", "has none"],
    );

    // Nothing above armed anything: four refusals, and the three stop points from before.
    let after = server.call("debug.list_stop_points", serde_json::json!({}));
    assert!(
        !after.contains("InstProbe.stat") && !after.contains("NoSuchClassHere"),
        "a refusal must not leave a stop point behind:\n{after}"
    );

    server.panic_reset();
}

/// FILT-9 (#101): an armed `InstanceOnly` filter PINS its object, and the listing says when it has gone.
///
/// Two facts in one run, and the first is the surprise. `CONTEXT.md` warns that a filter naming a
/// collected object "simply stops matching, which reads as *the code never ran*" — true of a thread
/// filter, and **false of this one while it is armed**, because `HotSpot` holds the object the modifier
/// names. Measured five ways on Temurin 17/21/25 (ADR-0027): nothing armed, an unfiltered breakpoint on
/// the same method, a filtered one, filtered-then-disabled and filtered-then-cleared. Only the filtered
/// arm survives the drop, so the modifier is the strong reference and clearing or disabling releases it.
///
/// That inverts what needs asserting. While armed there is no silent-quiet hazard at all — the debuggee
/// cannot collect what it is holding — but there IS a retention the caller is paying for on a shared JVM,
/// so the arm reply has to state it. The hazard moves to the **disable → re-arm** cycle, which is the
/// whole point of `toggle_stop_point` and which this test therefore drives end to end.
///
/// Both halves are asserted against the *same* object in the *same* run, because either alone is
/// ambiguous: "still live while armed" could just be a slow collector, and "collected after disable"
/// could just be a drop that finally took. Together they are the pin.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn an_armed_instance_filter_pins_its_object_and_reports_it_once_that_is_released() {
    let Some(jdk) =
        jdk_or_skip("an_armed_instance_filter_pins_its_object_and_reports_it_once_that_is_released")
    else {
        return;
    };
    let mut probe =
        Probe::launch_running(&jdk, "InstProbe", |l| l.starts_with("ready")).expect("launch InstProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("work Y 2"))
        .unwrap_or_else(|| panic!("the probe never worked twice\n  output: {:?}", probe.output()));

    let listed = server
        .call("debug.list_instances", serde_json::json!({"class_names": ["InstProbe"], "max_instances": 5}));
    let y = inst_probe_handle(&mut server, &listed, "Y");

    let set = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "InstProbe", "method": "work", "trace": true, "instance_id": y}),
    );
    let bp = set
        .lines()
        .find_map(|l| l.split_whitespace().find(|w| w.starts_with("bp_")))
        .unwrap_or_else(|| panic!("no bp_ id in:\n{set}"))
        .to_string();
    // The cost is stated where it is incurred, not only in the ADR.
    assert_contains_all(
        "the arm reply says the filter pins the object",
        &set,
        &["PINS the object", "until you clear or disable it"],
    );

    // 1. The pin. The probe drops its last reference to Y and runs two collections; the object survives
    //    them, because the armed modifier is holding it.
    probe.send_line("drop").expect("send drop cue");
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("dropped"))
        .unwrap_or_else(|| panic!("the probe never dropped Y\n  output: {:?}", probe.output()));
    let still = server.evaluate(&format!("{y}.name"));
    assert!(
        still.contains("\"Y\""),
        "the probe dropped Y and collected twice, yet an ARMED instance filter must keep it alive — this \
         is the measured behaviour the arm reply promises the caller. Got: {still}"
    );
    let armed_listing = server.call("debug.list_stop_points", serde_json::json!({}));
    assert!(
        !armed_listing.contains("HAS VANISHED"),
        "nothing has vanished while the filter is armed:\n{armed_listing}"
    );

    // 2. Disabling releases the pin, and only then can the object go.
    server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": bp, "enabled": false}));
    // Ask for another collection, and this is load-bearing rather than belt-and-braces: the two the
    // probe ran on the `drop` cue happened while the modifier still pinned Y, and nothing collects an
    // unreachable object that nobody asks about. Skipping this would report "still pinned" for a JVM
    // that had simply not been asked again — a false negative shaped exactly like the finding.
    probe.send_line("gc").expect("send gc cue");
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("collected"))
        .unwrap_or_else(|| panic!("the probe never collected again\n  output: {:?}", probe.output()));
    let deadline = std::time::Instant::now() + EVENT_TIMEOUT;
    let mut listing;
    loop {
        listing = server.call("debug.list_stop_points", serde_json::json!({}));
        if listing.contains("HAS VANISHED") || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    assert_contains_all(
        "the listing says the filter can never match again, and why its silence is not `no hits`",
        &listing,
        &[
            &format!("FILTER OBJECT {y} HAS VANISHED"),
            "does NOT mean the code did not run",
            "stop point(s) above are scoped to an object the debuggee has since collected",
        ],
    );

    // The reader of an empty trace buffer is exactly who needs this, and gets it there too.
    assert_contains_all(
        "get_traces says it as well",
        &server.call("debug.get_traces", serde_json::json!({})),
        &["scoped to an object the debuggee has collected"],
    );

    // 3. And the re-arm is refused, rather than producing a stop point that lists as armed and never
    //    fires. This is the outcome the whole disable/re-arm path was at risk of.
    let rearm =
        server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": bp, "enabled": true}));
    assert_contains_all(
        "re-arming a filter whose object is gone is refused, with the remedy",
        &rearm,
        &["collected that object", "debug.list_instances"],
    );

    server.panic_reset();
}

/// EVAL-12 (#112): a ONE-SEGMENT name resolves against the frame's own class — local, then `this`,
/// then static — instead of failing between the local lookup and the dotted-static one.
///
/// The ordering is the substance, so all four shapes are driven in one run against a probe that puts a
/// static and an instance field of the same name in scope at the same point. Any of them measured alone
/// would pass against a resolver that had simply stopped trying at the first thing it found.
///
/// Driven through `trace_expr` rather than `debug.evaluate` because that is where the bug actually bites:
/// the failure is per-record text, not an arming error, so a broken resolver gives you an armed,
/// apparently-healthy logpoint whose every capture is an error string — and you learn that only when you
/// read the snapshots. The issue was found exactly that way.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_bare_name_resolves_against_the_frames_own_class_in_the_java_order() {
    let Some(jdk) = jdk_or_skip("a_bare_name_resolves_against_the_frames_own_class_in_the_java_order") else {
        return;
    };
    let mut probe =
        Probe::launch_running(&jdk, "BareNameProbe", |l| l.starts_with("ready")).expect("launch probe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("tick 2"))
        .unwrap_or_else(|| panic!("the probe never ticked twice\n  output: {:?}", probe.output()));

    let src = probe_source("BareNameProbe");
    // Five traced stop points, each carrying the one expression its site is about.
    let sites = [
        ("BP1", "BareNameProbe$Child", "shadowed", "a local wins over both fields"),
        ("BP2", "BareNameProbe$Child", "shadowed", "`this` field wins over the inherited static"),
        ("BP3", "BareNameProbe$Child", "inherited", "a static inherited from a superclass"),
        ("BP4", "BareNameProbe", "calls", "a static of the frame's own class, from a STATIC method"),
        // Its own line, not BP4's: two traced stop points on one line is #102 (BP-6), still open.
        ("BP5", "BareNameProbe", "nosuchthing", "a name that is genuinely nowhere"),
    ];
    for (marker, class, expr, what) in sites {
        let set = server.call(
            "debug.set_line_stop",
            serde_json::json!({
                "class_pattern": class, "line": probe_line(&src, &format!("// {marker}")),
                "trace": true, "trace_expr": expr,
            }),
        );
        assert!(set.contains("bp_"), "could not arm {what}: {set}");
    }

    // Let every site record. All five are traced, so the probe must keep running throughout.
    let base = probe.output().len();
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert!(probe.output().len() > base, "the probe stopped under five traced stop points");

    let traces = server.call("debug.get_traces", serde_json::json!({}));
    let value_of = |method: &str, expr: &str| -> String {
        traces
            .lines()
            .filter(|l| l.starts_with('#') && l.contains(method) && l.contains(&format!("{expr} =>")))
            .find_map(|l| l.split(&format!("{expr} =>")).nth(1).map(|t| t.trim().to_string()))
            .unwrap_or_else(|| panic!("no record for {method} carrying `{expr}`:\n{traces}"))
    };

    // 1. A local shadows everything, which is the existing behaviour and must not regress.
    let local = value_of("localWins", "shadowed");
    assert!(local.contains("30"), "a local must win over both fields — got `{local}`");

    // 2. No local: the instance field of `this`, NOT the static of the same name on the superclass.
    //    Both are in scope here, which is what makes this an ordering assertion rather than a lookup.
    let instance = value_of("instanceWins", "shadowed");
    assert!(
        instance.contains("20") && !instance.contains("10"),
        "with no local, `this.shadowed` (20) must win over the inherited static Base.shadowed (10) — \
         got `{instance}`"
    );

    // 3. A static declared on the SUPERCLASS, by its bare name. Java reaches it; so must this.
    let inherited = value_of("inheritedStatic", "inherited");
    assert!(inherited.contains('7'), "an inherited static must resolve by its bare name — got `{inherited}`");

    // 4. The issue's own case: a static of the frame's own class, read from a STATIC method — where
    //    there is no `this` to fall back on, so the static step is the only thing that can answer.
    let calls = value_of("tick", "calls");
    assert!(
        !calls.contains("<error"),
        "a static of the frame's own class must resolve from a static method, which is what the issue \
         was filed about — got `{calls}`"
    );
    assert!(calls.contains("(int)"), "the capture must be a value, not prose — got `{calls}`");

    // 5. A name that is nowhere. The message must name the class searched and the fix, rather than
    //    describing the resolver's arity requirement.
    let unknown = value_of("unknownSite", "nosuchthing");
    assert_contains_all(
        "an unresolvable bare name says where it looked and what to type instead",
        &unknown,
        &["BareNameProbe", "nosuchthing"],
    );
    assert!(
        !unknown.contains("needs at least Class.field"),
        "the failure should name the fix, not the resolver's arity rule — got `{unknown}`"
    );

    server.panic_reset();
}

/// BP-6 (#102): two traced stop points on the SAME line both record.
///
/// The brief asked which of three problems this was, and one probe run settled it. On Temurin 17/21/25,
/// three `BREAKPOINT` requests at one bytecode location get three **distinct** request ids, and every hit
/// arrives as **one composite carrying all three**. So `HotSpot` is not the constraint and the ids are not
/// colliding — the trace path was reading `events.first()` and dropping the rest.
///
/// Asserted against the probe's own stdout as well as the traces, because "it recorded" and "the line
/// ran" are the two readings that have to be told apart here: an empty buffer for the second stop point
/// looked exactly like the code never executing, which is why the bug survived being noticed once.
///
/// Two different `trace_expr` values, since that is the reason to want this at all — watching two
/// variables at one statement you cannot afford to suspend, which on the shared 8180 is most of them.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn two_traced_stop_points_on_one_line_both_record() {
    let Some(jdk) = jdk_or_skip("two_traced_stop_points_on_one_line_both_record") else { return };
    let mut probe =
        Probe::launch_running(&jdk, "BareNameProbe", |l| l.starts_with("ready")).expect("launch probe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
    probe.wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("tick 2")).expect("probe never ticked");

    let line = probe_line(&probe_source("BareNameProbe"), "// BP4");
    // Two expressions that both RESOLVE, and resolve differently: a bare name (EVAL-12) and the
    // qualified form. An expression that errors would satisfy "each recorded its own text" with an
    // error string, which is not the same claim.
    for expr in ["calls", "BareNameProbe$Child.shadowed"] {
        let set = server.call(
            "debug.set_line_stop",
            serde_json::json!({
                "class_pattern": "BareNameProbe", "line": line, "trace": true, "trace_expr": expr,
            }),
        );
        assert!(set.contains("bp_"), "could not arm a second stop point on line {line}: {set}");
    }

    // The line must actually run while both are armed, and the probe must keep running — both stop
    // points are traced, so nothing may freeze.
    let ticks_before = probe.output().iter().filter(|l| l.starts_with("tick ")).count();
    std::thread::sleep(std::time::Duration::from_secs(3));
    let ticks_after = probe.output().iter().filter(|l| l.starts_with("tick ")).count();
    assert!(
        ticks_after > ticks_before + 1,
        "the probe must keep ticking under two traced stop points — it went {ticks_before} -> \
         {ticks_after}\n  output tail: {:?}",
        probe.output().iter().rev().take(5).collect::<Vec<_>>(),
    );

    let traces = server.call("debug.get_traces", serde_json::json!({}));
    let bp1: Vec<&str> = traces.lines().filter(|l| l.starts_with('#') && l.contains("[bp_1]")).collect();
    let bp2: Vec<&str> = traces.lines().filter(|l| l.starts_with('#') && l.contains("[bp_2]")).collect();
    assert!(
        !bp1.is_empty() && !bp2.is_empty(),
        "BOTH stop points on line {line} must record — bp_1 has {} record(s), bp_2 has {}. The probe \
         ticked {} time(s) in the window, so the line certainly ran.\n{traces}",
        bp1.len(),
        bp2.len(),
        ticks_after - ticks_before,
    );
    // Each carries its own expression, so this is two stop points and not one reported twice.
    assert!(
        bp1.iter().any(|l| l.contains("| calls => (int)")),
        "bp_1 must record its own expression as a VALUE:\n  {bp1:?}"
    );
    assert!(
        bp2.iter().any(|l| l.contains("BareNameProbe$Child.shadowed => (int) 10")),
        "bp_2 must record its own, different expression as a value:\n  {bp2:?}"
    );
    // And the hit counts are each stop point's own, not one shared tally.
    let listing = server.call("debug.list_stop_points", serde_json::json!({}));
    assert!(
        !listing.contains("Hits: 0"),
        "neither stop point should report zero hits after the line ran:\n{listing}"
    );

    server.panic_reset();
}

// ---------------------------------------------------------------------------------------------
// EVAL-8 (#82) — float, double and char literals
//
// The literals exist so that a stop point can be armed on the ONE transaction whose amount disagrees,
// across thousands of clean ones. So every test here asserts against `MoneyProbe`'s own stdout rather
// than against the debugger's reply: a condition that silently never matches and a condition that is
// never evaluated both leave every tool reporting success, and the probe prints `offer` before the
// conditioned line and `charged` after it precisely so that the two are distinguishable.
// ---------------------------------------------------------------------------------------------

/// How many payments `MoneyProbe` cycles through, and which one of them is the odd one out.
const MONEY_CYCLE: i64 = 4;
const MONEY_ODD_INDEX: i64 = 2;

/// The `(int) n` a `debug.evaluate` reply carries, or `None` if it carries something else.
fn evaluated_int(reply: &str) -> Option<i64> {
    let rest = reply.split("(int) ").nth(1)?;
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '-')).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Arm one suspending `condition` on `MoneyProbe`'s conditioned line and prove it fired on the odd
/// payment and **only** on it.
///
/// Shared by the three literal kinds because the odd payment is odd on all three of its fields at once,
/// so a condition on any one of them must select the same hit — and disagreement between them would mean
/// one literal kind is not being compared at all. Three `#[test]`s over one helper rather than one test
/// looping three times: `shard-plan.py` cannot split a loop, and TEST-35 measured what that costs.
fn a_money_condition_fires_only_on_the_odd_payment(test: &str, condition: &str) {
    let Some(jdk) = jdk_or_skip(test) else { return };
    let mut probe = Probe::launch(&jdk, "MoneyProbe").expect("launch MoneyProbe");
    // The watchdog off: this test deliberately leaves the VM suspended at the matching hit, and a rescue
    // would resume it under the assertion that it is stopped.
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    probe.attach(&mut server);

    let line = probe_line(&probe_source("MoneyProbe"), "// BP1");
    let armed = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "MoneyProbe", "line": line, "condition": condition}),
    );
    assert_contains_all(&format!("the condition `{condition}` armed"), &armed, &["bp_"]);

    let hit = server.wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT).unwrap_or_else(|| {
        panic!(
            "`{condition}` never fired. The probe cycles all four payments every ~600ms, so the value it \
             names certainly came round — a condition that parses and then never matches is exactly what \
             this test exists to catch.\n  probe tail: {:?}",
            probe.output().iter().rev().take(10).collect::<Vec<_>>(),
        )
    });
    assert_contains_all("the matching hit suspended the VM", &hit, &["[suspended] true"]);

    // Which hit it was, read from the frame the condition matched on.
    let tick = evaluated_int(&server.evaluate("i")).unwrap_or_else(|| {
        panic!("could not read the tick off the suspended frame: {}", server.evaluate("i"))
    });
    assert_eq!(
        tick % MONEY_CYCLE,
        MONEY_ODD_INDEX,
        "`{condition}` stopped on tick {tick}, which is payment {} of the cycle rather than the odd one \
         ({MONEY_ODD_INDEX}). A comparison that is wrong about the literal's width or type does not fail \
         to fire — it fires on the wrong payment.",
        tick % MONEY_CYCLE,
    );
    assert_contains_all(
        "the amount the condition matched reads back exactly",
        &server.evaluate("p.vlPagamento"),
        &["1.005"],
    );

    // --- and now the probe's own account of it, which is the part no reply can fake ---
    let offer = format!("offer 1.005 taxa 0.1 moeda U tick {tick}");
    probe.wait_for_line(EVENT_TIMEOUT, |l| l == offer).unwrap_or_else(|| {
        panic!(
            "the probe never printed `{offer}` — the debugger says it stopped on tick {tick} but the probe \
         did not reach that payment.\n  probe tail: {:?}",
            probe.output().iter().rev().take(10).collect::<Vec<_>>(),
        )
    });
    let out = probe.output();
    assert!(
        !out.iter().any(|l| l == &format!("charged 1.005 tick {tick}")),
        "the probe printed the line AFTER the conditioned one, so it was never suspended on the hit the \
         reply claims: `{condition}`\n  probe tail: {:?}",
        out.iter().rev().take(10).collect::<Vec<_>>(),
    );
    // The non-matching hits were released rather than frozen — otherwise "fires only on the match" would
    // be satisfied by a condition that froze the probe on its very first hit.
    assert!(
        out.iter().any(|l| l.starts_with("charged 10.5 "))
            && out.iter().any(|l| l.starts_with("charged 99.99 ")),
        "the payments the condition does NOT match must have run to completion. Neither `charged 10.5` \
         nor `charged 99.99` is in the output, so this stopped on the first hit whatever the condition \
         said.\n  probe tail: {:?}",
        out.iter().rev().take(12).collect::<Vec<_>>(),
    );

    server.panic_reset();
}

/// The issue's own case: `1.005` is not representable, so this fires only if the debugger's parser rounds
/// the decimal string to the same f64 `javac` did.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_double_condition_fires_only_on_the_payment_that_matches() {
    a_money_condition_fires_only_on_the_odd_payment(
        "a_double_condition_fires_only_on_the_payment_that_matches",
        "p.vlPagamento == 1.005",
    );
}

/// The width test. `0.1f` widens to 0.100000001490116…, `0.1` to 0.100000000000000005…, and the `float`
/// field holds the former — so a literal that skipped f32 on the way in compares unequal to every value
/// the field can hold and this condition never fires.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_float_condition_is_exact_against_a_float_field() {
    a_money_condition_fires_only_on_the_odd_payment(
        "a_float_condition_is_exact_against_a_float_field",
        "p.taxa == 0.1f",
    );
}

/// A char literal reaches the comparison as a UTF-16 code unit and compares numerically, as Java's own
/// `==` on a `char` does.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_char_condition_fires_on_the_char_that_matches() {
    a_money_condition_fires_only_on_the_odd_payment(
        "a_char_condition_fires_on_the_char_that_matches",
        "p.moeda == 'U'",
    );
}

/// `[?vlPagamento > 99.99]` — and its `>=` twin, because the **pair** is what proves the threshold is
/// compared strictly rather than after a rounding step. One payment is exactly on it, and a filter built
/// only from values far from its threshold cannot tell the two apart.
///
/// The reply states `N of 4 matched`, which is the assertion: it is the filter's own count of what it
/// kept, not a substring of the expression it echoes back.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_double_filter_excludes_the_element_exactly_on_the_threshold() {
    let Some(jdk) = jdk_or_skip("a_double_filter_excludes_the_element_exactly_on_the_threshold") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "MoneyProbe").expect("launch MoneyProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    probe.attach(&mut server);

    // The stop point is setup rather than the thing under test: it is what gives the filter a suspended
    // frame to resolve `MoneyProbe.pagtos` against.
    let line = probe_line(&probe_source("MoneyProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "MoneyProbe", "line": line}));
    server.wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT).expect("the setup stop never fired");

    let strict = server.evaluate("MoneyProbe.pagtos[?vlPagamento > 99.99]");
    assert!(
        strict.contains("1 of 4 matched") && strict.contains("Pagto[1050.75"),
        "`> 99.99` must select exactly the one payment above it, and name it:\n{strict}"
    );

    let inclusive = server.evaluate("MoneyProbe.pagtos[?vlPagamento >= 99.99]");
    assert!(
        inclusive.contains("2 of 4 matched") && inclusive.contains("Pagto[99.99"),
        "`>= 99.99` must additionally select the payment that IS 99.99. If this selects one, the \
         comparison is not strict on the boundary and a threshold filter is silently wrong; if it selects \
         four, it is not comparing at all.\n  >  : {strict}\n  >= : {inclusive}"
    );

    // The other two literal kinds in a predicate, so the filter path is not only exercised for doubles.
    let by_taxa = server.evaluate("MoneyProbe.pagtos[?taxa > 0.05f]");
    assert!(
        by_taxa.contains("1 of 4 matched") && by_taxa.contains("Pagto[1.005"),
        "a float literal in a predicate must select the one payment whose taxa is higher:\n{by_taxa}"
    );
    let by_moeda = server.evaluate("MoneyProbe.pagtos[?moeda == 'U']");
    assert!(
        by_moeda.contains("1 of 4 matched") && by_moeda.contains("Pagto[1.005"),
        "a char literal in a predicate must select the one payment in the other currency:\n{by_moeda}"
    );

    server.panic_reset();
}

/// `f(float)` and `f(double)` are different candidates to the JVM, and an implementation that widened
/// every floating literal to `double` would resolve both to the same one. The probe's return values name
/// which method actually ran, so the debugger cannot be the only witness.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn float_and_double_literals_reach_the_overload_they_name() {
    let Some(jdk) = jdk_or_skip("float_and_double_literals_reach_the_overload_they_name") else { return };
    let mut probe = Probe::launch(&jdk, "MoneyProbe").expect("launch MoneyProbe");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    probe.attach(&mut server);

    // An invoke needs a thread suspended by an event.
    let line = probe_line(&probe_source("MoneyProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "MoneyProbe", "line": line}));
    server.wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT).expect("the setup stop never fired");

    let as_float = server.evaluate("MoneyProbe.cobrar(2.0f)");
    assert!(
        as_float.contains("float:2.0"),
        "`2.0f` must select cobrar(float) — the probe names the method that ran:\n{as_float}"
    );
    let as_double = server.evaluate("MoneyProbe.cobrar(1.5)");
    assert!(
        as_double.contains("double:1.5"),
        "`1.5` must select cobrar(double), the other member of the same pair:\n{as_double}"
    );
    // A double parameter reached with a literal, and a char parameter, which need the D and C tags to
    // survive the coercion path rather than only the comparison one.
    assert!(server.evaluate("MoneyProbe.taxar(1.5)").contains("taxa:1.5"), "a double argument");
    assert!(server.evaluate("MoneyProbe.marcar('x')").contains("char:x"), "a char argument");

    // And the write path: a double literal into a double static, echoed by reading it back.
    let wrote =
        server.call("debug.set_value", serde_json::json!({"target": "MoneyProbe.cobrado", "value": "2.5"}));
    assert_contains_all("a double literal writes to a double static", &wrote, &["✅"]);
    assert!(
        server.evaluate("MoneyProbe.cobrado").contains("2.5"),
        "the double written must read back: {}",
        server.evaluate("MoneyProbe.cobrado")
    );

    server.panic_reset();
}

// ---------------------------------------------------------------------------------------------
// EVAL-9 (#86) — an UNFETCHED Hibernate lazy association is a third answer
//
// The safety claim is that detection invokes NOTHING, so every test here ends by reading the
// `initialized` flag again: still false means nothing was loaded. Both probes are STRUCTURAL — the
// suite cannot depend on hibernate-core being installed — and each says at the top of its source what
// that does and does not prove. The names were measured separately, against `javap` on three real
// hibernate-core jars and against a real detached proxy through this debugger; #86 records both, and
// nothing here is evidence for them.
// ---------------------------------------------------------------------------------------------

/// Fully-qualified main classes: these two probes declare a package, because the thing under test IS a
/// fully-qualified type name (`org.hibernate.proxy.HibernateProxy`).
const LAZY_PROXY_MAIN: &str = "org.hibernate.proxy.LazyProxyProbe";
const LAZY_COLLECTION_MAIN: &str = "org.hibernate.collection.spi.LazyCollectionProbe";

/// An unfetched entity proxy is reported rather than walked into — on a method call AND on an inherited
/// field read, which is the case the brief does not name and the one with no error today.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn an_unfetched_proxy_is_reported_instead_of_being_initialised() {
    let Some(jdk) = jdk_or_skip("an_unfetched_proxy_is_reported_instead_of_being_initialised") else {
        return;
    };
    let mut probe = Probe::launch_in_package(&jdk, "LazyProxyProbe", LAZY_PROXY_MAIN).expect("launch");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("proxy implements marker: true"))
        .expect("the probe never confirmed its own shape — the stand-in is not what this test assumes");

    // A method call through the proxy: the case the issue was filed about.
    let called = server.evaluate("LazyProxyProbe.unfetched.getRef()");
    assert_contains_all(
        "a method call through an unfetched proxy is reported, not performed",
        &called,
        &["UNFETCHED Hibernate proxy", "getRef()", "NOT resolved", "force_initialize"],
    );
    assert!(
        !called.contains("WALKED IN"),
        "the getter RAN. That value is the probe's marker for a walk that should never have happened — a \
         real proxy would have issued SELECTs or thrown here:\n{called}"
    );

    // An INHERITED field read: silently wrong today, which is worse than the invoke.
    let field = server.evaluate("LazyProxyProbe.unfetched.id");
    assert_contains_all(
        "an inherited field read is reported too",
        &field,
        &["UNFETCHED Hibernate proxy", ".id"],
    );
    assert!(
        !field.contains("= null"),
        "reading `.id` answered null. On a proxy that is the UNPOPULATED inherited copy, not the entity's \
         id — a wrong answer with no error at all, which is why the check sits above the field/method \
         split:\n{field}"
    );

    // The proxy's OWN declared field is still readable, and it has to be: it is what the detection reads.
    assert_contains_all(
        "a field the proxy itself declares is its own state and stays readable",
        &server.evaluate("LazyProxyProbe.unfetched.$$_hibernate_interceptor.initialized"),
        &["(boolean) false"],
    );

    // The Hibernate 3.x/4.x spelling reaches the same verdict through the other field name.
    assert_contains_all(
        "the Javassist-era `handler` field is found too",
        &server.evaluate("LazyProxyProbe.unfetchedJavassist.getRef()"),
        &["UNFETCHED Hibernate proxy"],
    );

    // --- and the three things that must be UNAFFECTED ---
    assert_contains_all(
        "an INITIALISED proxy is an ordinary object",
        &server.evaluate("LazyProxyProbe.loaded.id"),
        &["null"],
    );
    assert!(
        !server.evaluate("LazyProxyProbe.loaded.id").contains("UNFETCHED"),
        "a proxy whose `initialized` is true must not be reported as unfetched"
    );
    // Named like a proxy, implements nothing: the INTERFACE decides, not the name.
    let not_really = server.evaluate("LazyProxyProbe.notAProxy.id");
    assert!(
        !not_really.contains("UNFETCHED"),
        "a class merely NAMED `$HibernateProxy$` is not one. The name is a cost gate; the marker interface \
         is the decision, and conflating them would report an unfetched row for any class named this \
         way:\n{not_really}"
    );
    assert_contains_all("and it reads normally", &not_really, &["3"]);
    assert_contains_all(
        "a plain entity is untouched",
        &server.evaluate("LazyProxyProbe.plainEntity.ref"),
        &["\"plain\""],
    );

    // THE SAFETY CLAIM: after all of the above, nothing has been initialised.
    assert_contains_all(
        "detection invoked nothing — the flag is still false",
        &server.evaluate("LazyProxyProbe.unfetched.$$_hibernate_interceptor.initialized"),
        &["(boolean) false"],
    );
}

/// `debug.evaluate_chain`'s third link ending, and the case a chain that ENDS on the lazy value would
/// otherwise get wrong: a rendered collection that looks empty when nobody has fetched it.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_chain_stops_at_an_unfetched_collection_and_says_which_of_three_it_is() {
    let Some(jdk) = jdk_or_skip("a_chain_stops_at_an_unfetched_collection_and_says_which_of_three_it_is")
    else {
        return;
    };
    let mut probe =
        Probe::launch_in_package(&jdk, "LazyCollectionProbe", LAZY_COLLECTION_MAIN).expect("launch");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("implements marker: true"))
        .expect("the probe never confirmed its own shape");

    // A suspending stop point, so the invoking assertions below have a thread JDWP will run a method on.
    // Setup, not the thing under test — the reports above and below need no suspension at all, which is
    // itself part of the point.
    let line = probe_line(&probe_source("LazyCollectionProbe"), "// BP1");
    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": LAZY_COLLECTION_MAIN, "line": line}),
    );
    server.wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT).expect("the setup stop never fired");

    // The next link is what would load it, so THAT is what gets reported.
    let sized = server.call(
        "debug.evaluate_chain",
        serde_json::json!({"expression": "LazyCollectionProbe.unfetched.size()"}),
    );
    assert_contains_all(
        "calling size() on an unfetched collection is reported",
        &sized,
        &["UNFETCHED Hibernate collection", "size()", "NOT resolved"],
    );
    assert!(
        !sized.contains("-1"),
        "size() RAN. -1 is the probe's marker for a call no real collection could answer, and reaching it \
         means the deferred SELECT was issued:\n{sized}"
    );

    // A chain that ENDS on the collection: neither null nor a value, and it must not read as empty.
    let ended = server
        .call("debug.evaluate_chain", serde_json::json!({"expression": "LazyCollectionProbe.unfetched"}));
    assert_contains_all(
        "a chain ending on the collection names the third outcome",
        &ended,
        &["⏳", "UNFETCHED Hibernate collection", "next link"],
    );
    assert!(
        !ended.contains("no link in this chain is null or unfetched"),
        "the walk reported a clean chain. An unfetched collection is not a clean answer — it is the \
         difference between 'the association is empty' and 'nobody looked':\n{ended}"
    );

    // A field read triggers nothing on a collection, so it must NOT be refused — including the flag
    // itself, which the first implementation refused.
    assert_contains_all(
        "a field read on a collection is safe and stays allowed",
        &server.evaluate("LazyCollectionProbe.unfetched.initialized"),
        &["(boolean) false"],
    );
    assert_contains_all(
        "including one the bag declares itself",
        &server.evaluate("LazyCollectionProbe.unfetched.role"),
        &["Reserva.reservaHotelList"],
    );

    // --- unaffected ---
    let fetched = server.call(
        "debug.evaluate_chain",
        serde_json::json!({"expression": "LazyCollectionProbe.fetched.size()"}),
    );
    assert!(
        fetched.contains("-1") && !fetched.contains("UNFETCHED"),
        "an INITIALISED collection must behave exactly as before — size() runs:\n{fetched}"
    );
    let not_a_collection = server.call(
        "debug.evaluate_chain",
        serde_json::json!({"expression": "LazyCollectionProbe.notACollection.size()"}),
    );
    assert!(
        !not_a_collection.contains("UNFETCHED"),
        "a class in org.hibernate.collection that implements the marker interface is a collection; one \
         that merely lives in the package is not. The package is a cost gate, not the decision:\n\
         {not_a_collection}"
    );

    assert_contains_all(
        "detection invoked nothing — the flag is still false",
        &server.evaluate("LazyCollectionProbe.unfetched.initialized"),
        &["(boolean) false"],
    );
}

/// `force_initialize:true` is the opt-in, and a read-only session refuses it by name.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn force_initialize_walks_in_when_the_caller_asks_for_it() {
    let Some(jdk) = jdk_or_skip("force_initialize_walks_in_when_the_caller_asks_for_it") else { return };
    let mut probe = Probe::launch_in_package(&jdk, "LazyProxyProbe", LAZY_PROXY_MAIN).expect("launch");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
    probe.wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("proxy implements marker: true")).expect("shape");

    let line = probe_line(&probe_source("LazyProxyProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": LAZY_PROXY_MAIN, "line": line}));
    server.wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT).expect("the setup stop never fired");

    // With the opt-in, the walk happens — and the probe's marker value is the proof that it did.
    let forced = server.call(
        "debug.evaluate",
        serde_json::json!({"expression": "LazyProxyProbe.unfetched.getRef()", "force_initialize": true}),
    );
    assert!(
        forced.contains("WALKED IN"),
        "force_initialize:true must actually walk in — otherwise the argument is decoration and the \
         caller has no way to get the value at all:\n{forced}"
    );
    // The chain tool takes it too, and the two must not disagree.
    let forced_chain = server.call(
        "debug.evaluate_chain",
        serde_json::json!({"expression": "LazyProxyProbe.unfetched.getRef()", "force_initialize": true}),
    );
    assert!(
        forced_chain.contains("WALKED IN"),
        "evaluate_chain must honour force_initialize identically to evaluate:\n{forced_chain}"
    );
}

/// The read-only half, and its own probe rather than a second session on the first one: a `dt_socket` agent
/// with `server=y` stops listening once a debugger has handshaked, so two servers cannot hold one probe.
///
/// `force_initialize` is refused **at the argument**. The load runs Hibernate's deferred SELECTs inside the
/// debuggee, so it is a write, and `read_only` exists to make a write impossible by accident. Refusing here
/// rather than letting the invoke fail is what makes the message name what the caller asked for.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_read_only_session_refuses_force_initialize_by_name() {
    let Some(jdk) = jdk_or_skip("a_read_only_session_refuses_force_initialize_by_name") else { return };
    let probe = Probe::launch_in_package(&jdk, "LazyProxyProbe", LAZY_PROXY_MAIN).expect("launch");
    // Wait for the probe's own shape line before asserting, as this test's two siblings do. Accepting a JDWP
    // connection is NOT the same as the class being loaded, and a static read against a class that has not
    // loaded yet answers "no loaded class matches" — correct, and it asserts the wrong finding (TEST-17,
    // #49). Measured rather than theoretical: without this the test passed on JDK 21 and 17 and failed
    // *deterministically* on JDK 11, which starts more slowly relative to the attach.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("proxy implements marker: true"))
        .expect("the probe never confirmed its own shape");
    let mut server = Server::start().expect("start server");
    let attached = server.call("debug.attach", serde_json::json!({"port": probe.port, "read_only": true}));
    assert_contains_all("the read-only session attached", &attached, &["Connected"]);

    let refused = server.call(
        "debug.evaluate",
        serde_json::json!({"expression": "LazyProxyProbe.unfetched.getRef()", "force_initialize": true}),
    );
    assert_contains_all(
        "read-only names force_initialize rather than failing inside the invoke",
        &refused,
        &["read-only", "force_initialize", "refused"],
    );

    // And without it, a read-only session still gets the honest answer rather than nothing — the report
    // needs no write, which is the reason it is the default.
    assert_contains_all(
        "the report itself needs no write, so read-only still gets it",
        &server.evaluate("LazyProxyProbe.unfetched.getRef()"),
        &["UNFETCHED Hibernate proxy"],
    );
}

// ---------------------------------------------------------------------------------------------
// DISC-12 (#95) — generic type information, and the fallback when there is none
//
// The fallback is the whole design risk: a generic signature is an OPTIONAL class-file attribute and
// the JDWP generic commands answer with an EMPTY STRING when a member has none, so a naive
// implementation renders a blank type where the raw descriptor used to be right. Every test here
// asserts the absent case in the same reply as the present one, which is the only way to know the
// fallback was exercised rather than merely believed.
// ---------------------------------------------------------------------------------------------

/// `debug.list_fields` — type arguments where the class file has them, and byte-identical output where
/// it does not.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn list_fields_shows_type_arguments_and_falls_back_where_there_are_none() {
    let Some(jdk) = jdk_or_skip("list_fields_shows_type_arguments_and_falls_back_where_there_are_none")
    else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "GenericsProbe", |l| l.contains("ready")).expect("launch");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let listed = server.call("debug.list_fields", serde_json::json!({"class_name": "GenericsProbe"}));
    assert_contains_all(
        "every parameterised field renders its type arguments",
        &listed,
        &[
            "java.util.List<java.lang.String> names",
            "java.util.Map<java.lang.Integer, java.util.List<GenericsProbe$Widget>> byId",
            // The worst case the issue names: two levels of Map, unreadable raw.
            "java.util.Map<java.lang.Integer, java.util.Map<java.lang.String, java.util.LinkedList<GenericsProbe$WSSessao>>> sessions",
            // A wildcard, which the acceptance criteria require not be mangled.
            "java.util.Map<java.lang.String, ? extends java.lang.Number> bounded",
            // An ARRAY of a parameterised type, not a parameterised type of an array.
            "java.util.List<java.lang.String>[] buckets",
        ],
    );
    assert_contains_all(
        "and a member with NO generic signature renders exactly what it did before DISC-12",
        &listed,
        &[
            "java.util.List rawList",
            "int rawQty",
            "java.lang.String rawName",
            "GenericsProbe$Widget[] rawWidgets",
            "long[][] rawGrid",
        ],
    );
    // The absence assertion that matters: an empty generic signature must never reach the output.
    for bad in ["  names", "static  names", "static final  rawQty", "<> "] {
        assert!(
            !listed.contains(bad),
            "a blank type reached the output ('{bad}'), which is what happens when an EMPTY generic \
             signature is used instead of falling back:\n{listed}"
        );
    }
    // Modifiers still render, and in front of the generic type rather than lost to it.
    assert!(
        listed.contains("static java.util.List<java.lang.String> names"),
        "the modifiers must still lead the declaration:\n{listed}"
    );
}

/// `debug.list_methods` — a generic signature supplies the type parameters, the parameter types and the
/// return type in one parse, and a method with none is unchanged.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn list_methods_shows_type_parameters_and_falls_back_where_there_are_none() {
    let Some(jdk) = jdk_or_skip("list_methods_shows_type_parameters_and_falls_back_where_there_are_none")
    else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "GenericsProbe", |l| l.contains("ready")).expect("launch");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let listed = server.call("debug.list_methods", serde_json::json!({"class_name": "GenericsProbe"}));
    assert_contains_all(
        "a generic method renders its type parameter, arguments and return",
        &listed,
        &[
            "<T> java.util.List<T> firstOf(java.util.List<T>, java.util.Map<java.lang.String, T>)",
            // An intersection bound, which only the generic grammar carries.
            "<T extends java.lang.Number & java.lang.Comparable<T>> T biggest(java.util.List<T>)",
        ],
    );
    assert_contains_all(
        "and a method with NO generic signature renders exactly what it did before DISC-12",
        &listed,
        &["static int twiceRaw(int)"],
    );
    assert!(
        !listed.contains("<> ") && !listed.contains("  twiceRaw"),
        "no blank type or empty type-parameter list may reach the output:\n{listed}"
    );
}

/// `debug.get_stack`'s locals — the declared type appears **only** where it says more than the value
/// beside it does, so an ordinary local is byte-identical and a `List` finally names its element type.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_stack_locals_declared_type_appears_only_where_it_adds_something() {
    let Some(jdk) = jdk_or_skip("a_stack_locals_declared_type_appears_only_where_it_adds_something") else {
        return;
    };
    let mut probe = Probe::launch(&jdk, "GenericsProbe").expect("launch");
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    probe.attach(&mut server);

    let line = probe_line(&probe_source("GenericsProbe"), "// BP1");
    server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "GenericsProbe", "line": line}));
    server.wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT).expect("the stop never fired");

    let stack = server.call("debug.get_stack", serde_json::json!({"include_variables": true}));
    assert_contains_all(
        "a parameterised local names its element type, which the value alone cannot",
        &stack,
        &[
            "java.util.List<GenericsProbe$Widget> lines =",
            "java.util.Map<java.lang.String, java.util.List<GenericsProbe$Widget>> grouped =",
        ],
    );
    // The other half, in the SAME reply: locals whose declared type says nothing new are untouched.
    assert_contains_all(
        "a local with no generic signature keeps the exact line it had before DISC-12",
        &stack,
        &["plainCount = (int) ", "plainText = \"unchanged\""],
    );
    assert!(
        !stack.contains("int plainCount") && !stack.contains("java.lang.String plainText"),
        "an erased type must NOT be printed in front of a local — `get_stack` never showed one, and \
         printing it everywhere would change every locals line in every reply for no gain:\n{stack}"
    );

    // And the element type is not decoration: it is what makes the next expression writable without a guess.
    assert_contains_all(
        "the type the listing named is the type the value actually holds",
        &server.evaluate("lines[0].qty"),
        &["(int) 3"],
    );

    server.panic_reset();
}

// ---------------------------------------------------------------------------------------------
// FILT-6 (#83) — a condition on the three stop-point kinds that never had one
//
// `CondKindsProbe` throws the same type with two different field values and NO message, which is the
// shape that makes the exception case expensive: three hits in every four are noise, and the only
// usable discriminator is a field on the exception INSTANCE. Every test here also proves the negative
// — that the noise value did NOT select — because a condition that matched everything and a condition
// that was never evaluated both look like success otherwise.
// ---------------------------------------------------------------------------------------------

/// A conditional EXCEPTION stop, suspending, asserted against the probe's own stdout.
///
/// The condition reads the thrown instance's field through the reserved `exception` head, which is the
/// piece of FILT-6 that did not exist before: the hit's top frame belongs to the throwing method, so
/// `this` is the thrower and the exception was unreachable from a condition.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn an_exception_condition_reads_the_thrown_instances_own_field() {
    let Some(jdk) = jdk_or_skip("an_exception_condition_reads_the_thrown_instances_own_field") else {
        return;
    };
    // `launch_running`, not `launch`: an exception breakpoint cannot be deferred, so `AppException` has to
    // have been thrown once before it can be armed. A `tick` line means one whole iteration has run.
    let mut probe =
        Probe::launch_running(&jdk, "CondKindsProbe", |l| l.starts_with("tick ")).expect("launch");
    // The watchdog off: this deliberately leaves the VM suspended at the matching throw.
    let mut server = Server::start_with_env(&[("JDWP_WATCHDOG_SECS", "0")]).expect("start server");
    probe.attach(&mut server);

    let armed = server.call(
        "debug.set_exception_stop",
        serde_json::json!({
            "class_pattern": "CondKindsProbe$AppException",
            "caught": true,
            "condition": "exception.cdException == 999",
        }),
    );
    assert_contains_all("the conditional exception stop armed", &armed, &["exc_"]);
    // The listing must show it, or a caller cannot tell a conditional stop from an unconditional one.
    assert_contains_all(
        "list_stop_points shows the condition",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &["Condition: exception.cdException == 999"],
    );

    let hit = server.wait_for_event("\"exception\"", EVENT_TIMEOUT).unwrap_or_else(|| {
        panic!(
            "the condition never fired. The probe throws every iteration and one in four carries 999, so \
             the value certainly came round — a condition that cannot reach the exception instance does not \
             error, it silently never matches.\n  probe tail: {:?}",
            probe.output().iter().rev().take(10).collect::<Vec<_>>(),
        )
    });
    assert_contains_all("the matching throw suspended the VM", &hit, &["[suspended] true"]);

    // The throwing frame's own local carries the same value, which also confirms the frame the condition
    // was evaluated on is the THROWING one. (`swallowed` is the catch parameter and is not assigned yet at
    // the throw site, so it is deliberately not what this reads.)
    assert_contains_all(
        "the frame the condition ran on is the throwing frame",
        &server.evaluate("cd"),
        &["(int) 999"],
    );

    // --- the probe's own account, which is what no reply can fake ---
    let out = probe.output();
    let last = out
        .iter()
        .rev()
        .find(|l| l.starts_with("offer ") || l.starts_with("done "))
        .expect("the probe printed neither line");
    assert!(
        last.starts_with("offer "),
        "the probe got past the throw, so it was never suspended on it — stdout ends at `{last}`"
    );
    let i: i64 = last.trim_start_matches("offer ").trim().parse().expect("an offer index");
    assert_eq!(
        i % 4,
        2,
        "the stop landed on iteration {i}, which is not one of the 999 iterations. A condition that read \
         the WRONG field, or the thrower instead of the exception, fires on the wrong throw rather than \
         failing to fire.\n  probe tail: {:?}",
        out.iter().rev().take(10).collect::<Vec<_>>(),
    );
    // And the noise iterations ran to completion, so the condition really did let them go.
    assert!(
        out.iter().any(|l| l == "done 0") && out.iter().any(|l| l == "done 1"),
        "the iterations whose cdException is 1 must have finished — otherwise this stopped on the first \
         throw whatever the condition said.\n  probe tail: {:?}",
        out.iter().rev().take(12).collect::<Vec<_>>(),
    );

    server.panic_reset();
}

/// A conditional FIELD stop and a conditional METHOD-EXIT stop, both traced — and the budget, which is
/// the acceptance criterion that a condition-skipped hit is not charged.
///
/// Traced rather than suspending because that is the shape the issue is about (filtering a trace on a
/// shared instance), and because the probe continuing to tick is the only evidence nothing froze.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn conditional_field_and_method_exit_traces_filter_without_charging_the_budget() {
    let Some(jdk) =
        jdk_or_skip("conditional_field_and_method_exit_traces_filter_without_charging_the_budget")
    else {
        return;
    };
    // `launch_running`, not `launch`: the first thing this test does is arm a WATCHPOINT, which cannot be
    // deferred, so the class has to be loaded before the arming — the TEST-17 (#49) race exactly. It lost
    // that race **deterministically on JDK 11** ("Class 'CondKindsProbe' is not loaded yet"), and passed on
    // 17, 21 and 25, which is the shape §5.5 of the handoff warns about: a slower start does not make a race
    // less likely, it changes the outcome. Found while running the suite for DUMP-7; the race predates it.
    let mut probe =
        Probe::launch_running(&jdk, "CondKindsProbe", |l| l.starts_with("tick ")).expect("launch");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // A field stop conditioned on the INCOMING value, which reading the field cannot give you:
    // FIELD_MODIFICATION is reported before the write lands.
    let watch = server.call(
        "debug.set_field_stop",
        serde_json::json!({
            "class_name": "CondKindsProbe",
            "field_name": "total",
            "on_write": true,
            "trace": true,
            "condition": "newValue == 999",
            "trace_max_hits": 30,
        }),
    );
    assert_contains_all("the conditional field stop armed", &watch, &["watch_"]);

    // A method-exit stop conditioned on a local of the returning frame, negated — so `!` is exercised
    // end to end and not only in the parser's unit tests.
    let mexit = server.call(
        "debug.set_method_exit_stop",
        serde_json::json!({
            "class_pattern": "CondKindsProbe",
            "method": "classify",
            "trace": true,
            "condition": "!(cd == 1)",
            "trace_max_hits": 30,
        }),
    );
    assert_contains_all("the conditional method-exit stop armed", &mexit, &["mexit_"]);

    // Wait for enough iterations that the noise value has been through several times.
    // `tick_index`, NOT `trailing_tick`: this probe prints `tick N` at the START of the line. Reaching for
    // the wrong one does not fail usefully — it returns None, the wait can never match, and the test dies
    // after the full timeout claiming the probe froze while its output plainly shows it ticking.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n >= 9))
        .expect("the probe never reached its 9th tick");

    let traces = server.call("debug.get_traces", serde_json::json!({}));
    assert!(
        traces.contains("999"),
        "neither conditional trace recorded the value it was armed for:\n{traces}"
    );
    // The negative, and the whole point: the noise value must not be in the buffer at all.
    //
    // **These needles were checked against a real reply, not guessed.** The first version of this loop
    // matched `-> (int) 1` where the renderer writes `new=(int) 1`, so it could never fire — and it was
    // hiding a real defect: the traced path was still reading `condition: None` for all three of the new
    // kinds, so neither condition was being evaluated at all. A defeat-the-fix run is what surfaced it,
    // and the lesson is that a negative assertion has to be *seen failing* before it is trusted.
    for record in traces.lines().filter(|l| l.starts_with('#')) {
        assert!(
            !record.contains("new=(int) 1 ") && !record.contains("returned=(int) 1 "),
            "a condition let a noise hit through. Three hits in four carry the value 1, so a condition \
             that is not being evaluated records them all:\n  {record}\n{traces}"
        );
    }
    // And the arithmetic, which is the same statement from the other side: far more hits than captures.
    let listing_now = server.call("debug.list_stop_points", serde_json::json!({}));
    let hits: Vec<i64> =
        listing_now.lines().filter_map(|l| l.trim().strip_prefix("Hits: ")?.trim().parse().ok()).collect();
    let captures: Vec<i64> = listing_now
        .lines()
        .filter_map(|l| l.trim().strip_prefix("⏱  Trace cost: ")?.split(' ').next()?.parse().ok())
        .collect();
    assert!(
        !hits.is_empty() && hits.iter().zip(&captures).all(|(h, c)| h > c),
        "every conditional stop point must have been HIT more often than it CAPTURED — equal counts mean \
         the condition is not filtering. hits={hits:?} captures={captures:?}\n{listing_now}"
    );

    // THE BUDGET: armed with 3, and the noise hits — far more than 3 by now — must not have spent it.
    let listing = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_contains_all(
        "both conditions are shown in the listing",
        &listing,
        &["Condition: newValue == 999", "Condition: !(cd == 1)"],
    );
    assert!(
        !listing.contains("[0 hit(s) left]") && !listing.contains("SPENT"),
        "a condition-skipped hit was charged to the trace budget. By tick 9 there have been ~7 noise hits \
         against a budget of 3, so a stop point that charged them would be spent — and 'exactly N traces, \
         then it stops' would mean something else entirely:\n{listing}"
    );

    // Nothing froze: the probe is still ticking, which is the only evidence for that. `highest_tick` and
    // `tick_index` because this probe's tick leads its line — see the note on the wait above.
    let before = highest_tick(&probe).unwrap_or(0);
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > before + 2)).is_some(),
        "the probe stopped ticking under two TRACED conditional stop points — a traced stop point must \
         hold only the hit thread, condition or no condition.\n  probe tail: {:?}",
        probe.output().iter().rev().take(8).collect::<Vec<_>>(),
    );
}

/// A read-only session refuses an INVOKING condition on all four kinds, naming the offending expression.
///
/// The refusal already existed for line stops; the other three passed `None` where the condition goes,
/// because there was nothing to check — not because a condition is exempt. Left alone, extending
/// conditions to three more kinds would have opened three holes in `read_only` at once.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_read_only_session_refuses_an_invoking_condition_on_every_kind() {
    let Some(jdk) = jdk_or_skip("a_read_only_session_refuses_an_invoking_condition_on_every_kind") else {
        return;
    };
    let probe = Probe::launch_running(&jdk, "CondKindsProbe", |l| l.starts_with("tick ")).expect("launch");
    let mut server = Server::start().expect("start server");
    let attached = server.call("debug.attach", serde_json::json!({"port": probe.port, "read_only": true}));
    assert_contains_all("the read-only session attached", &attached, &["Connected"]);

    let line = probe_line(&probe_source("CondKindsProbe"), "// MEXIT_LOCAL");
    let cases: [(&str, serde_json::Value); 4] = [
        (
            "debug.set_line_stop",
            serde_json::json!({"class_pattern": "CondKindsProbe", "line": line,
                               "condition": "classify(1) == 999"}),
        ),
        (
            "debug.set_exception_stop",
            serde_json::json!({"class_pattern": "CondKindsProbe$AppException", "caught": true,
                               "condition": "exception.toString() == \"x\""}),
        ),
        (
            "debug.set_field_stop",
            serde_json::json!({"class_name": "CondKindsProbe", "field_name": "total", "on_write": true,
                               "condition": "newValue.toString() == \"x\""}),
        ),
        (
            "debug.set_method_exit_stop",
            serde_json::json!({"class_pattern": "CondKindsProbe", "method": "classify",
                               "condition": "classify(2) == 1"}),
        ),
    ];
    for (tool, args) in cases {
        let refused = server.call(tool, args);
        assert_contains_all(
            &format!("{tool} must refuse an invoking condition in a read-only session"),
            &refused,
            &["Read-only session", "condition", "calls a method"],
        );
    }

    // And a NON-invoking condition on the same kinds is fine, so the refusal is about invocation rather
    // than about conditions.
    let ok = server.call(
        "debug.set_exception_stop",
        serde_json::json!({"class_pattern": "CondKindsProbe$AppException", "caught": true, "trace": true,
                           "condition": "exception.cdException == 999"}),
    );
    assert_contains_all(
        "a field-comparison condition is allowed read-only, which is what makes the refusal useful",
        &ok,
        &["exc_"],
    );
}

// ---------------------------------------------------------------------------------------------
// DUMP-7 (#96) — lock contention as events, with no suspend
//
// `MonitorProbe` is deliberately not `ContendedProbe`. That one takes locks and never gives them back,
// which is right for a *dump* and useless here: `MONITOR_CONTENDED_ENTER` fires once per waiter and
// `MONITOR_CONTENDED_ENTERED` never fires at all, so there is no pair and therefore no duration. This
// probe contends and RELEASES, over and over, on two locks whose hold times are an order of magnitude
// apart — which is what lets a `min_duration_ms` threshold be shown to keep one and drop the other,
// rather than merely to filter everything or nothing.
//
// Every test here asserts the probe is still ticking. That is the only evidence trace mode kept its
// promise: none of the six worker threads can report that it was never frozen, and main's tick stops the
// moment the VM is suspended.
// ---------------------------------------------------------------------------------------------

/// JDWP's `VirtualMachine` command set and its `CapabilitiesNew` command — the pair a `FaultRelay` keys on
/// to make the JVM lie about what it supports.
const VM_COMMAND_SET: u8 = 1;
const CAPABILITIES_NEW: u8 = 17;

/// The readiness line `MonitorProbe` prints once contention and waiting have DEMONSTRABLY happened.
///
/// Not `tick `: a tick only says main is running, and arming against a probe that has not yet contended
/// leaves a test waiting on events nothing produced — which fails as "the arming is broken" rather than as
/// "we were early". The probe asks the JVM rather than guessing, exactly as `ContendedProbe` does.
fn monitor_probe_ready(line: &str) -> bool {
    line.starts_with("monitors ready")
}

fn wedge_probe_ready(line: &str) -> bool {
    line.starts_with("wedge ready")
}

/// The `acquisitions` counter off `WedgeProbe`'s tick line — the contender's progress, seen from OUTSIDE
/// the debugger, which is the standard every non-suspending assertion here is held to.
fn wedge_acquisitions(probe: &Probe) -> Option<i64> {
    probe.output().iter().filter_map(|l| l.split("acquisitions=").nth(1)?.trim().parse::<i64>().ok()).max()
}

/// The `waits` counter off `WedgeProbe`'s tick line — the WAITER's progress, which is the thread a disowned
/// traced hit strands. `wedge_acquisitions` covers the contender and cannot see this one at all.
///
/// Split on whitespace rather than trimmed to the end of the line, because `waits=` is deliberately printed
/// *before* `acquisitions=` — see the probe's own note on why that order is load-bearing.
fn wedge_waits(probe: &Probe) -> Option<i64> {
    probe
        .output()
        .iter()
        .filter_map(|l| l.split("waits=").nth(1)?.split_whitespace().next()?.parse::<i64>().ok())
        .max()
}

/// How long to keep watching for a suspension left by a traced hit to clear before calling the thread
/// stranded (TEST-41, #126).
///
/// A bound on observing an **absence**, so it is sized off the positive it rules out rather than off
/// [`EVENT_TIMEOUT`] — the same argument as [`STUCK_CONFIRM`] and as `CLAUDE.md`'s TEST-30 note. The
/// positive here is a trace capture completing and resuming its thread: a handful of JDWP round trips on a
/// loopback connection, which this suite's own `⏱  Trace cost:` line reports in **milliseconds**. 3 s is
/// therefore a margin of two to three orders of magnitude, and a thread still held after it is not one
/// that is mid-capture. `EVENT_TIMEOUT`'s 25 s is sized for a JVM that has to *do* something first, which
/// is the opposite situation — and spending it here would trade a fast, legible failure for a slow one.
const TRACE_RESUME_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

/// DUMP-8 (#123): an invoking `trace_expr` is refused on the OPENING half of a pair, and the thing worth
/// keeping is why the refusal is the fix rather than the fallback.
///
/// The hazard is real and was reproduced before anything changed. `MONITOR_CONTENDED_ENTER` fires with the
/// hit thread queued at a `monitorenter`, so an invocation needing that monitor cannot proceed; the 2000 ms
/// budget frees the debugger and JDWP cannot cancel the call. Measured on Temurin 11.0.32 and 21.0.12
/// against `WedgeProbe`, whose lock is held 3000 ms:
///
/// ```text
/// | LOCK.stamp() => <error: invocation did not return within 2000ms …>
/// debug.list_threads {only_suspended:true}   0/7 … 0/7 … 1/7  0x2 wedge-contender [monitor]  (for ever)
/// ```
///
/// **The 1.2 s gap before that `1/7` is the whole reason this is an arm-time refusal.** It is exactly the
/// hold that was left: the invocation completes when the lock is released and the JVM re-suspends the
/// thread *then*, long after the capture path resumed it and moved on. Verifying the resume — reading the
/// count back and resuming until it clears, ADR-0003's rule — was written first and changed nothing; polled
/// every 400 ms with and without it the sequence is byte-identical. It was caught only because the negative
/// test PASSED without the fix, which is ADR-0034 earning its keep, and it is §3.4's lesson again: an
/// assertion can fire correctly and still prove the wrong thing.
///
/// So what is asserted here is the refusal, its precision, and the two things that must still work.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn an_invoking_trace_expr_is_refused_on_the_half_that_does_not_own_the_lock() {
    let Some(jdk) = jdk_or_skip("an_invoking_trace_expr_is_refused_on_the_half_that_does_not_own_the_lock")
    else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "WedgeProbe", wedge_probe_ready).expect("launch WedgeProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let arm = |server: &mut Server, args: serde_json::Value| server.call("debug.set_monitor_stop", args);

    // --- Refused wherever `blocked` is armed, including as part of `all` ---
    for kinds in [vec!["blocked"], vec!["all"], vec!["acquired", "blocked"]] {
        let refused = arm(
            &mut server,
            serde_json::json!({"kinds": kinds, "trace_expr": "LOCK.stamp()", "trace_max_hits": 1}),
        );
        assert_contains_all(
            &format!("an invoking trace_expr is refused when blocked is armed as {kinds:?}"),
            &refused,
            &["Refused", "CALLS A METHOD", "blocked", "cannot complete", "re-suspends the thread"],
        );
        // It must be the argument check talking, not the session lookup — otherwise this test would pass
        // against a build with no check at all.
        assert!(
            !refused.contains("No active debug session"),
            "refused for the wrong reason for kinds {kinds:?}: {refused}"
        );
    }

    // --- NOT refused on the three kinds where the thread owns the monitor ---
    //
    // A first cut of this refusal covered `wait` too, on an "opening half of a pair" framing, and CONTEXT.md
    // is what caught it: the glossary already said the thread owns the monitor at `wait`, because Java
    // requires holding one to call `wait()` on it at all. Measured on Temurin 21.0.12 through the released
    // server before the refusal existed — an invoking expression answered `(int) 7` on `wait` and `(int) 14`
    // on `waited`, the latter closing a question the glossary had left explicitly open.
    // TWO THINGS ABOUT THE EXPRESSION ARE LOAD-BEARING, and this loop used to get both wrong — it armed a
    // bare `LOCK.stamp()` on all three kinds and asserted only that the arming was not refused, which passes
    // against a build where the invocation never happens at all.
    //
    //  - IT MUST NAME THE MONITOR THE EVENT REPORTS ON. ADR-0036's own control is a `LOCK.stamp()` on a
    //    `waited` hit timing out at 2000 ms while `WAITED_ON.stamp()` returned, so the hazard is ownership
    //    of the *reported* monitor and not the event kind. Re-measured on Temurin 21.0.12: qualified
    //    `WedgeProbe.LOCK.stamp()` on `wait` still times out and leaves the waiter suspended for the rest of
    //    the run. Arming that here would be arming the very hazard the refusal exists to prevent.
    //  - IT MUST BE QUALIFIED. The frame at a wait hit is the native `java.lang.Object.wait0` with no local
    //    variable table, so a bare `LOCK` does not resolve there at all — and that failure looks nothing
    //    like a stall. ADR-0036 records that reading it as "it did not work" would have confirmed the wrong
    //    rule; a test that cannot tell the two apart is the same mistake with a green tick on it.
    for (kinds, expr) in [
        (vec!["acquired"], "WedgeProbe.LOCK.stamp()"),
        (vec!["wait"], "WedgeProbe.WAITED_ON.stamp()"),
        (vec!["waited"], "WedgeProbe.WAITED_ON.stamp()"),
    ] {
        // Emptied first, so `wait_for_traces` below cannot be satisfied by a record an earlier kind left
        // behind — which is exactly how the weaker version of this loop stayed green.
        server.call("debug.get_traces", serde_json::json!({"clear": true}));
        let accepted = arm(
            &mut server,
            serde_json::json!({"kinds": kinds, "trace_expr": expr,
                                                "trace_max_hits": 1, "trace_frames": 0}),
        );
        assert!(
            !accepted.contains("Refused"),
            "an invoking trace_expr must be accepted on {kinds:?}, where the thread owns the monitor: \
             {accepted}"
        );
        // Accepted is not the claim; RETURNING A VALUE is. Three outcomes must not read alike here — a value,
        // the 2000 ms timeout, and a name that never resolved — so the assertion is on the rendered `(int)`
        // and the other two are excluded by name.
        let recorded = server.wait_for_traces(expr, EVENT_TIMEOUT).unwrap_or_else(|| {
            panic!(
                "no snapshot for an invoking {expr} on {kinds:?}, so the ADR-0036 claim that it returns \
                 there is untested. probe output: {:?}",
                probe.output()
            )
        });
        // THE RECORD FOR THIS EXPRESSION, not the buffer. `debug.get_traces` returns the whole ring, and the
        // earlier iteration's `(int) N` is still in it — so asserting on the buffer passed against the bare
        // `LOCK.stamp()` this loop used to arm, matching a value another kind had produced. Measured, not
        // guessed: reverting the expression while asserting on the buffer left the test green.
        let line = recorded
            .lines()
            .find(|l| l.contains(expr))
            .unwrap_or_else(|| panic!("no line for {expr} in:\n{recorded}"));
        assert!(
            line.contains("(int) "),
            "an invoking trace_expr must RETURN A VALUE on {kinds:?}, where the thread owns the reported \
             monitor — accepted-but-never-invoked is what this used to assert. Got: {line}"
        );
        assert!(
            !line.contains("did not return within"),
            "the invocation STALLED on {kinds:?}, where the thread owns the reported monitor — that is the \
             hazard this kind is supposed to be free of: {line}"
        );
        // Cleared by the id the arming actually returned. The hardcoded `mon_all` this used to pass matched
        // nothing for these kinds (they arm `mon_acquired_N` / `mon_wait_N` / `mon_waited_N`), so the clear
        // was a silent no-op and `debug.panic` below was doing all the work.
        let id = accepted
            .split_whitespace()
            .find(|t| t.starts_with("mon_"))
            .unwrap_or_else(|| panic!("no mon_ id in the arm reply for {kinds:?}: {accepted}"))
            .to_string();
        let cleared = server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": id}));
        assert!(cleared.contains("cleared"), "clearing {kinds:?} did not report a clear: {cleared}");
        server.call("debug.panic", serde_json::json!({}));
    }

    // Several expressions: the offending one is named by index, or four expressions leave the caller
    // guessing which. Same discipline as the read-only refusal.
    let indexed = arm(
        &mut server,
        serde_json::json!({"kinds": ["blocked"], "trace_expr": ["LOCK.name", "LOCK.stamp()"]}),
    );
    assert_contains_all("the offending element is named", &indexed, &["trace_expr[1]"]);

    // --- What must still work, so this is not a blanket removal of trace_expr from the kind ---
    //
    // A FIELD READ on the same opening half. Asserted against a live hit rather than just an accepted
    // arming: the refusal is about invocation, and a read that armed but never recorded would be the same
    // loss by a quieter route.
    arm(
        &mut server,
        serde_json::json!({"kinds": ["blocked"], "trace_expr": "LOCK.name", "trace_max_hits": 1,
                           "trace_frames": 0}),
    );
    let field = server
        .wait_for_traces("LOCK.name =>", EVENT_TIMEOUT)
        .expect("a field read on the blocked half never recorded");
    assert_contains_all("a field read resolves on the blocked half", &field, &["\"wedge\""]);

    // And the debuggee is untouched by all of it.
    assert_wedge_untouched(&mut server, &probe);

    server.panic_reset();
}

/// The DUMP-7/8 sections' standing promise, asserted from OUTSIDE the debugger: a traced hit left nothing
/// suspended, and the contender is still getting through its synchronized block.
///
/// The second half is the one no reply of ours could fake, which is why it reads the probe's own counter
/// rather than a thread listing.
fn assert_wedge_untouched(server: &mut Server, probe: &Probe) {
    // Polled rather than read once (TEST-41, #126). `wait_for_traces` returns as soon as a record is
    // readable and the capture path files the record BEFORE it resumes the hit thread, so a single read
    // taken the moment it returns lands exactly on the boundary between "stranded for the life of the
    // JVM" — the hazard trace mode exists to prevent — and "caught mid-capture", which is not a defect at
    // all. Both failed with the same message, which claimed the first, so a sighting could not be acted on.
    if let Err(still) = server.wait_for_no_suspended(TRACE_RESUME_WINDOW) {
        panic!(
            "a traced hit left a thread STILL suspended after {TRACE_RESUME_WINDOW:?}, which is long past \
             a capture — so this is a stranded thread rather than one caught mid-capture. The waiter's own \
             counter, from outside the debugger, says waits={:?}:\n{still}",
            wedge_waits(probe)
        );
    }
    // The baseline is kept as an `Option` rather than defaulted, so "the probe never reported an
    // acquisition at all" cannot be dressed up as a reading of -1 — TEST-40 (#125) was exactly that
    // mistake, on this same pair of counters.
    let before = wedge_acquisitions(probe);
    let advanced = (0..60).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(150));
        wedge_acquisitions(probe) > before
    });
    assert!(
        advanced,
        "the contender never completed another acquisition, so something wedged it. acquisitions was \
         {before:?} and is {:?} now",
        wedge_acquisitions(probe)
    );
}

/// The `acquired` half carries none of this, and the reason is not visible in the code (DUMP-8, #123).
///
/// `MONITOR_CONTENDED_ENTERED` fires when the thread has GOT the lock, so an invocation needing that same
/// monitor re-enters it — Java monitors are reentrant per thread — and returns immediately. The opening
/// half is the hazard precisely because it is the one instant in the pair when the thread does not own
/// what the snapshot is about. Asserted rather than left implicit, so a future change that "fixes" this
/// half symmetrically has something to fail against.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn the_acquired_half_owns_the_lock_so_the_same_expression_resolves_there() {
    let Some(jdk) = jdk_or_skip("the_acquired_half_owns_the_lock_so_the_same_expression_resolves_there")
    else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "WedgeProbe", wedge_probe_ready).expect("launch WedgeProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    server.call(
        "debug.set_monitor_stop",
        serde_json::json!({"kinds": ["acquired"], "trace_expr": "LOCK.stamp()", "trace_max_hits": 1,
                           "trace_frames": 0}),
    );
    let acquired =
        server.wait_for_traces("monitor_acquired", EVENT_TIMEOUT).expect("the acquired half never fired");
    assert_contains_all(
        "the very expression that stalls on the blocked half returns a value here",
        &acquired,
        &["LOCK.stamp() =>", "(int) "],
    );
    assert!(
        !acquired.contains("did not return within"),
        "the acquired half must not time out — the thread owns the monitor by then:\n{acquired}"
    );
    assert!(
        server.call("debug.list_threads", serde_json::json!({"only_suspended": true})).starts_with("0/"),
        "nothing may be left suspended by a traced hit"
    );

    server.panic_reset();
}

/// All four kinds decode, are captured in trace mode, and the VM is never suspended.
///
/// The four acceptance criteria about decoding and volume land here: each kind is observed, a snapshot names
/// the lock and the thread, `MONITOR_WAIT`'s requested timeout and `MONITOR_WAITED`'s `timed_out` are both
/// reported — and **both readings of `timed_out`**, because "nobody signalled it" and "it was signalled" are
/// opposite diagnoses and a hard-coded flag would pass against only one.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn all_four_monitor_kinds_are_decoded_and_traced_without_suspending_anything() {
    let Some(jdk) = jdk_or_skip("all_four_monitor_kinds_are_decoded_and_traced_without_suspending_anything")
    else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "MonitorProbe", monitor_probe_ready).expect("launch");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let before = highest_tick(&probe).unwrap_or(-1);
    let armed = server.call(
        "debug.set_monitor_stop",
        // `all`, and `trace_max_hits: 0` because contention here runs at ~15 pairs/s plus ~60 waits/s and
        // the default 200 would disarm mid-test. Safe in a probe nobody else is using; it is exactly the
        // setting the tool description says to choose knowingly.
        serde_json::json!({"kinds": ["all"], "trace_max_hits": 0, "trace_frames": 1}),
    );
    assert_contains_all(
        "all four kinds armed, each under its own id",
        &armed,
        &["mon_blocked_", "mon_acquired_", "mon_wait_", "mon_waited_", "Mode: trace"],
    );

    // Each kind in turn. Polled separately rather than once for all four, so a failure names WHICH kind
    // never arrived — the four are decoded by three different code paths and the tails differ in width.
    for needle in ["monitor_blocked", "monitor_acquired", "monitor_wait", "monitor_waited"] {
        assert!(
            server.wait_for_traces(needle, EVENT_TIMEOUT).is_some(),
            "no {needle} snapshot arrived. The probe reports its own contention on every tick line, so \
             check whether it is producing any.\n  probe tail: {:?}",
            probe.output().iter().rev().take(6).collect::<Vec<_>>(),
        );
    }

    let traces = server.call("debug.get_traces", serde_json::json!({}));
    // A snapshot must NAME the lock, by type and by handle: "an Object" identifies nothing on a server
    // holding hundreds, and the handle is what correlates two threads onto one lock.
    assert_contains_all(
        "the snapshots name the locks by type and handle",
        &traces,
        &["MonitorProbe$FastLock@0x", "MonitorProbe$TimeoutLock@0x", "thread=0x"],
    );
    // The two things the wire really does carry, as opposed to the duration it does not.
    assert_contains_all(
        "the requested wait timeout and both readings of timed_out are reported",
        &traces,
        &["40ms requested", "timed out — no notify arrived", "notified"],
    );

    // --- the probe's own account, which is what no reply can fake ---
    let after = highest_tick(&probe).unwrap_or(-1);
    assert!(
        after > before,
        "the probe stopped ticking, so a monitor stop point armed with trace:true suspended the VM — which \
         is the one thing trace mode promises it will not do. before={before} after={after}\n  probe \
         tail: {:?}",
        probe.output().iter().rev().take(8).collect::<Vec<_>>(),
    );

    server.panic_reset();
}

/// A closed bracket carries a duration, and the reply says the DEBUGGER measured it.
///
/// The honesty half of ADR-0035, and it is the assertion that would catch the tempting shortcut: JDWP's
/// `MONITOR_WAIT` carries a `timeout` field, so an implementation could print that and look plausible. It is
/// the argument the caller passed to `wait(…)`, not a measurement — `wait(5000)` returning in 3ms still
/// reports 5000. So this checks the contended pair, where no number exists on the wire at all, and checks
/// that what is printed admits whose figure it is.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_paired_monitor_snapshot_carries_a_debugger_measured_duration() {
    let Some(jdk) = jdk_or_skip("a_paired_monitor_snapshot_carries_a_debugger_measured_duration") else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "MonitorProbe", monitor_probe_ready).expect("launch");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let before = highest_tick(&probe).unwrap_or(-1);
    // The default kinds are exactly this pair, so passing none is also a test of that default.
    let armed = server.call("debug.set_monitor_stop", serde_json::json!({"trace_max_hits": 0}));
    assert_contains_all(
        "the default arming is the contended pair, and the reply says whose measurement it is",
        &armed,
        &["mon_blocked_", "mon_acquired_", "blocked_for: measured across both events, BY THIS SERVER"],
    );

    // Polled on the MEASURED wording, not on `blocked_for=`, and the difference is a real race rather than
    // pedantry: with no threshold set an *unmeasurable* pair is kept (its start predates the arming, because
    // those threads were already blocked), and it carries a `blocked_for=` too. Waiting on the weaker needle
    // returned as soon as the first such snapshot landed and then asserted a figure that had not arrived —
    // which flaked on JDK 11 under full-suite contention and passed everywhere else. `wait_for_traces`'
    // contract is that the needle must be something only the expected record has.
    let traces = server
        .wait_for_traces("measured by the DEBUGGER across both events", EVENT_TIMEOUT)
        .unwrap_or_else(|| {
            panic!(
                "no snapshot carried a debugger-measured duration. A `blocked_for=` alone is not enough — an \
                 unmeasurable pair has one too.\n  traces: {}",
                server.call("debug.get_traces", serde_json::json!({}))
            )
        });
    // The opening half says where it started rather than printing a zero, which would read as "it was not
    // blocked at all" — the opposite of the finding.
    assert_contains_all(
        "the opening half reports a pending measurement rather than a zero",
        &traces,
        &["<pending — this is where it started"],
    );

    // A real figure, and one that matches the probe's own hold time. FastLock is held 60ms and SlowLock
    // 400ms, so anything under 10ms would mean the pairing matched the wrong two events.
    let measured: Vec<i64> = traces
        .lines()
        .filter_map(|l| {
            let (_, after) = l.split_once("blocked_for=")?;
            after.split("ms ").next()?.trim().parse().ok()
        })
        .collect();
    // >= 10ms, and the probe is what makes that a sound inference rather than a hopeful one: each holder waits
    // for its contender to report BLOCKED before it starts counting its hold, so a measured block is >= the
    // 60ms (fast) or 400ms (slow) hold on any runner. Before that, the contender's own 1ms spin could be
    // descheduled deep into the hold window and block legitimately briefly — CI reported `measured=[8]` on
    // JDK 11 while five other legs passed, and the old wording here blamed the pairing for it.
    assert!(
        measured.iter().any(|ms| *ms >= 10),
        "no measured block reached 10ms. The probe now guarantees >= 60ms by waiting for its contender to be \
         BLOCKED before timing the hold, so this is a pairing or arithmetic fault rather than runner load. \
         measured={measured:?}"
    );

    let after = highest_tick(&probe).unwrap_or(-1);
    assert!(after > before, "the probe stopped ticking: before={before} after={after}");
    server.panic_reset();
}

/// `min_duration_ms` between the probe's two hold times keeps the slow lock and drops the fast one.
///
/// **The discriminating test, and the reason the probe has two locks.** A threshold that filtered everything
/// and one that filtered nothing both look like success against a single duration. The gap here is an order
/// of magnitude — 60ms against 400ms — so a threshold of 200ms has a right and a wrong answer.
///
/// It also pins the two behaviours the threshold changes that a caller would otherwise read as bugs: the
/// opening half records nothing at all while it is set, and `Hits` keeps counting regardless — so "contended
/// constantly, never for long" stays distinguishable from "never contended".
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_monitor_duration_threshold_keeps_the_slow_lock_and_drops_the_fast_one() {
    let Some(jdk) = jdk_or_skip("a_monitor_duration_threshold_keeps_the_slow_lock_and_drops_the_fast_one")
    else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "MonitorProbe", monitor_probe_ready).expect("launch");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let before = highest_tick(&probe).unwrap_or(-1);
    let armed = server.call(
        "debug.set_monitor_stop",
        serde_json::json!({"min_duration_ms": 200, "trace_max_hits": 0, "trace_frames": 0}),
    );
    assert_contains_all(
        "the reply states what the threshold filters and what it changes about the opening half",
        &armed,
        &["min_duration_ms: 200", "filters what is RECORDED", "becomes pure timestamping"],
    );

    let traces = server.wait_for_traces("MonitorProbe$SlowLock@0x", EVENT_TIMEOUT).unwrap_or_else(|| {
        panic!("no SlowLock snapshot over 200ms arrived, though the probe holds it 400ms")
    });
    // The fast lock is held 60ms, so it must not be here at all.
    assert!(
        !traces.contains("MonitorProbe$FastLock@0x"),
        "a FastLock block (held 60ms) got past a 200ms threshold, so the threshold is not being applied to \
         the measured duration.\n  got: {traces}"
    );
    // And the opening half recorded nothing, which is what stops the budget going on "started blocking".
    assert!(
        !traces.contains("<pending — this is where it started"),
        "the opening half recorded snapshots despite min_duration_ms — the budget would go on lines that \
         cannot carry a duration.\n  got: {traces}"
    );
    // Nor may an UNMEASURABLE bracket get through. This is the case JDK 11 found and a faster JVM hid: the
    // first closing events after arming have no matching start, because those threads were already blocked
    // — so before this was fixed a 200ms threshold's very first snapshot was a 60ms lock. It is timing, not
    // a JDK difference, so passing on one JVM proves nothing here.
    assert!(
        !traces.contains("<not measured — no matching start"),
        "a bracket whose duration could not be measured was reported under a 200ms threshold. It might have \
         lasted 1ms; the argument's only promise is that it did not.\n  got: {traces}"
    );

    // The hit tally is the part that keeps the silence readable: it counts every event the JVM reported,
    // threshold or no threshold, so a quiet buffer beside a large Hits is an ANSWER rather than a gap.
    let listing = server.call("debug.list_stop_points", serde_json::json!({}));
    assert_contains_all(
        "the listing explains the suppression and still counts the hits",
        &listing,
        &["min_duration_ms: 200", "records NOTHING while it is set", "Hits: "],
    );
    let hits: Vec<i64> = listing
        .lines()
        .filter_map(|l| l.trim().strip_prefix("Hits: ")?.split_whitespace().next()?.parse().ok())
        .collect();
    assert!(
        hits.iter().any(|h| *h > 0),
        "every stop point reported Hits: 0 while snapshots were arriving — the tally is being charged after \
         the threshold, which is the thing that makes a filtered silence unreadable. hits={hits:?}"
    );

    let after = highest_tick(&probe).unwrap_or(-1);
    assert!(after > before, "the probe stopped ticking: before={before} after={after}");
    server.panic_reset();
}

/// One half of a pair reports its events and says the duration is not measurable — rather than printing a
/// zero, and rather than the reply implying a measurement it cannot make.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn an_unpaired_monitor_half_reports_events_but_no_duration() {
    let Some(jdk) = jdk_or_skip("an_unpaired_monitor_half_reports_events_but_no_duration") else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "MonitorProbe", monitor_probe_ready).expect("launch");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let armed = server.call(
        "debug.set_monitor_stop",
        serde_json::json!({"kinds": ["blocked"], "trace_max_hits": 0, "trace_frames": 0}),
    );
    assert_contains_all(
        "the arm reply says up front that no duration is available and what to add",
        &armed,
        &["mon_blocked_", "blocked_for: NOT available", "Add 'acquired'"],
    );
    assert!(
        !armed.contains("mon_acquired_"),
        "arming one kind must not arm its partner behind the caller's back: {armed}"
    );

    let traces = server
        .wait_for_traces("monitor_blocked", EVENT_TIMEOUT)
        .expect("a lone opening half must still report its events");
    assert_contains_all(
        "the snapshot says the duration is not measurable, and why",
        &traces,
        &["<not measurable — the other half of this pair is not armed"],
    );
    // The listing must agree with the snapshot. Re-derived from the session rather than remembered, which
    // is what makes clearing the partner change the answer.
    assert_contains_all(
        "the listing says the same thing",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &["blocked_for: unavailable", "'acquired' is not armed"],
    );

    server.panic_reset();
}

/// A flooding monitor stop point spends its trace budget, disarms itself, and SAYS so.
///
/// The acceptance criterion, and this is the easiest kind in the tool to make flood: the probe produces
/// roughly 75 monitor events a second across its four locks, so a budget of 5 is reached in well under a
/// second without any special arrangement.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_flooding_monitor_stop_disarms_itself_on_its_budget_and_says_so() {
    let Some(jdk) = jdk_or_skip("a_flooding_monitor_stop_disarms_itself_on_its_budget_and_says_so") else {
        return;
    };
    let mut probe = Probe::launch_running(&jdk, "MonitorProbe", monitor_probe_ready).expect("launch");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // THE PREMISE OF THE FINAL ASSERTION, ESTABLISHED RATHER THAN HOPED FOR (#125). `monitor_probe_ready`
    // waits for `monitors ready`, and its own doc explains why it is not `tick ` — correct for arming, but it
    // means readiness fires BEFORE the first tick line exists, so `before` was the `unwrap_or(-1)` default on
    // every single run. `after > before` then only means "kept ticking" if `before` was a real number: when a
    // flooding stop point on a starved 4-vCPU runner delayed main's first `println` past the budget disarm,
    // `after` was -1 as well, `-1 > -1` failed, and the message blamed the probe for STOPPING when it had
    // never started. Two of three CI attempts on JDK 11 shard 1/2, and 3/3 green locally — a gentler box, as
    // CLAUDE.md warns.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some())
        .expect("MonitorProbe never printed a tick, so there was never a witness to keep");
    let before = highest_tick(&probe)
        .expect("a tick line was seen but did not parse, which means tick_index and the probe disagree");
    server.call(
        "debug.set_monitor_stop",
        serde_json::json!({"kinds": ["wait"], "trace_max_hits": 5, "trace_frames": 0}),
    );

    let traces = server.wait_for_traces("reached its trace-hit budget", EVENT_TIMEOUT).unwrap_or_else(|| {
        panic!(
            "the budget note never appeared, so silence after 5 snapshots would read \
                                   as \"no more contention\"\n  probe tail: {:?}",
            probe.output().iter().rev().take(6).collect::<Vec<_>>()
        )
    });
    assert_contains_all(
        "the note names the remedy rather than only the fact",
        &traces,
        &["stopped recording", "trace_max_hits"],
    );

    // A disarm is not a freeze: the whole point of the TRACE-8 in-flight handling is that events the JVM
    // had already generated are dropped and resumed rather than surfaced as suspending hits.
    // Polled rather than read once: the claim is that the probe is STILL ticking, and one read taken the
    // instant the budget note appears can land between two of its 150 ms ticks. A bounded wait for the next
    // tick is the same observation without the coin flip — and it still fails, in the same time, if the
    // probe is genuinely frozen.
    let advanced = probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > before));
    assert!(
        advanced.is_some(),
        "the probe stopped ticking after the budget disarm, so an in-flight monitor event was surfaced as a \
         suspending hit. It WAS ticking before it (tick {before}), so this is a stop and not a slow \
         start.\n  probe tail: {:?}",
        probe.output().iter().rev().take(8).collect::<Vec<_>>(),
    );

    server.panic_reset();
}

/// A JVM that reports `canRequestMonitorEvents = false` is told what it cannot do AND what to use instead —
/// not handed a bare `NOT_IMPLEMENTED (99)`.
///
/// Driven through a `FaultRelay` that rewrites the `CapabilitiesNew` reply, because no `HotSpot` this
/// project has met answers `false`: every one measured says true, including Temurin 11.0.32. So the branch
/// that reports the refusal is unreachable from a real JVM and would otherwise ship untested.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_jvm_that_cannot_report_monitor_events_is_told_so_with_the_fallback_named() {
    let Some(jdk) = jdk_or_skip("a_jvm_that_cannot_report_monitor_events_is_told_so_with_the_fallback_named")
    else {
        return;
    };
    let probe = Probe::launch_running(&jdk, "MonitorProbe", monitor_probe_ready).expect("launch");

    // `CapabilitiesNew` is 32 one-byte booleans. Positions 1-16 true so nothing else in the server changes
    // behaviour, 17 (`canRequestMonitorEvents`) FALSE, 18 true — which also checks the decoder reads 17
    // rather than its neighbour, since getting the offset wrong by one would report 18's value here.
    let mut caps = vec![0u8; 32];
    for byte in caps.iter_mut().take(16) {
        *byte = 1;
    }
    caps[17] = 1;
    // `VirtualMachine` (set 1), `CapabilitiesNew` (command 17). Named rather than inlined because a wrong
    // pair here faults nothing and the test then passes for the wrong reason.
    let relay = FaultRelay::start(probe.port, vec![(VM_COMMAND_SET, CAPABILITIES_NEW, Fault::Payload(caps))])
        .expect("start fault relay");

    let mut server = Server::start().expect("start server");
    server.attach(relay.port);

    let refused = server.call("debug.set_monitor_stop", serde_json::json!({}));
    assert_contains_all(
        "the refusal names the capability, says what it means, and names the fallback",
        &refused,
        &["canRequestMonitorEvents = false", "debug.thread_dump", "suspend:true"],
    );
    // The message MENTIONS `NOT_IMPLEMENTED (99)` on purpose — it is telling the caller what they were
    // spared — so this cannot assert the string is absent. What it can assert is that the reply is the
    // capability refusal rather than the arming failing: an unguarded path produces "Failed to arm".
    assert!(
        !refused.contains("Failed to arm"),
        "the request reached the debuggee and was refused there, so the capability check did not fire: \
         {refused}"
    );
    // Nothing was armed, so the refusal left the debuggee untouched.
    assert_contains_all(
        "a refused arming registers no stop point",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &["No breakpoints set"],
    );

    server.panic_reset();
}

/// Every up-front refusal, in one test because none of them reaches the debuggee.
///
/// They run before the session is even resolved, so this needs no JVM at all — which is why they are one
/// cheap test rather than five slow ones. Three of the five exist because a JDWP modifier does not mean
/// what the argument reads like on this event kind (measured, ADR-0035) and two because a duration measured
/// across a pair needs the pair.
#[test]
fn every_monitor_arming_refusal_explains_itself_before_touching_the_debuggee() {
    let mut server = Server::start().expect("start server");

    let cases: Vec<(serde_json::Value, Vec<&str>)> = vec![
        // InstanceOnly: accepted by HotSpot and ignored, so it is refused rather than passed through.
        (
            serde_json::json!({"instance_id": "@0x1f4c"}),
            vec!["instance_id is not supported", "tests the frame's `this`", "Temurin 11.0.32"],
        ),
        // A suspending monitor stop has nothing to narrow to except one thread.
        (
            serde_json::json!({"trace": false}),
            vec!["Refused", "no thread_id", "contention is not a site you chose"],
        ),
        // ClassOnly means the LOCATION's class on the contended pair — measured, and the numbers are in the
        // message so a reader can see it was not inferred from the spec alone.
        (
            serde_json::json!({"monitor_class": "java.util.Hashtable"}),
            vec!["monitor_class is refused with", "0 events on blocked and 74 on wait"],
        ),
        // A threshold with one half of a pair could never record anything.
        (
            serde_json::json!({"kinds": ["blocked"], "min_duration_ms": 100}),
            vec!["min_duration_ms needs BOTH halves", "could never record anything"],
        ),
        // Count is applied per request by the JVM, so it and a threshold cannot both be satisfied.
        (
            serde_json::json!({"hit_count": 3, "min_duration_ms": 100}),
            vec!["cannot both be set", "DELETES the request"],
        ),
        // An unknown kind names all four and says they are pairs.
        (serde_json::json!({"kinds": ["entered"]}), vec!["is not a monitor event kind", "PAIRS"]),
        // DUMP-8 (#123): an invoking expression on the half where the thread does not own the monitor.
        // Here as well as in the live test because the live one costs a JVM and this costs nothing — and
        // because the sentence the refusal has to carry is the measured consequence, not the mechanism.
        (
            serde_json::json!({"kinds": ["blocked"], "trace_expr": "lock.getStatus()"}),
            vec!["CALLS A METHOD", "re-suspends the thread", "Read a FIELD instead"],
        ),
    ];
    for (args, wants) in cases {
        let refused = server.call("debug.set_monitor_stop", args.clone());
        assert_contains_all(&format!("refusal for {args}"), &refused, &wants);
        // The refusal must come from the argument check, not from there being no session — otherwise this
        // test would pass against a build with no checks at all.
        assert!(
            !refused.contains("No active debug session"),
            "refused for the wrong reason (no session) for {args}: {refused}"
        );
    }

    // And a well-formed call gets past the argument checks, so the refusals are not simply "always no".
    let no_session = server.call("debug.set_monitor_stop", serde_json::json!({}));
    assert_contains_all(
        "a valid arming reaches the session lookup",
        &no_session,
        &["No active debug session"],
    );

    // The same for an INVOKING expression on the three kinds where the thread owns the monitor. This is the
    // half a symmetric "fix" of DUMP-8 would break, and it is measured: on Temurin 21.0.12 an invoking
    // expression answered `(int) 7` on `wait` and `(int) 14` on `waited`. Costs no JVM to state here.
    let owned = server.call(
        "debug.set_monitor_stop",
        serde_json::json!({"kinds": ["acquired", "wait", "waited"], "trace_expr": "lock.getStatus()"}),
    );
    assert_contains_all(
        "invoking is accepted where the thread owns the monitor",
        &owned,
        &["No active debug session"],
    );
}

/// EVAL-11 (#124): every `debug.run_named_query` refusal that can be settled from the arguments alone is,
/// and none of them costs a JVM.
///
/// The reason this test exists separately from the live one is the reason its sibling above does: a caller
/// who has given contradictory arguments should learn that from the reply, not from a JPA exception thrown
/// three invocations deep with a message about something else. Each case asserts the refusal reached the
/// *argument* check rather than the session lookup, so the test cannot pass against a build with no checks.
#[test]
fn every_named_query_refusal_explains_itself_before_touching_the_debuggee() {
    let mut server = Server::start().expect("start server");

    let cases: Vec<(serde_json::Value, Vec<&str>)> = vec![
        // A missing or blank name. The tool's whole input is a name, so this is the first thing to settle.
        (serde_json::json!({"query_name": "   "}), vec!["query_name is required", "Nothing was sent"]),
        // Both binding forms. A JPQL query declares names or positions, never both, so merging them would
        // send a parameter the query cannot have.
        (
            serde_json::json!({"query_name": "R.f", "parameters": {"a": 1},
                               "positional_parameters": [1]}),
            vec!["not both", "one form or the other", "Nothing was sent"],
        ),
        // The same key twice, once as a value and once as an expression. Letting one win silently is how a
        // query answers a question the caller did not ask.
        (
            serde_json::json!({"query_name": "R.f", "parameters": {"codigo": "R-7"},
                               "parameter_expressions": {"codigo": "this.codigo"}}),
            vec!["was given twice", "silently letting one win"],
        ),
        // A collection parameter is refused rather than stringified: `IN (:names)` needs a Java collection,
        // and binding the JSON text of a list would return zero rows and look like an answer.
        (
            serde_json::json!({"query_name": "R.f", "parameters": {"codigos": ["A", "B"]}}),
            vec!["is a JSON array", "IN (:names)", "parameter_expressions"],
        ),
        (
            serde_json::json!({"query_name": "R.f", "parameters": {"where": {"a": 1}}}),
            vec!["is a JSON object", "only scalars map to a Java value"],
        ),
        // JPQL positions are 1-based, so 0 is not a position at all.
        (
            serde_json::json!({"query_name": "R.f", "parameter_expressions": {"0": "x"}}),
            vec!["not a valid position", "1-based"],
        ),
        (
            serde_json::json!({"query_name": "R.f", "parameter_expressions": {"codigo": "  "}}),
            vec!["is empty", "or drop the entry"],
        ),
        // `setMaxResults(0)` is legal JPA and useless here: it reports 0 rows whatever the query matches,
        // which is the one answer this tool must never give by accident.
        (
            serde_json::json!({"query_name": "R.f", "max_fetch": 0}),
            vec!["asks the provider for no rows at all", "TRUE count", "max_rows"],
        ),
    ];
    for (args, wants) in cases {
        let refused = server.call("debug.run_named_query", args.clone());
        assert_contains_all(&format!("refusal for {args}"), &refused, &wants);
        assert!(
            !refused.contains("No active debug session"),
            "refused for the wrong reason (no session) for {args}: {refused}"
        );
    }

    // And a well-formed call gets past the argument checks, so the refusals are not simply "always no".
    let no_session =
        server.call("debug.run_named_query", serde_json::json!({"query_name": "Reserva.findByCodigo"}));
    assert_contains_all("a valid call reaches the session lookup", &no_session, &["No active debug session"]);
}

/// EVAL-11 (#124): a named JPA query runs against a live `EntityManager`, and all four of the issue's
/// acceptance criteria are checked against the probe's OWN counters rather than against our reply.
///
/// The two that matter most cannot be proved by reading the reply at all, which is why `JpaProbe` carries
/// counters for them:
///
///  - **it does not flush.** Every query in the probe starts at JPA's default `FlushModeType.AUTO`, and
///    `getResultList()` increments `flushes` when it is still AUTO when the query runs. `flushes == 0`
///    afterwards is the only evidence that `FlushModeType.COMMIT` was actually set — a reply saying "flush
///    suppressed" proves nothing about whether it was.
///  - **it initialises nothing.** `Itens.toString()` and every getter on the row increments
///    `associationTouches` and returns a `WALKED IN` sentinel no correct reply could contain.
///    `associationTouches == 0` is what makes "fields were read, never invoked" a measurement.
///
/// The over-match is the shape #124 was filed about: the same query, once with both optional parameters
/// null and once with one bound, so 1000-versus-1 is a contrast rather than a number with nothing to compare
/// it to. `TABLE_ROWS` is read off the probe so the assertion cannot drift from what it builds.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_named_query_reports_its_over_match_without_flushing_or_initialising_anything() {
    let Some(jdk) =
        jdk_or_skip("a_named_query_reports_its_over_match_without_flushing_or_initialising_anything")
    else {
        return;
    };
    // `launch_in_package`, for the same reason EVAL-9's probe needs it: discovery turns on the
    // fully-qualified `jakarta.persistence.EntityManager`, so the stand-in has to be in that package and one
    // `.java` declares one package.
    let mut probe =
        Probe::launch_in_package(&jdk, "JpaProbe", "jakarta.persistence.JpaProbe").expect("launch JpaProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    // A SUSPENDING stop point in the frame that holds the bean as a parameter: it gives the free discovery
    // route something to find AND a thread suspended BY AN EVENT, which is the only kind JDWP will invoke on.
    let armed = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "jakarta.persistence.JpaProbe", "method": "workWithEm"}),
    );
    assert!(!armed.contains("Refused"), "the stop point was refused: {armed}");
    server
        .wait_for_event("workWithEm", EVENT_TIMEOUT)
        .unwrap_or_else(|| panic!("never suspended in workWithEm; probe output: {:?}", probe.output()));

    // --- (1) the over-match: both optional parameters null matches the whole table ---
    let over = server.call(
        "debug.run_named_query",
        serde_json::json!({"query_name": "Reserva.findByCodigoAndStatus",
                           "parameters": {"codigo": null, "status": null}, "max_rows": 2}),
    );
    let rows = probe_table_rows(&mut server);
    assert_contains_all(
        "the over-match reports the full count",
        &over,
        &[
            &format!("{rows} row(s)"),
            // The route is part of the answer: one costs nothing and the other does not exist.
            "found in frame 0 as local `em`",
            "jakarta.persistence",
            "codigo = null (null)",
        ],
    );
    // Bounded rendering, and the bound hides rows without hiding their number.
    assert_contains_all("the projection is bounded", &over, &["Rows 1-2 of", "more (raise max_rows)"]);

    // --- (3) rendering initialised nothing: the sentinel is absent AND the counter never moved ---
    assert!(!over.contains("WALKED IN"), "the projection invoked toString() on a row's association: {over}");
    assert_contains_all("a nested object is a handle, not a walk-in", &over, &["itens=", "Itens @0x"]);

    // --- (2) one parameter bound: the contrast that makes the over-match mean something ---
    let one = server.call(
        "debug.run_named_query",
        serde_json::json!({"query_name": "Reserva.findByCodigoAndStatus",
                           "parameters": {"codigo": "R-7", "status": null}, "max_rows": 2}),
    );
    assert_contains_all(
        "binding one parameter narrows to one row",
        &one,
        &["— 1 row(s)", "codigo = \"R-7\" (String)", "codigo=\"R-7\""],
    );

    // --- the probe's own counters, which is the half no reply of ours could fake ---
    let flushes = server
        .call("debug.evaluate", serde_json::json!({"expression": "jakarta.persistence.JpaProbe.flushes"}));
    assert!(
        flushes.contains("= (int) 0"),
        "a query FLUSHED the persistence context — under JPA's default AUTO that is a write to the \
         database performed by asking a question. Expected 0: {flushes}"
    );
    let touches = server.call(
        "debug.evaluate",
        serde_json::json!({"expression": "jakarta.persistence.JpaProbe.associationTouches"}),
    );
    assert!(
        touches.contains("= (int) 0"),
        "something invoked a method on a row or its association, so the projection initialised state it \
         promised not to. Expected 0: {touches}"
    );

    // POSITIVE CONTROL, and without it the two zeros above are worth nothing: a counter that CANNOT move
    // reads as a passing assertion for ever. This walks in deliberately — `@0x….getCodigo()` on a row the
    // projection just rendered — and the counter must follow. Same discipline as ADR-0034's: an assertion
    // can fire correctly and still prove the wrong thing.
    let row_handle = first_row_handle(&one);
    let walked =
        server.call("debug.evaluate", serde_json::json!({"expression": format!("{row_handle}.getCodigo()")}));
    assert!(
        walked.contains("R-7"),
        "the deliberate walk-in did not reach the row, so the control proves nothing: {walked}"
    );
    let after = server.call(
        "debug.evaluate",
        serde_json::json!({"expression": "jakarta.persistence.JpaProbe.associationTouches"}),
    );
    assert!(
        !after.contains("= (int) 0"),
        "invoking a getter on a row did NOT move associationTouches, so the counter is dead and the \
         'initialised nothing' assertion above was vacuous: {after}"
    );

    server.panic_reset();
}

/// PERF-1 (#100), the criterion that a **dependent** sequence is demonstrably not issued together: a row's
/// fields are read off the row's OWN type, so the values wave cannot be folded into the type wave.
///
/// The projection reads every row's type in one wave and every row's fields in a second. Those two waves
/// could be collapsed into one — every row's type read and every row's field read issued together — and it
/// would look like a further optimisation, cost half the round trips again, and be wrong: the field ids
/// would have to come from somewhere, and the only candidate is another row's type.
///
/// `Reserva.mixedTypes` makes that visible in a reply rather than arguable in a review. Its rows alternate
/// `Reserva` and `Itens`, and the two share no field, so a values read issued before its own type read came
/// back asks the JVM for field ids the object does not have.
///
/// **Measured, by building that merged wave and reading what came back** rather than predicting it. Every
/// second row read:
///
/// ```text
///   [1] JpaProbe$Reserva @0x12 <fields unreadable>
/// ```
///
/// Two failures in one line, and the second is the dangerous one. `<fields unreadable>` is `INVALID_FIELDID`
/// reported honestly — the JVM does reject a foreign field id, which was worth confirming rather than
/// assuming. But the row is also labelled `Reserva`, and it is an `Itens`: a caller reading that reply is
/// told the wrong type with no indication anything went wrong. So the assertions are on both halves, and the
/// positive one is the sharper: `skus` is a field of `Itens` and of nothing else here, so a row rendering
/// `skus=` proves the second wave used the right type's ids rather than merely having not crashed.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_heterogeneous_result_reads_each_rows_fields_off_its_own_type() {
    let Some(jdk) = jdk_or_skip("a_heterogeneous_result_reads_each_rows_fields_off_its_own_type") else {
        return;
    };
    let mut probe =
        Probe::launch_in_package(&jdk, "JpaProbe", "jakarta.persistence.JpaProbe").expect("launch JpaProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    let armed = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "jakarta.persistence.JpaProbe", "method": "workWithEm"}),
    );
    assert!(!armed.contains("Refused"), "the stop point was refused: {armed}");
    server
        .wait_for_event("workWithEm", EVENT_TIMEOUT)
        .unwrap_or_else(|| panic!("never suspended in workWithEm; probe output: {:?}", probe.output()));

    // Four rows is two of each type, which is what makes this a mixed set rather than a set with one
    // oddity in it: whichever type the first row has, the other appears twice.
    let mixed = server.call(
        "debug.run_named_query",
        serde_json::json!({"query_name": "Reserva.mixedTypes", "max_rows": 4}),
    );
    assert_contains_all(
        "both types are projected, each with its own fields",
        &mixed,
        &["Reserva @0x", "codigo=", "Itens @0x", "skus="],
    );
    assert!(
        !mixed.contains("<fields unreadable>"),
        "a row's fields were read with another type's field ids — which is exactly what folding the values \
         wave into the type wave would produce, and is why the two waves are two (PERF-1, #100):\n{mixed}"
    );
    // The projection still invokes nothing, which the mixed path must not quietly change: `Itens.toString()`
    // is the sentinel and reading `skus` as a field must not reach it.
    assert!(!mixed.contains("WALKED IN"), "the mixed-type projection invoked toString() on a row: {mixed}");

    server.panic_reset();
}

/// PERF-1 (#100): a wide result set costs a bounded number of round trips rather than two per row, measured
/// through the latency relay.
///
/// **The instrument is a marginal cost, not an elapsed time.** A query's fixed cost — discovery, the
/// invocations, the array read — is a few dozen round trips whichever way the rows are read, and at 8ms
/// apiece it dwarfs what is being measured. So this times the *same* query at two row counts and subtracts:
/// what is left is what one extra row costs. Then it does that at two round trip times and subtracts again,
/// which removes our own per-packet cost and leaves only what the wire charges per row.
///
/// The two hypotheses are far apart, which is what makes a wall clock adequate here:
///
/// - **serialised** — one `ReferenceType` and one `GetValues` per row, each awaited: `2 × RTT` per row,
///   so 16ms at this RTT. **Measured at 17.67ms** by running this test against the sequential projection.
/// - **independent reads** — both reads waved, 16 to a window: `2 × RTT / 16`, so 1ms. **Measured at
///   1.78ms**, a 9.9x cut. The gap between 1.0 predicted and 1.78 measured is the window's edges and the
///   per-wave fixed cost, and it is quoted rather than rounded to the prediction.
///
/// The assertion is that a row costs less than **one** round trip of wire time, where serialising would
/// charge two. That is 4.5x above the measured reading and 2.2x below the measured serialised one, which is
/// the margin a clock on a contended runner can carry.
///
/// **`Bare.all` and not `Reserva.findByCodigoAndStatus`, because the realistic row is the wrong instrument.**
/// A `Reserva` costs far more per row than its own two reads — each `String` field is a
/// `StringReference.Value` round trip and its association is another `ObjectReference.ReferenceType` — so
/// measured over `Reserva` this conversion moves the per-row wire cost from **79.19ms to 63.18ms**, exactly
/// the 2 x RTT it converts and a fifth of the total. That is the honest figure for the tool and it is
/// recorded in ADR-0038; it is not a measurement of the primitive, because seven of those eight round trips
/// per row are reads nothing has converted yet. `Bare` has a `long` and a `double` and no other cost, so
/// here the two reads *are* the per-row cost.
///
/// **The two readings are taken the way TEST-13 ([#38](https://github.com/YgorPerez/java-debugging-mcp/issues/38))
/// established**: one relay, one attach, the round trip dialled up and down under a live connection, arms
/// alternated, each scored on its fastest sample. Two attaches put a JVM handshake and several seconds
/// between the readings, and on a box running the rest of this suite that difference stopped meaning the wire.
///
/// The relay charges coalesced traffic once, so a wave that leaves in one `write` is charged one delay rather
/// than sixteen — which flatters exactly the arm under test. That is why the assertion is against `RTT` and
/// not against `2 × RTT / 16`: the lower bound is honest about which side it favours, and the number it is
/// held to is the one a serialised path could not reach even with the flattery.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_wide_result_set_costs_a_bounded_number_of_round_trips_per_row() {
    /// Samples per arm. Three is enough for one spike to be outvoted, as in
    /// `latency_added_to_the_wire_shows_up_as_held_time_per_packet`.
    const ROUNDS: usize = 3;
    /// Rows in the narrow arm — enough to pay every fixed cost the wide arm pays, and no more.
    const NARROW: usize = 2;
    /// Rows in the wide arm. 50 is three windows' worth of each wave, so the window's own arithmetic is
    /// exercised rather than a single wave that happens to fit.
    const WIDE: usize = 50;

    let Some(jdk) = jdk_or_skip("a_wide_result_set_costs_a_bounded_number_of_round_trips_per_row") else {
        return;
    };
    let probe =
        Probe::launch_in_package(&jdk, "JpaProbe", "jakarta.persistence.JpaProbe").expect("launch JpaProbe");

    let rtt = std::time::Duration::from_millis(8);
    // Attached through the relay rather than directly, so `probe` is never handed the server — which is
    // also why it is not `mut` here as it is in every other JpaProbe test.
    let relay = LatencyRelay::start(probe.port, std::time::Duration::ZERO).expect("start relay");
    let mut server = Server::start().expect("start server");
    // Attaching THROUGH the relay is the point: the server is told nothing about it.
    server.attach(relay.port);

    let armed = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "jakarta.persistence.JpaProbe", "method": "workWithEm"}),
    );
    assert!(!armed.contains("Refused"), "the stop point was refused: {armed}");
    server
        .wait_for_event("workWithEm", EVENT_TIMEOUT)
        .unwrap_or_else(|| panic!("never suspended in workWithEm; probe output: {:?}", probe.output()));

    let mut sample = |delay: std::time::Duration, rows: usize| -> f64 {
        relay.set_rtt(delay);
        let started = std::time::Instant::now();
        let reply = server
            .call("debug.run_named_query", serde_json::json!({"query_name": "Bare.all", "max_rows": rows}));
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        assert!(
            reply.contains("row(s)") && !reply.contains("Refused"),
            "the query has to succeed for its duration to mean anything — the reply was:\n{}",
            head_of(&reply)
        );
        elapsed
    };

    // Thrown away rather than averaged in: the first query on a fresh connection fills `TypeCache` with the
    // row type's signature, fields and superclass chain, so it does strictly more work than the ones being
    // compared. Both row counts, because the wide arm warms one extra thing — nothing, as it turns out, but
    // asserting that was not the job of this test.
    sample(std::time::Duration::ZERO, NARROW);
    sample(std::time::Duration::ZERO, WIDE);

    // Written this way rather than as `(WIDE - NARROW) as f64`: the gate fails on warnings and
    // `clippy::cast_precision_loss` is one.
    let extra_rows = f64::from(u32::try_from(WIDE - NARROW).unwrap_or(1));
    let mut near = Vec::with_capacity(ROUNDS);
    let mut far = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        // Narrow and wide adjacent within each arm, so a slow stretch lands on both and cancels in the
        // subtraction instead of inventing a marginal cost.
        let n_narrow = sample(std::time::Duration::ZERO, NARROW);
        let n_wide = sample(std::time::Duration::ZERO, WIDE);
        near.push((n_wide - n_narrow) / extra_rows);
        let f_narrow = sample(rtt, NARROW);
        let f_wide = sample(rtt, WIDE);
        far.push(((f_wide - f_narrow) / extra_rows, f_narrow - n_narrow));
    }

    // The fastest sample of each arm. A busy machine can only make a query slower, so the floor is the
    // closest thing to its cost with the noise removed — and it is a floor for both arms alike.
    let near_per_row = near.iter().copied().fold(f64::MAX, f64::min);
    let (far_per_row, fixed_added) =
        far.iter().copied().min_by(|a, b| a.0.total_cmp(&b.0)).expect("an arm with no samples");
    let rtt_ms = rtt.as_secs_f64() * 1000.0;

    // The guard that has to come first: if the relay is not actually delaying anything, every number below
    // is a measurement of nothing that will happily pass. The NARROW query pays the fixed cost — dozens of
    // round trips — so turning the wire up must move it by many multiples of one RTT.
    assert!(
        fixed_added > rtt_ms * 3.0,
        "turning the round trip up to {rtt_ms}ms only added {fixed_added:.1}ms to the fixed cost of the \
         query, which is a few dozen round trips. The relay is not delaying the traffic, so the per-row \
         figures below measure nothing.\n  near: {near:?}\n  far: {far:?}"
    );

    let wire_per_row = far_per_row - near_per_row;
    // Printed whether it passes or not, on the same principle as the runner's `JDK in use:` line: the next
    // person to touch this wants the reading, and instrumenting a passing test to get it is how a
    // measurement gets estimated instead.
    eprintln!(
        "PERF-1: a row costs {wire_per_row:.2}ms of wire time at a {rtt_ms}ms round trip \
         ({near_per_row:.2}ms/row straight through, {far_per_row:.2}ms/row away). Serialised: {:.0}ms.",
        rtt_ms * 2.0
    );
    assert!(
        wire_per_row < rtt_ms,
        "each extra row cost {wire_per_row:.2}ms of WIRE time at a {rtt_ms}ms round trip. A row is two \
         reads — its type and its fields — so reading them one at a time and awaiting each costs {:.0}ms \
         per row, and reading each as an independent read costs about {:.1}ms. This landed at or above one \
         whole round trip per row, which is the serialised shape (PERF-1, #100).\n  per row straight \
         through: {near_per_row:.2}ms\n  per row {rtt_ms}ms away: {far_per_row:.2}ms\n  near: {near:?}\n  \
         far: {far:?}",
        rtt_ms * 2.0,
        rtt_ms * 2.0 / 16.0
    );

    server.panic_reset();
}

/// The `@0x…` handle of the first projected row in a `debug.run_named_query` reply.
///
/// Taken from the reply rather than from a second heap query on purpose: the claim being tested is that the
/// projection hands back a usable handle, so the handle under test has to be the one it printed.
fn first_row_handle(reply: &str) -> String {
    reply
        .lines()
        .find(|l| l.trim_start().starts_with("[0] "))
        .and_then(|l| l.split_whitespace().find(|t| t.starts_with("@0x")))
        .map_or_else(|| panic!("no [0] row with an @0x handle in the reply:\n{reply}"), str::to_string)
}

/// The probe's table size, read off its own startup line so the over-match assertion cannot drift from the
/// number of rows it actually builds.
fn probe_table_rows(server: &mut Server) -> i64 {
    let read = server
        .call("debug.evaluate", serde_json::json!({"expression": "jakarta.persistence.JpaProbe.TABLE_ROWS"}));
    read.rsplit_once(") ")
        .and_then(|(_, n)| n.trim().parse::<i64>().ok())
        .unwrap_or_else(|| panic!("could not read TABLE_ROWS off the probe: {read}"))
}

/// EVAL-11 (#124), the other half of the live surface: a POSITIONAL bind, an unknown name, and a query that
/// throws while running.
///
/// A second test rather than a longer first one, on TEST-35's lesson: `shard-plan.py` splits by test, so a
/// long body is a floor under every shard count and two 3-second tests schedule better than one 6-second
/// one. They share no state — each launches its own probe — so nothing is lost by the split.
///
/// The three legs are grouped because they are all about a reply being the RIGHT one rather than merely
/// non-empty: the positional query has to read back *its own* JPQL (the probe returns a per-query string
/// precisely so that reading the wrong one would show), an unknown name has to be distinguishable from a
/// query that blew up, and a query that blew up must not borrow the unknown-name message.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_named_query_binds_by_position_and_keeps_its_two_failures_apart() {
    let Some(jdk) = jdk_or_skip("a_named_query_binds_by_position_and_keeps_its_two_failures_apart") else {
        return;
    };
    let mut probe =
        Probe::launch_in_package(&jdk, "JpaProbe", "jakarta.persistence.JpaProbe").expect("launch JpaProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);
    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "jakarta.persistence.JpaProbe", "method": "workWithEm"}),
    );
    server
        .wait_for_event("workWithEm", EVENT_TIMEOUT)
        .unwrap_or_else(|| panic!("never suspended in workWithEm; probe output: {:?}", probe.output()));

    // The same predicate bound POSITIONALLY, 1-based as JPQL's `?1` is, and reporting ITS OWN query text —
    // the probe returns a per-query string precisely so that reading the wrong one back would show here.
    let positional = server.call(
        "debug.run_named_query",
        serde_json::json!({"query_name": "Reserva.findByCodigoPositional",
                           "positional_parameters": ["R-7"], "max_rows": 2}),
    );
    assert_contains_all(
        "a positional bind is 1-based and reads back its own JPQL",
        &positional,
        &["— 1 row(s)", "?1 = \"R-7\" (String)", "?1 is null or r.codigo = ?1"],
    );

    // --- (4) an unknown name is its own answer, and does not pretend it could list the real ones ---
    let unknown =
        server.call("debug.run_named_query", serde_json::json!({"query_name": "Reserva.doesNotExist"}));
    assert_contains_all(
        "an unknown query name says so",
        &unknown,
        &[
            "No named query 'Reserva.doesNotExist'",
            "IllegalArgumentException",
            "CANNOT be listed",
            "@NamedQuery",
        ],
    );
    // A query that throws when RUN is a different diagnosis and must not borrow that message.
    let broken = server.call("debug.run_named_query", serde_json::json!({"query_name": "Reserva.broken"}));
    assert!(
        !broken.contains("No named query"),
        "a query that threw while RUNNING was reported as a name that does not exist: {broken}"
    );

    server.panic_reset();
}

/// EVAL-11 (#124): with no `EntityManager` in the frame, the refusal names the two-step instead of reaching
/// for a heap walk that could not work.
///
/// **The measurement behind it is the point.** `ReferenceType.Instances` answers about an object's EXACT
/// runtime class, so asking it for `jakarta.persistence.EntityManager` returns 0 however many beans are
/// alive — measured against this very probe: the interface answers **0 live instances** while the concrete
/// `JpaProbe$ProbeEntityManager` answers **1**. JDWP publishes no "which classes implement this interface"
/// command, so there is nothing to walk and no honest fallback to build. A refusal that names
/// `debug.list_instances` and `entity_manager` is worth more than a guessed list of provider class names,
/// which is the trap `LazyProxyProbe` documents at length.
///
/// `workWithoutEm` is static and holds no bean, and the probe keeps its `EntityManager` in a static field of
/// a *different* class — so `this` is absent, no local names it, and the free route genuinely has nothing.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn a_named_query_with_no_bean_in_the_frame_names_the_two_step_instead_of_guessing() {
    let Some(jdk) =
        jdk_or_skip("a_named_query_with_no_bean_in_the_frame_names_the_two_step_instead_of_guessing")
    else {
        return;
    };
    let mut probe =
        Probe::launch_in_package(&jdk, "JpaProbe", "jakarta.persistence.JpaProbe").expect("launch JpaProbe");
    let mut server = Server::start().expect("start server");
    probe.attach(&mut server);

    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "jakarta.persistence.JpaProbe", "method": "workWithoutEm"}),
    );
    server
        .wait_for_event("workWithoutEm", EVENT_TIMEOUT)
        .unwrap_or_else(|| panic!("never suspended in workWithoutEm; probe output: {:?}", probe.output()));

    let refused = server
        .call("debug.run_named_query", serde_json::json!({"query_name": "Reserva.findByCodigoAndStatus"}));
    assert_contains_all(
        "the refusal names the route out",
        &refused,
        &[
            "No EntityManager was found in this frame",
            "EXACT runtime class",
            "debug.list_instances",
            "entity_manager",
        ],
    );

    // The interface really does answer 0 while the concrete class answers 1 — asserted here rather than
    // taken on trust, because the whole refusal above rests on it and a future JVM could change it.
    let by_interface = server.call(
        "debug.list_instances",
        serde_json::json!({"class_names": ["jakarta.persistence.EntityManager"]}),
    );
    assert!(
        by_interface.contains("0 live instance(s)"),
        "the EntityManager INTERFACE reported instances, which would mean a heap route is possible after \
         all and this refusal is now the wrong answer: {by_interface}"
    );
    let by_class = server.call(
        "debug.list_instances",
        serde_json::json!({"class_names": ["jakarta.persistence.JpaProbe$ProbeEntityManager"]}),
    );
    assert!(
        by_class.contains("1 live instance(s)"),
        "the concrete implementation reported no instances, so the control for the measurement above did \
         not hold: {by_class}"
    );

    // And the handle it printed is what makes the two-step work — the refusal is a route, not a dead end.
    let handle = by_class
        .split_whitespace()
        .find(|t| t.starts_with("@0x"))
        .unwrap_or_else(|| panic!("no @0x handle in the listing: {by_class}"))
        .to_string();
    let ran = server.call(
        "debug.run_named_query",
        serde_json::json!({"query_name": "Reserva.findByCodigoAndStatus",
                           "entity_manager": handle, "max_rows": 1}),
    );
    assert_contains_all(
        "the handle the refusal pointed at works",
        &ran,
        &["row(s)", &format!("given as `{handle}`")],
    );

    server.panic_reset();
}
