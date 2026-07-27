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

mod common;

use common::{
    assert_contains_all, jdk_or_skip, probe_line, probe_source, probe_source_path, Fault, FaultRelay, Jdk,
    LatencyRelay, Probe, Server, EVENT_TIMEOUT,
};

/// EVAL-1 / EVAL-2: static-method invocation and object arguments, through the real handlers.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn evaluate_static_methods_and_object_arguments() {
    let Some(jdk) = jdk_or_skip("evaluate_static_methods_and_object_arguments") else { return };
    let probe = Probe::launch(&jdk, "EvalProbe").expect("launch EvalProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let source = probe_source("EvalProbe");
    let line = probe_line(&source, "// BP1");
    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "EvalProbe", "line": line}),
    );
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
    assert_contains_all("chained off a static call", &server.evaluate("EvalProbe.infraName().length()"), &["(int) 4"]);
    assert_contains_all("field then instance call", &server.evaluate("EvalProbe.holder.label()"), &["holder#3"]);
    assert_contains_all(
        "unknown static method",
        &server.evaluate("EvalProbe.noSuchMethod(1)"),
        &["has no static method"],
    );

    // --- EVAL-2: expressions as arguments, passed by reference ---
    assert_contains_all("local as arg to instance method", &server.evaluate("a.matches(b)"), &["(boolean) true"]);
    assert_contains_all("local as arg to static method", &server.evaluate("EvalProbe.describe(a)"), &["item:alpha/1"]);
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
    assert_contains_all("int boxes into Integer", &server.evaluate("EvalProbe.takesInteger(5)"), &["Integer:5"]);
    // Array covariance — a String[] is an Object[], which no signature comparison can tell you.
    assert_contains_all(
        "array covariance",
        &server.evaluate("EvalProbe.takesObjects(EvalProbe.words)"),
        &["Object[]:2"],
    );
    // The cheap path must be unchanged: an exact match still wins without asking the JVM anything.
    assert_contains_all("exact overload still preferred", &server.evaluate("EvalProbe.pick(a)"), &["Item:alpha"]);

    // --- Conditions whose right-hand side is an expression, not a literal ---
    // Each gets its own line so its hit is distinguishable from every earlier one by line number.
    // Both hold on every iteration, so a miss means the condition failed to evaluate.
    for (marker, condition) in [("// BP2", "a.name == b.name"), ("// BP3", "check == EvalProbe.twice(local)")] {
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
fn watchpoints_report_field_writes_and_reads() {
    let Some(jdk) = jdk_or_skip("watchpoints_report_field_writes_and_reads") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
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
    let hit = server.wait_for_event("field_modification", EVENT_TIMEOUT).expect("counter write never reported");
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
        &["\"method\":\"relabel\"", "\"field\":\"WatchProbe$Holder.label\"", "\"static\":false", "\"instance\":\"0x"],
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
        &server.call("debug.set_field_stop", serde_json::json!({"class_name": "WatchProbe", "field_name": "nope"})),
        &["has no field 'nope'"],
    );
    assert_contains_all(
        "unloaded class",
        &server.call("debug.set_field_stop", serde_json::json!({"class_name": "NoSuchClass", "field_name": "x"})),
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
    let set = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "LateWorker", "line": line}),
    );
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
    assert_contains_all("static field on the late class", &server.evaluate("LateWorker.label"), &["late-worker"]);

    // Once armed it is a normal breakpoint, so it must now appear as armed rather than deferred.
    assert_contains_all(
        "promoted from deferred to armed",
        &server.call("debug.list_stop_points", serde_json::json!({})),
        &["0 deferred"],
    );

    server.panic_reset();
}

/// TEST-1: `force_return` must change what the CALLER receives, not merely report success. Proven by
/// reading the probe's own stdout.
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
        // test, not a pass.
        assert!(
            !probe.output().iter().any(|l| l.contains(expected_output)),
            "probe printed {expected_output} before force_return — the probe is not a valid control"
        );

        server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "ForceProbe", "line": line}));
        server
            .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
            .unwrap_or_else(|| panic!("breakpoint at ForceProbe:{line} never fired"));

        let forced_reply = server.call("debug.force_return", serde_json::json!({"value": forced}));
        assert!(
            !forced_reply.contains("Failed") && !forced_reply.contains("error"),
            "force_return({forced}) was rejected: {forced_reply}"
        );

        // Clear first: leaving the breakpoint armed would re-suspend on the next iteration before
        // main() gets to print, and the test would time out waiting for its own effect.
        server.panic_reset();
        let seen = probe.wait_for_line(EVENT_TIMEOUT, |l| l.contains(expected_output));
        assert!(
            seen.is_some(),
            "caller never observed the forced value: expected a line containing {expected_output}, \
             force_return said: {forced_reply}\n  recent output: {:?}",
            probe.output().iter().rev().take(8).collect::<Vec<_>>()
        );
    }

    server.panic_reset();
}

/// OBJ-1: recursive expansion — nested objects, collections, cycles, and the bounds.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn deep_expansion_walks_objects_collections_and_survives_cycles() {
    let Some(jdk) = jdk_or_skip("deep_expansion_walks_objects_collections_and_survives_cycles") else { return };
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
    assert_contains_all(
        "List elements",
        &d3,
        &["tags = ", "[0] = \"urgent\"", "[1] = \"fragile\""],
    );
    assert_contains_all("Map entries render as key → value", &d3, &["counts = ", "\"a\" → (int) 1"]);
    assert_contains_all("Set elements", &d3, &["labels = ", "\"x\""]);
    assert_contains_all("Optional present and empty", &d3, &["note = Optional[\"gift\"]", "missing = Optional.empty"]);
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
    assert!(
        !d1.contains("city = "),
        "max_depth=1 must not reach order.customer.address.city:\n{d1}"
    );

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
    assert_contains_all("Optional via index is refused clearly", &server.evaluate("order.note[0]"), &["not indexable"]);

    // Bounds and type errors say which is which.
    assert_contains_all("array out of bounds", &server.evaluate("order.numbers[9]"), &["out of bounds"]);
    assert_contains_all(
        "non-int list index",
        &server.evaluate("order.lines[\"x\"]"),
        &["list index must be an int"],
    );

    // --- [a..b]: half-open slice ---
    let sliced = server.evaluate("order.lines[1..3]");
    assert_contains_all("slice reports selection and count", &sliced, &["2 of 5", "[0] = ", "Line(bb,5,false)", "Line(cc,2,true)"]);
    assert!(!sliced.contains("Line(aa"), "slice [1..3] must exclude element 0:\n{sliced}");
    assert!(!sliced.contains("Line(dd"), "slice [1..3] must be half-open:\n{sliced}");
    // An over-long range clamps rather than erroring — asking for "up to 100" is normal.
    assert_contains_all("over-long range clamps", &server.evaluate("order.lines[0..100]"), &["5 of 5"]);
    assert_contains_all("empty range", &server.evaluate("order.lines[2..2]"), &["0 of 5"]);
    assert_contains_all("array slice", &server.evaluate("order.numbers[0..2]"), &["2 of 3", "(int) 1"]);
    assert_contains_all("reversed range is rejected", &server.evaluate("order.lines[3..1]"), &["ends before it starts"]);

    // --- [?predicate]: left side resolves against each element ---
    let paid = server.evaluate("order.lines[?paid == true]");
    assert_contains_all("boolean field predicate", &paid, &["2 of 5 matched", "Line(aa,1,true)", "Line(cc,2,true)"]);
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
    assert_contains_all("no matches still reports the scan", &server.evaluate("order.lines[?qty > 999]"), &["0 of 5 matched"]);
    // A predicate that can't resolve on any element is an error, not "0 matched".
    assert_contains_all(
        "broken predicate is an error, not an empty result",
        &server.evaluate("order.lines[?nosuchfield == 1]"),
        &["failed on every element"],
    );
    assert_contains_all("string filter on a String list", &server.evaluate("order.tags[?length() == 7]"), &["1 of 2 matched"]);

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
    assert_contains_all("the hit names its thread and location", &hit, &["\"method\":\"hello\"", "\"thread\":\"0x"]);

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
    assert_contains_all("helloCounter's count", &server.evaluate("this.helloCounter.count"), &["(double) 42"]);

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
    let hello_only = server.evaluate("this.meterRegistry.meters.values()[?id.name != \"http.server.requests\"]");
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
    let Some(jdk) = jdk_or_skip("traced_exception_breakpoints_record_throws_without_suspending") else { return };
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

/// TRACE-2: a watchpoint in trace mode records the mutating location and the old → new pair without
/// suspending — "who mutates this?" answered on a JVM you are not allowed to freeze.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn traced_watchpoints_record_writes_without_suspending() {
    let Some(jdk) = jdk_or_skip("traced_watchpoints_record_writes_without_suspending") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

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
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2))
            .is_some(),
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
        assert_eq!(
            hit.matches(" ← ").count(),
            depth,
            "record({v}) has {depth} caller(s) above it: {hit}"
        );
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
    assert!(
        !traces.contains(" ← "),
        "trace_frames:0 must record no callers at all: {traces}"
    );
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
    lines
        .take_while(|l| l.starts_with("   "))
        .find(|l| l.contains("Trace cost:"))
        .map(str::to_string)
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
    let running = server.call(
        "debug.thread_dump",
        serde_json::json!({"name_filter": "deadlock", "monitors": true}),
    );
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
    let one = dump_section(&dump, "deadlock-one")
        .unwrap_or_else(|| panic!("no deadlock-one section in:\n{dump}"));
    let two = dump_section(&dump, "deadlock-two")
        .unwrap_or_else(|| panic!("no deadlock-two section in:\n{dump}"));
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
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2))
            .is_some(),
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
    let Some(jdk) = jdk_or_skip("a_monitors_only_dump_finds_the_cycle_for_a_fraction_of_the_packets")
    else {
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
    let refused = server.call(
        "debug.thread_dump",
        serde_json::json!({"monitors_only": true, "monitors": false}),
    );
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
    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "SlowToStringProbe", "line": line}),
    );
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

    let threads = server.call(
        "debug.list_threads",
        serde_json::json!({"name_filter": "pool-worker", "limit": 400}),
    );
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
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| l.contains("resumed"))
        .expect("the pool never resumed");

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

    let threads = server.call(
        "debug.list_threads",
        serde_json::json!({"name_filter": "pool-worker", "limit": 400}),
    );
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

    let traces = server
        .wait_for_traces("PoolProbe.doWork", EVENT_TIMEOUT)
        .unwrap_or_else(|| panic!("the filtered stop point never recorded a throw"));

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
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2))
            .is_some(),
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
    let truncated = server.call(
        "debug.thread_dump",
        serde_json::json!({"suspend": true, "max_suspend_ms": 1, "limit": 60}),
    );
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
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2))
            .is_some(),
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
    let packets = dump_packet_cost(&deep).expect("no packet cost in the dump");
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
    let m_packets = dump_packet_cost(&monitors).expect("no packet cost in the monitors-only dump");
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
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base))
            .is_some(),
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
    let packets = dump_packet_cost(&deep).expect("no packet cost in the dump");

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
        let packets = dump_packet_cost(&dump).expect("no packet cost");
        // The figure the dump reports about ITSELF, which is what a caller on a real instance reads
        // instead of doing this arithmetic (TEST-8). Asserting on the reported number rather than a
        // recomputed one is the point: it is the reading #24 wanted from the 8180.
        let reported = dump_per_packet_ms(&dump).expect("the dump must report its own per-packet cost");
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
    let dump = server.call(
        "debug.thread_dump",
        serde_json::json!({"name_filter": "deadlock", "suspend": true}),
    );
    assert!(!dump.contains("Read-only session"), "a dump invokes nothing, so read-only must allow it: {dump}");
    assert_contains_all("the read-only dump still correlates the locks", &dump, &["holds: DeadlockProbe$Lock", "held by"]);

    // A dump WITHOUT suspend:true must leave the VM running: no pause, so the ticks never stop.
    server.call("debug.thread_dump", serde_json::json!({"limit": 5}));
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 2))
            .is_some(),
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
    let Some(jdk) = jdk_or_skip("a_broad_suspending_method_exit_is_refused_and_panic_clears_the_rest")
    else {
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
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 1))
            .is_some(),
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
    assert!(panicked.contains("method-exit"), "panic must say it cleared the method-exit request: {panicked}");
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
    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "ExcProbe", "line": line}),
    );
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in ExcProbe.main never fired");
    server.call("debug.step_over", serde_json::json!({}));
    server
        .wait_for_event("\"event\":\"step\"", EVENT_TIMEOUT)
        .expect("step never reported");

    // A bare call still means "the newest event", as it always did — plus a note that there is more.
    let latest = server.last_event();
    assert_contains_all("newest event, and the backlog is announced", &latest, &[
        "\"event\":\"step\"",
        "[pending] 1 older event",
    ]);
    assert!(
        !latest.contains("\"event\":\"breakpoint\""),
        "the default limit must return only the newest event: {latest}"
    );

    // Both are still there. This is the assertion that fails against a single-slot `last_event`.
    let both = server.call("debug.get_last_event", serde_json::json!({"limit": 5}));
    assert_contains_all("both hits are retrievable", &both, &[
        "\"event\":\"breakpoint\"",
        "\"event\":\"step\"",
    ]);
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
    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "DeepProbe", "line": line}),
    );
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
    assert_contains_all("the cap is reported once, naming where it stopped", &stack, &[
        "node budget (1000) exhausted at #",
        "remaining frames not expanded",
    ]);
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
    assert_contains_all("evaluate keeps its documented per-expression budget", &one, &[
        "node budget (400) exhausted",
    ]);

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
    assert_contains_all("a frame below an expanded one still shows its locals", &main_frame, &[
        "i = (int)",
        "order = DeepProbe$Order",
    ]);

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
    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "DeepProbe", "line": line}),
    );
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint in DeepProbe.inspect never fired");

    let write = |server: &mut Server, target: &str, value: &str| {
        server.call("debug.set_value", serde_json::json!({"target": target, "value": value}))
    };

    // --- Array element: ArrayReference.SetValues, no invocation in the debuggee ---
    let set = write(&mut server, "order.numbers[1]", "42");
    assert_contains_all("array element written, old value reported", &set, &["numbers[1] = 42", "was (int) 2"]);
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
    assert_contains_all("filtered entries render as key → value", &matched, &[
        "3 of 5 entr(ies)",
        "\"bb\" → ",
        "Line(bb,5,false)",
        "\"dd\" → ",
        "Line(dd,9,false)",
        "\"ee\" → ",
    ]);
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
    let first = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
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
    assert_contains_all("both sessions, by endpoint", &listed, &[
        "2 session(s)",
        &first.port.to_string(),
        &second.port.to_string(),
        &first_id,
        &second_id,
    ]);
    assert_contains_all("the newest attach is current", &listed, &["← current"]);
    assert_eq!(listed.matches("← current").count(), 1, "exactly one session is current:\n{listed}");
    let current_line = listed
        .lines()
        .find(|l| l.contains("← current"))
        .expect("a current line");
    assert!(
        current_line.contains(&second_id),
        "the last attach should be current, got: {current_line}"
    );
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
    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "DeepProbe", "line": line}),
    );
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
    let (head, _) = line
        .split_once(" JDWP packet(s)")
        .unwrap_or_else(|| panic!("no packet count in: {line}"));
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
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at + 3))
            .is_some(),
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
    let set = server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
    let bp_id = grab_token(&set, "bp_").expect("no bp id in set reply");
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint never fired");

    // Arm a step (sets pending_step), then clear the breakpoint so ONLY the step remains as the reason
    // the VM is suspended. Now the watchdog's step-clearing is the only thing that can free the probe.
    server.call("debug.step_over", serde_json::json!({}));
    server.call("debug.clear_stop_point", serde_json::json!({"breakpoint_id": bp_id}));
    let frozen_at = highest_tick(&probe).unwrap_or(0);

    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at + 2))
            .is_some(),
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
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint never fired");
    let frozen_at = highest_tick(&probe).expect("no tick before suspension");

    let bye = server.call("debug.disconnect", serde_json::json!({"session_id": sid}));
    assert_contains_all(
        "disconnect reports it left the VM safe",
        &bye,
        &["Disconnected", "resumed all threads"],
    );

    // The debuggee's own ticks resuming is the only proof the VM was actually left running.
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at + 3))
            .is_some(),
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
        count_trace_records(&by_id), 3,
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
    assert_contains_all(
        "a self-disarmed watch stays listed as disabled",
        &listed,
        &[&watch_id, "DISABLED"],
    );
    // …and the probe keeps running (it never suspended, and now records nothing either).
    assert!(
        probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > base + 4)).is_some(),
        "probe stopped ticking after a budgeted trace watch"
    );

    // BP-2: re-arming the self-disarmed watch works, keeps the same id, and records again — which is
    // only possible because the definition survived the auto-disarm.
    let on = server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": watch_id, "enabled": true}));
    assert_contains_all("a self-disarmed watch can be re-armed", &on, &["Re-armed", &watch_id]);
    server.call("debug.get_traces", serde_json::json!({"clear": true}));
    assert!(
        (0..40).any(|_| {
            let got = count_trace_records(&server.call("debug.get_traces", serde_json::json!({}))) > 0;
            if !got { std::thread::sleep(std::time::Duration::from_millis(100)); }
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
                    && l.rsplit(' ').next()
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
    assert_contains_all("OR predicate", &or, &["3 of 5 matched", "Line(aa,1,true)", "Line(cc,2,true)", "Line(dd,9,false)"]);
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
        &format!("condition held at the first qualifying iteration (n == {expect_n}, resumed from {first_n})"),
        &server.evaluate("n"),
        &[&format!("(int) {expect_n}")],
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
            if !got { std::thread::sleep(std::time::Duration::from_millis(100)); }
            got
        }),
        "the trace breakpoint never recorded while enabled"
    );

    // Disable: the JDWP request is cleared but the definition kept.
    let off = server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": bp_id, "enabled": false}));
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
        count_trace_records(&server.call("debug.get_traces", serde_json::json!({}))), 0,
        "a disabled breakpoint must not fire"
    );

    // Re-enable: re-armed at the same location (with a fresh id), and snapshots resume.
    let on = server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": bp_id, "enabled": true}));
    assert_contains_all("enable re-arms", &on, &["Re-armed"]);
    assert!(
        (0..40).any(|_| {
            let got = count_trace_records(&server.call("debug.get_traces", serde_json::json!({}))) > 0;
            if !got { std::thread::sleep(std::time::Duration::from_millis(100)); }
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
    let again = server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": bp_id, "enabled": false}));
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
    assert_contains_all("the pause resume is reported as a pause", &note, &["watchdog auto-resumed", "debug.pause"]);
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
    let set = server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
    let bp_id = grab_token(&set, "bp_").expect("no bp id");
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint never fired");
    let frozen_at = highest_tick(&probe).expect("no tick before suspension");

    // Read AND DRAIN the events — the normal polling pattern EVT-1 added `drain` for. This is what used
    // to erase the watchdog's only record of which request froze the VM.
    let drained = server.call("debug.get_last_event", serde_json::json!({"drain": true}));
    assert!(drained.contains("breakpoint"), "expected the breakpoint hit before draining: {drained}");

    // The VM must be resumed…
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at))
            .is_some(),
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
    assert_contains_all("an explicit call is refused", &server.evaluate("order.lines[0].getQty()"), &["Read-only"]);

    // 4. Reads needing no invocation keep working — the honest cost is shallower output, not no output.
    assert_contains_all("a field read still works", &server.evaluate("order.status"), &["\"OPEN\""]);
    assert_contains_all("an array index still works", &server.evaluate("order.numbers[2]"), &["(int) 3"]);
    assert_contains_all("a nested field read still works", &server.evaluate("order.customer.name"), &["\"Ana\""]);
    assert_contains_all("get_stack still works", &server.call("debug.get_stack", serde_json::json!({})), &["inspect"]);

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
    assert_contains_all("an invoking trace_expr is refused at arm time", &texpr, &["Read-only", "trace_expr"]);

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

    let toggled = server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": bp_id, "enabled": false}));
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
    let refused = server.call(
        "debug.set_value",
        serde_json::json!({"target": "order.customer", "value": "order.status"}),
    );
    assert_contains_all(
        "a mismatched reference is refused",
        &refused,
        &["mismatch", "java.lang.String"],
    );

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
    assert_contains_all("get_stack still works", &server.call("debug.get_stack", serde_json::json!({})), &["inspect"]);

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
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at + 3))
            .is_some(),
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
    let set = server.call("debug.set_line_stop", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
    let bp_id = grab_token(&set, "bp_").expect("no bp id");
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint never fired");

    // Pause while already suspended at the breakpoint. This is the call that used to clobber the cause.
    server.call("debug.pause", serde_json::json!({}));
    let frozen_at = highest_tick(&probe).expect("no tick before suspension");

    // The watchdog must still disarm the breakpoint (not just resume), and the VM must STAY running —
    // a lost disarm shows up as the probe re-freezing within ~150ms of the resume.
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at))
            .is_some(),
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
    let on = server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": watch_id, "enabled": true}));
    assert_contains_all("re-armed by name", &on, &["Re-armed", "WatchProbe.counter"]);
    assert!(
        (0..40).any(|_| {
            let got = count_trace_records(&server.call("debug.get_traces", serde_json::json!({}))) > 0;
            if !got { std::thread::sleep(std::time::Duration::from_millis(100)); }
            got
        }),
        "the re-armed watchpoint never fired — re-resolution by name failed"
    );

    // A deferred breakpoint's class never loads; arming it is fine, but its class genuinely isn't there,
    // which is the state BP-4 says must be reported rather than guessed at.
    let deferred = server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "com.example.GoneAway", "line": 3}),
    );
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
    const ALL: [Self; 5] = [
        Self::Breakpoint,
        Self::Pause,
        Self::BreakpointThenPause,
        Self::BreakpointDrained,
        Self::Step,
    ];
}

/// Drive one (freeze, resume) pair and assert the invariant. Panics with the offending combination
/// named, so a failure says which state broke which path.
fn assert_resume_is_honest(jdk: &Jdk, freeze: Freeze, resume: Resume) {
    // The watchdog must be ON for its own case and OFF for the others — otherwise it would rescue a
    // broken `continue`/`panic` and the test would pass on someone else's work.
    let watchdog = if matches!(resume, Resume::Watchdog) { "1" } else { "0" };
    let probe = Probe::launch(jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server =
        Server::start_with_env(&[("JDWP_WATCHDOG_SECS", watchdog)]).expect("start server");
    server.attach(probe.port);
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
    let advanced = probe
        .wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some_and(|n| n > frozen_at + 2))
        .is_some();

    // Whatever the path says about itself, gathered after the fact: `continue`/`panic` answer inline,
    // the watchdog leaves its note on `list_stop_points` / `get_last_event`. After a disconnect there is
    // no session left to ask, so the inline reply is all there is.
    let said = if matches!(resume, Resume::Disconnect) {
        reply
    } else {
        format!("{reply}\n{}\n{}",
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
            if !got { std::thread::sleep(std::time::Duration::from_millis(100)); }
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
    let on = server.call("debug.toggle_stop_point", serde_json::json!({"breakpoint_id": watch_id, "enabled": true}));
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
                if !got { std::thread::sleep(std::time::Duration::from_millis(100)); }
                got
            }),
            "the re-armed watchpoint reported success but never fired against the reloaded class — \
             it was re-armed against the discarded type (BP-4)"
        );
    }

    // Whatever happened to the watchpoint, the debuggee must still be running.
    let now = probe.output().iter().filter(|l| l.starts_with("tick ")).count();
    assert!(
        probe
            .wait_for_line(EVENT_TIMEOUT, |l| l.starts_with("tick "))
            .is_some() && now > 0,
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
    let probe = Probe::launch(&jdk, "EvalProbe").expect("launch EvalProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // Substring match finds the probe and its nested classes.
    let mine = server.call("debug.list_classes", serde_json::json!({"filter": "EvalProbe"}));
    assert_contains_all("probe classes", &mine, &["EvalProbe", "EvalProbe$Item", "EvalProbe$Task"]);
    // Dotted FQNs, never the `Lpkg/Cls;` form the JVM actually reports — the caller composes against
    // the former and JDWP speaks the latter, which is the whole reason this renders anything.
    assert!(!mine.contains("LEvalProbe"), "signatures must be decoded, not raw JNI: {mine}");

    // A prefix anchors at the start, so it must not behave as a substring.
    let prefixed = server.call("debug.list_classes", serde_json::json!({"filter": "java.util.*", "limit": 200}));
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
    let arrays = server.call(
        "debug.list_classes",
        serde_json::json!({"filter": "*[]", "include_arrays": true, "limit": 5}),
    );
    assert!(arrays.contains("[]"), "include_arrays must surface array types: {arrays}");

    // Nothing matched must not read as "no such class" — the JVM only knows what it has loaded.
    let absent = server.call("debug.list_classes", serde_json::json!({"filter": "com.nosuch.*"}));
    assert_contains_all("absent class", &absent, &["Nothing matched", "not loaded yet"]);
}

/// DISC-2 (#30): a method listing the caller can compose a `debug.evaluate` call from, rather than
/// guessing at parameter lists that overload resolution will then refuse.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn list_methods_renders_java_signatures_and_marks_static() {
    let Some(jdk) = jdk_or_skip("list_methods_renders_java_signatures_and_marks_static") else { return };
    let probe = Probe::launch(&jdk, "EvalProbe").expect("launch EvalProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let m = server.call("debug.list_methods", serde_json::json!({"class_name": "EvalProbe", "limit": 100}));
    // Primitives, arrays and void all render as Java source types.
    assert_contains_all("rendered signatures", &m, &[
        "static int twice(int)",
        "static java.lang.String greet(java.lang.String)",
        "static void main(java.lang.String[])",
    ]);
    assert!(!m.contains("(I)"), "raw JVM descriptors must not leak into the listing: {m}");

    // Every overload appears — comparing their parameter lists side by side is the point.
    assert_eq!(m.matches("pick(").count(), 4, "all four pick overloads should be listed: {m}");
    // The instance overload is the one with no `static` marker.
    assert!(m.contains("java.lang.String pick(int)"), "instance overload missing: {m}");
    // The static initialiser is not callable and not breakable, so it is not listed.
    assert!(!m.contains("<clinit>"), "<clinit> is noise and should be omitted: {m}");

    let twice = server.call(
        "debug.list_methods",
        serde_json::json!({"class_name": "EvalProbe", "name_filter": "twice"}),
    );
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
    let missing = server.call(
        "debug.list_methods",
        serde_json::json!({"class_name": "com.example.NoSuchThing"}),
    );
    assert_contains_all("unloaded class", &missing, &["is not loaded", "debug.list_classes"]);
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
    let probe = Probe::launch(&jdk, "EvalProbe").expect("launch EvalProbe");
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
    let jvm_only = server.call(
        "debug.source",
        serde_json::json!({"class_name": "EvalProbe", "source_roots": []}),
    );
    assert_contains_all("JVM-reported source file", &jvm_only, &[
        "EvalProbe.java",
        "reported by the JVM",
        "No source roots are configured",
    ]);
    assert!(!jvm_only.contains("// BP1"), "an empty root list must read no file at all: {jvm_only}");
    // A JDK class proves the command rather than our probe's build: nothing local could supply this.
    let jdk_class = server.call(
        "debug.source",
        serde_json::json!({"class_name": "java.lang.String", "source_roots": []}),
    );
    assert!(jdk_class.contains("String.java"), "SourceFile for a JDK class: {jdk_class}");

    // --- a bounded window around a line, numbered ---
    let window = server.call(
        "debug.source",
        serde_json::json!({"class_name": "EvalProbe", "line": bp1, "context": 2}),
    );
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
    let whole = server.call(
        "debug.source",
        serde_json::json!({"class_name": "EvalProbe", "whole_file": true}),
    );
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
    assert_contains_all("no root holds it", &elsewhere, &[
        "reported by the JVM",
        "Not found on disk",
        "Searched 1 root",
    ]);

    // A line past the end of the file is source DRIFT, not an error — and telling the caller which it
    // is only works because the JVM's answer and the local file arrive in the same reply.
    let stale = server.call(
        "debug.source",
        serde_json::json!({"class_name": "EvalProbe", "line": total + 500}),
    );
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
    let Some(jdk) = jdk_or_skip("source_says_when_a_loaded_class_carries_no_source_file_attribute")
    else {
        return;
    };
    let probe = Probe::launch_stripped(&jdk, "StrippedProbe").expect("launch StrippedProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    let stripped = server.call(
        "debug.source",
        serde_json::json!({"class_name": "StrippedProbe", "source_roots": []}),
    );
    assert_contains_all("no SourceFile attribute", &stripped, &[
        "NO source file",
        "-g:none",
        "debug.list_methods",
    ]);
    // Not the unloaded answer. The class is right there and the attribute is what is missing, so a reply
    // saying "not loaded" would send someone hunting a classpath problem that does not exist.
    assert!(!stripped.contains("is not loaded"), "ABSENT_INFORMATION is not 'not loaded': {stripped}");
    let methods = server.call("debug.list_methods", serde_json::json!({"class_name": "StrippedProbe"}));
    assert_contains_all("the stripped class really is loaded", &methods, &["main"]);

    // Control, same session and same connection: a class that DID keep the attribute still answers.
    // Without it this test would also pass with `SourceFile` broken outright, or with every reply an
    // error the assertions happened to match — the green-run-of-nothing shape this repo keeps finding.
    let jdk_class = server.call(
        "debug.source",
        serde_json::json!({"class_name": "java.lang.String", "source_roots": []}),
    );
    assert_contains_all("a class that kept its attribute", &jdk_class, &["String.java", "reported by the JVM"]);

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
    let Some(jdk) =
        jdk_or_skip("source_reports_the_smap_when_the_class_was_translated_from_another_file")
    else {
        return;
    };
    let probe = Probe::launch_with_smap(&jdk, "SmapProbe").expect("launch SmapProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    // Needs no local file: the SMAP travels in the class, so this half answers on a box holding no
    // source at all — which is the half that matters when the question is what the deployed thing is.
    let translated = server.call(
        "debug.source",
        serde_json::json!({"class_name": "SmapProbe", "source_roots": []}),
    );
    assert_contains_all("SMAP present", &translated, &[
        "SmapProbe.java",
        "JSR-45 SMAP) present",
        "is the intermediate",
        "hello.jsp",
        "*S JSP",
    ]);
    // The whole attribute came back, not just a header that happened to match: `*L` and a line-section
    // entry are the last bytes of the fixture, so their arrival proves the length field was right too.
    assert_contains_all("the whole SMAP round-tripped", &translated, &["*L", "1#1,3:24", "*E"]);

    // The control: same file, same compile, no attribute. `debug.source` must say nothing about an SMAP
    // for it — an unconditional banner would be worse than none, since it would teach the reader to
    // ignore the one case where the `.java` really is not what anyone wrote.
    let plain = server.call(
        "debug.source",
        serde_json::json!({"class_name": "Neighbour", "source_roots": []}),
    );
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
    assert_contains_all("labelled intermediate, still read", &both, &[
        "is the intermediate",
        "hello.jsp",
        "public class SmapProbe",
    ]);
}

/// `retired=<n>` from a `ChurnProbe` heartbeat — how many workers have finished so far.
fn churn_retired(line: &str) -> Option<u64> {
    line.split("retired=").nth(1)?.split_whitespace().next()?.parse().ok()
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
/// The probe reaches (2) on purpose: `ChurnProbe` runs an explicit collector, because a JDWP object id is
/// a weak reference and a retired worker's `Thread` is unreachable the moment it exits. Nothing forces
/// the two states to happen in a given dump, so this takes several and requires each shape once; failing
/// to find either is reported as "the probe is not churning", which is what it would actually mean.
///
/// Both of the pinned messages below are, in the author's reading, WRONG — see the comments. They are
/// pinned rather than fixed because a test that reaches a line and asserts nothing about it is how this
/// repo has previously reported coverage it had not looked at. Fixing either should flip this test.
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
    let relay =
        LatencyRelay::start(probe.port, std::time::Duration::from_millis(2)).expect("start relay");
    let mut server = Server::start().expect("start server");
    server.attach(relay.port);

    // A whole generation of workers must have turned over before anything is asserted: at
    // `SLOTS / LIFE_MS` a first tick can arrive before any churn worker has retired at all, and a dump
    // taken then would be a dump of a perfectly stable JVM.
    probe
        .wait_for_line(EVENT_TIMEOUT, |l| churn_retired(l).is_some_and(|n| n >= 48))
        .unwrap_or_else(|| panic!("the pool never retired a full generation\n  output: {:?}", probe.output()));

    // `limit` is deliberately an order of magnitude above the thread count. It matters later: whatever
    // the dump ends up withholding, the caller's limit is provably not the reason.
    let ask = serde_json::json!({"limit": 500, "monitors": false, "max_frames": 3});
    let mut vanished_but_reported: Option<String> = None;
    let mut vanished_and_dropped: Option<String> = None;
    for _ in 0..12 {
        let dump = server.call("debug.thread_dump", ask.clone());

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
            stable, 8,
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
        panic!(
            "twelve dumps and no thread ever finished during one — ChurnProbe is not churning\n  \
             output: {:?}",
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
    let section = dump_section(&reported, &name)
        .unwrap_or_else(|| panic!("no section for {name} in:\n{reported}"));
    // FINDING, pinned. The JVM has just answered ZOMBIE — this thread is *finished* — and the same row
    // explains its unreadable stack as "running", then advises `suspend:true`, which cannot help: a
    // finished thread will never be suspendable. The `!suspended` branch phrases every unreadable stack
    // as a running one, and a churning pool is where that reading is wrong.
    assert!(
        section.contains("running — JDWP can only read a suspended thread's stack; pass suspend:true"),
        "a finished thread's row is currently explained as a running one — if that has been fixed, this \
         assertion is what should tell you:\n{section}"
    );

    // --- (2) finished AND collected: the id is gone, so the row is dropped ---
    let dropped = vanished_and_dropped.unwrap_or_else(|| {
        panic!(
            "twelve dumps and no thread id ever went stale mid-dump, so `collect_dump_rows`' dead-thread \
             arm still has not executed — the probe's collector is not reclaiming retired workers\n  \
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
    // FINDING, pinned. The only explanation offered is the caller's own `limit` — which was 500 against
    // ~63 threads, so raising it changes nothing and narrowing with `name_filter` changes nothing. The
    // two causes of a short dump (the limit, and threads that stopped existing) are reported with one
    // sentence that only describes the first.
    assert!(
        dropped.contains("(raise limit, or narrow with name_filter)"),
        "the shortfall is currently attributed to `limit` — pinned so that attributing it to the churn \
         instead flips this test rather than passing quietly:\n{}",
        head_of(&dropped)
    );

    // --- and the same pool, dumped with the VM frozen ---
    // Freezing right after `AllThreads` closes almost the whole window, which is why this half exists:
    // the states above are a property of reading a *live* list, not a fault the churn induces in the
    // dump. What is asserted here is coherence — every row it printed that is still a thread is one it
    // was actually holding — plus a real stack off the stable half and a VM that is running afterwards.
    let base = highest_tick(&probe).expect("no tick to count from");
    let frozen = server.call(
        "debug.thread_dump",
        serde_json::json!({"limit": 500, "suspend": true, "max_frames": 4}),
    );
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
    let dump = server.call(
        "debug.thread_dump",
        serde_json::json!({"limit": 200, "suspend": true, "monitors_only": true}),
    );
    assert_contains_all("the dump completed and resumed the VM", &dump, &["verified running", "Cost:"]);

    // The premise, checked before anything is concluded from it. If the contention had not formed, every
    // assertion below would pass vacuously on a dump with no `waiting to enter` lines in it at all.
    let waiting = dump.lines().filter(|l| l.contains("waiting to enter:")).count();
    assert_eq!(
        waiting, 48,
        "this test is about contention at scale, so all 48 waiters must be queued in the dump it \
         reads:\n{}",
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
    let wrong: Vec<&str> = dump
        .lines()
        .filter(|l| l.contains("waiting to enter:"))
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
/// The third is not fine, and it is pinned rather than fixed. The JVM names a lambda's hidden class
/// `SyntheticProbe$$Lambda/0x00007f…`, where the `/` separates the class from a JVM-assigned address and
/// is **not** a package separator. `decode_signature` replaces every `/` in a `L…;` signature with a
/// `.`, because in a JNI signature that is what a `/` is — so what a caller is shown is
/// `SyntheticProbe$$Lambda.0x00007f…`, a name the JVM does not use and will not answer to. It reads
/// exactly like a class `0x00007f…` in a package `SyntheticProbe$$Lambda`, which is the kind of wrongness
/// that gets acted on.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
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

    // --- the hidden classes, pinned exactly as they render ---
    // Three of them: the lambda, the method reference, and the one `new Thread(SyntheticProbe::worker)`
    // created. Each is a frame in its own right, between the ordinary frames on either side.
    let hidden: Vec<&str> = stack.lines().map(str::trim).filter(|l| l.contains("$$Lambda")).collect();
    assert_eq!(
        hidden.len(),
        3,
        "a lambda and a method reference each add a hidden-class frame of their own, and the thread's own \
         method reference adds a third:\n{stack}"
    );
    // THE FINDING. `/` is not a package separator here, and every one of these is rendered as though it
    // were. Both halves are asserted: what the caller IS shown, and that the JVM's own spelling never
    // appears — so a fix that stops mangling the name flips this test rather than passing quietly.
    for line in &hidden {
        assert!(
            line.contains("$$Lambda.0x"),
            "pinned: a hidden class currently renders with the JVM's `/` rewritten to `.`, which is not \
             its name — if that has been fixed, this is the assertion that should tell you: {line}"
        );
    }
    assert!(
        !stack.contains("$$Lambda/0x"),
        "pinned: the JVM's own spelling of a hidden class does not survive decode_signature:\n{stack}"
    );
    // And it has no line number, because a hidden class has no line table — so the one frame a reader
    // cannot act on is also the one with nothing to look up.
    let mangled = frame_line(&stack, "$$Lambda").expect("a hidden-class frame was found a moment ago");
    let after_run = mangled.split(".run").nth(1).unwrap_or("!");
    assert!(
        after_run.trim().is_empty(),
        "a hidden class has no line table, so its frame must carry no line: {mangled}"
    );

    // The mangled name is at least CONSISTENT — `debug.list_classes` spells it the same way, so a name
    // copied out of a stack finds the class again here. What it cannot do is leave this tool: the JVM's
    // `Class.getName()`, a `-verbose:class` line, a jstack dump and a JDWP class pattern all use the `/`.
    let by_mangled =
        server.call("debug.list_classes", serde_json::json!({"filter": "SyntheticProbe$$Lambda"}));
    assert!(
        by_mangled.contains("SyntheticProbe$$Lambda."),
        "the name a stack shows must at least find the class again in this tool:\n{by_mangled}"
    );
    let by_jvm_name =
        server.call("debug.list_classes", serde_json::json!({"filter": "SyntheticProbe$$Lambda/0x"}));
    assert!(
        by_jvm_name.starts_with("0/0 class(es)"),
        "pinned: searching for a hidden class by the name the JVM itself uses finds nothing, because the \
         listing is decoded exactly the way the stack is:\n{by_jvm_name}"
    );
    // …and the miss is then explained with the one reading that is definitely wrong. All three classes are
    // loaded — the listing above found them a moment ago under the other spelling — so "a class the JVM has
    // not loaded yet does not appear here" sends a caller looking for a code path that was never the
    // problem. CONTEXT.md's whole point about **loaded** is that "not loaded" and "no such class" are
    // indistinguishable from outside; this is a third case, and the tool cannot see it because it is the
    // one doing the renaming.
    assert!(
        by_jvm_name.contains("has not loaded yet"),
        "pinned: a name this tool mangled is reported as a class that may not be loaded:\n{by_jvm_name}"
    );

    // The dump renders frames through the same decoder, so the two views must not disagree — a caller who
    // reads one and pastes into the other has to get the same class.
    let dump = server.call(
        "debug.thread_dump",
        serde_json::json!({"name_filter": "synthetic-worker", "max_frames": 20}),
    );
    assert!(
        dump.contains("$$Lambda.0x") && !dump.contains("$$Lambda/0x"),
        "the dump and get_stack must spell a hidden class the same way:\n{dump}"
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
/// **The arrays are the half that matters, and not for symmetry.** `handlers.rs` renders a bare primitive
/// with its own private copy of the match (`render_primitive`); the copy in `types.rs` — `Value::format`,
/// the one that measured 16.67% — is reached only for ARRAY ELEMENTS and for the type-mismatch message.
/// A probe with eight primitive locals and no arrays would exercise the duplicate and leave the original
/// exactly as unmeasured as before, which is the kind of coverage that reports a number without having
/// looked.
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
    server.call(
        "debug.set_line_stop",
        serde_json::json!({"class_pattern": "PrimitiveProbe", "line": line}),
    );
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .unwrap_or_else(|| panic!("the breakpoint in PrimitiveProbe.work never fired"));

    let stack = server.call(
        "debug.get_stack",
        serde_json::json!({"max_frames": 1, "include_variables": true}),
    );

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
        ("cs", "sChars", "cs", "char[3]{(char) 'a', (char) 'Z', (char) '?'}"),
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

    // FINDING, pinned. `chars[2]` is `(char) 0xD800`, a lone surrogate — an ordinary thing to find in a
    // Java `char[]`, since a `char` is a UTF-16 code unit and not a Unicode scalar value. It is not
    // representable as a Rust `char`, so the renderer's `unwrap_or('?')` fires and it comes back as
    // `(char) '?'` — byte-identical to a real question mark, and there is nothing in the reply to tell
    // the two apart. The array above pins it; this says what it means.
    assert!(
        stack.contains("(char) 'Z', (char) '?'"),
        "a lone surrogate must be rendered somehow, and the current somehow is indistinguishable from a \
         literal '?':\n{stack}"
    );
    let real_question_mark = server.evaluate("PrimitiveProbe.sChars[1]");
    assert_eq!(
        evaluated(&real_question_mark),
        "(char) 'Z'",
        "sanity: element 1 is a Z, so the '?' above came from element 2 and not from a failed read"
    );

    // The other route into `Value::format`: the type-mismatch message renders the value that was refused.
    // Both object shapes go through it, and neither is reachable from an array of primitives.
    let null_into_int = server.call(
        "debug.set_value",
        serde_json::json!({"target": "PrimitiveProbe.sInt", "value": "null"}),
    );
    assert_contains_all(
        "null refused for an int, showing what was refused",
        &null_into_int,
        &["is declared int", "(object) null", "not assignable"],
    );
    let string_into_int = server.call(
        "debug.set_value",
        serde_json::json!({"target": "PrimitiveProbe.sInt", "value": "\"nope\""}),
    );
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
