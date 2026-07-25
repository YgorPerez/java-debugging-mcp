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

use common::{assert_contains_all, jdk_or_skip, probe_line, probe_source, Jdk, Probe, Server, EVENT_TIMEOUT};

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
        "debug.set_breakpoint",
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
            "debug.set_breakpoint",
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
        "debug.set_watchpoint",
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
        "debug.set_watchpoint",
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
        "debug.set_watchpoint",
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
        "debug.set_watchpoint",
        serde_json::json!({"class_name": "WatchProbe", "field_name": "counter", "modify": true, "access": true}),
    );
    assert_contains_all("modify+access makes two requests", &set, &["watch_modify_", "watch_access_"]);
    let listed = server.call("debug.list_breakpoints", serde_json::json!({}));
    assert_contains_all("listed", &listed, &["2 watchpoint(s)", "WatchProbe.counter"]);

    let one = set
        .split_whitespace()
        .find(|w| w.starts_with("watch_modify_"))
        .map(|w| w.trim_end_matches(',').to_string())
        .expect("no watch_modify_ id in set output");
    let cleared = server.call("debug.clear_breakpoint", serde_json::json!({"breakpoint_id": one}));
    assert_contains_all("cleared one", &cleared, &["Watchpoint cleared", "WatchProbe.counter"]);
    assert_contains_all(
        "the other survives",
        &server.call("debug.list_breakpoints", serde_json::json!({})),
        &["1 watchpoint(s)"],
    );
    assert_contains_all("panic reports watchpoints", &server.panic_reset(), &["watchpoint"]);
    assert_contains_all(
        "nothing left",
        &server.call("debug.list_breakpoints", serde_json::json!({})),
        &["No breakpoints set"],
    );

    // --- Argument validation.
    assert_contains_all(
        "unknown field",
        &server.call("debug.set_watchpoint", serde_json::json!({"class_name": "WatchProbe", "field_name": "nope"})),
        &["has no field 'nope'"],
    );
    assert_contains_all(
        "unloaded class",
        &server.call("debug.set_watchpoint", serde_json::json!({"class_name": "NoSuchClass", "field_name": "x"})),
        &["not loaded yet"],
    );
    assert_contains_all(
        "neither kind selected",
        &server.call(
            "debug.set_watchpoint",
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
        "debug.set_breakpoint",
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
        &server.call("debug.list_breakpoints", serde_json::json!({})),
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
            server.call("debug.list_breakpoints", serde_json::json!({}))
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
        &server.call("debug.list_breakpoints", serde_json::json!({})),
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

        server.call("debug.set_breakpoint", serde_json::json!({"class_pattern": "ForceProbe", "line": line}));
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
    server.call("debug.set_breakpoint", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
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
    server.call("debug.set_breakpoint", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
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
        "debug.set_breakpoint",
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
        "debug.set_exception_breakpoint",
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
        &server.call("debug.list_breakpoints", serde_json::json!({})),
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
        "debug.set_watchpoint",
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
        &server.call("debug.list_breakpoints", serde_json::json!({})),
        &["watch WatchProbe.counter", "(trace)"],
    );

    server.panic_reset();
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
        "debug.set_breakpoint",
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
        "debug.set_breakpoint",
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
        "debug.set_breakpoint",
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
        "debug.set_watchpoint",
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
        "debug.set_breakpoint",
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
    server.call("debug.set_breakpoint", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
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

    // The disarm is discoverable, in list_breakpoints and in the next get_last_event — and per BP-2 the
    // breakpoint is *disabled*, not deleted, so its definition survived and it can be re-armed.
    let listed = server.call("debug.list_breakpoints", serde_json::json!({}));
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
    server.call("debug.set_breakpoint", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
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
    let set = server.call("debug.set_breakpoint", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
    let bp_id = grab_token(&set, "bp_").expect("no bp id in set reply");
    server
        .wait_for_event(&format!("\"line\":{line}"), EVENT_TIMEOUT)
        .expect("breakpoint never fired");

    // Arm a step (sets pending_step), then clear the breakpoint so ONLY the step remains as the reason
    // the VM is suspended. Now the watchdog's step-clearing is the only thing that can free the probe.
    server.call("debug.step_over", serde_json::json!({}));
    server.call("debug.clear_breakpoint", serde_json::json!({"breakpoint_id": bp_id}));
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
    server.call("debug.set_breakpoint", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
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
        "debug.set_watchpoint",
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
    let listed = server.call("debug.list_breakpoints", serde_json::json!({}));
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
    let on = server.call("debug.toggle_breakpoint", serde_json::json!({"breakpoint_id": watch_id, "enabled": true}));
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
        "debug.set_exception_breakpoint",
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
    server.call("debug.set_breakpoint", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
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
    // holds at n == 3. Reaching the suspend at all proves both clauses were evaluated. Swap the plain
    // breakpoint (still suspended from the first hit) for the conditioned one WHILE suspended, then
    // resume so the condition is evaluated afresh on later calls.
    let listed = server.call("debug.list_breakpoints", serde_json::json!({}));
    let old_bp = grab_token(&listed, "bp_").expect("no bp to clear");
    server.call("debug.clear_breakpoint", serde_json::json!({"breakpoint_id": old_bp}));
    server.call(
        "debug.set_breakpoint",
        serde_json::json!({"class_pattern": "DeepProbe", "line": line, "condition": "n > 2 && local > 2"}),
    );
    server.call("debug.continue", serde_json::json!({}));
    server.call("debug.get_last_event", serde_json::json!({"drain": true})); // discard the first hit

    let hit = server
        .wait_for_event("\"event\":\"breakpoint\"", EVENT_TIMEOUT)
        .expect("compound condition never fired");
    assert!(hit.contains("[suspended] true"), "the compound condition should have suspended: {hit}");
    assert_contains_all("condition held at n == 3", &server.evaluate("n"), &["(int) 3"]);

    server.panic_reset();
}

/// BP-1: `toggle_breakpoint` silences and re-arms a breakpoint without losing its definition. Tested
/// on a trace breakpoint so the probe never freezes: disabled -> no new snapshots; re-enabled -> they
/// resume.
#[test]
#[ignore = "needs a JDK and a live JVM; run with --ignored"]
fn toggle_breakpoint_disables_and_rearms() {
    let Some(jdk) = jdk_or_skip("toggle_breakpoint_disables_and_rearms") else { return };
    let probe = Probe::launch(&jdk, "WatchProbe").expect("launch WatchProbe");
    let mut server = Server::start().expect("start server");
    server.attach(probe.port);

    probe.wait_for_line(EVENT_TIMEOUT, |l| tick_index(l).is_some()).expect("probe never ticked");
    let line = probe_line(&probe_source("WatchProbe"), "counter = counter + 1;");
    let set = server.call(
        "debug.set_breakpoint",
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
    let off = server.call("debug.toggle_breakpoint", serde_json::json!({"breakpoint_id": bp_id, "enabled": false}));
    assert_contains_all("disable keeps the definition", &off, &["Disabled", "re-arm"]);
    assert_contains_all(
        "a disabled breakpoint stays listed",
        &server.call("debug.list_breakpoints", serde_json::json!({})),
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
    let on = server.call("debug.toggle_breakpoint", serde_json::json!({"breakpoint_id": bp_id, "enabled": true}));
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
        &server.call("debug.list_breakpoints", serde_json::json!({})),
        &[&bp_id],
    );
    let again = server.call("debug.toggle_breakpoint", serde_json::json!({"breakpoint_id": bp_id, "enabled": false}));
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
    let note = server.call("debug.list_breakpoints", serde_json::json!({}));
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
    let set = server.call("debug.set_breakpoint", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
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

    let listed = server.call("debug.list_breakpoints", serde_json::json!({}));
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
    server.call("debug.set_breakpoint", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
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
        "debug.set_breakpoint",
        serde_json::json!({"class_pattern": "DeepProbe", "line": line, "condition": "order.getTotal() > 1"}),
    );
    assert_contains_all("an invoking condition is refused at arm time", &cond, &["Read-only", "condition"]);
    let texpr = server.call(
        "debug.set_watchpoint",
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
        "debug.set_breakpoint",
        serde_json::json!({"class_pattern": "com.example.NeverLoaded", "line": 10}),
    );
    assert_contains_all("the breakpoint is deferred", &set, &["Deferred", "bp_"]);
    let bp_id = grab_token(&set, "bp_").expect("no bp id in deferred reply");

    // It IS listed, so "not found" would be a lie.
    assert_contains_all(
        "a deferred breakpoint is listed",
        &server.call("debug.list_breakpoints", serde_json::json!({})),
        &[&bp_id],
    );

    let toggled = server.call("debug.toggle_breakpoint", serde_json::json!({"breakpoint_id": bp_id, "enabled": false}));
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
    server.call("debug.set_breakpoint", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
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
    server.call("debug.set_breakpoint", serde_json::json!({"class_pattern": "DeepProbe", "line": line}));
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
    let set = server.call("debug.set_breakpoint", serde_json::json!({"class_pattern": "WatchProbe", "line": line}));
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
        &server.call("debug.list_breakpoints", serde_json::json!({})),
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
        "debug.set_watchpoint",
        serde_json::json!({"class_name": "WatchProbe", "field_name": "counter", "trace": true}),
    );
    let watch_id = grab_token(&set, "watch_modify_").expect("no watch id");

    // Disable then re-arm: the re-arm must re-resolve WatchProbe.counter by name and fire again.
    server.call("debug.toggle_breakpoint", serde_json::json!({"breakpoint_id": watch_id, "enabled": false}));
    server.call("debug.get_traces", serde_json::json!({"clear": true}));
    let on = server.call("debug.toggle_breakpoint", serde_json::json!({"breakpoint_id": watch_id, "enabled": true}));
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
        "debug.set_breakpoint",
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
            server.call("debug.set_breakpoint", hit_once());
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
            server.call("debug.set_breakpoint", hit_once());
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
    // the watchdog leaves its note on `list_breakpoints` / `get_last_event`. After a disconnect there is
    // no session left to ask, so the inline reply is all there is.
    let said = if matches!(resume, Resume::Disconnect) {
        reply
    } else {
        format!("{reply}\n{}\n{}",
            server.call("debug.list_breakpoints", serde_json::json!({})),
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
