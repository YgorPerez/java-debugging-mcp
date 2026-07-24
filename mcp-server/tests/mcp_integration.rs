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
// Always scope to `--test mcp_integration`: a bare `cargo test -- --ignored` also un-ignores the
// illustrative ```ignore doctests in jdwp-client, which are not meant to compile.
//
// Without a JDK each test prints SKIP and passes, so a JDK-less CI stays green rather than red.

mod common;

use common::{assert_contains_all, jdk_or_skip, probe_line, probe_source, Probe, Server, EVENT_TIMEOUT};

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
    // A subscript in a write target used to be parsed and then dropped, writing the whole field.
    assert_contains_all(
        "a subscripted set_value target is refused, not silently widened",
        &server.call("debug.set_value", serde_json::json!({"target": "order.lines[0..2]", "value": "1"})),
        &["Subscripts aren't supported"],
    );
    assert_contains_all(
        "an indexed write is refused too",
        &server.call("debug.set_value", serde_json::json!({"target": "order.numbers[0]", "value": "1"})),
        &["Subscripts aren't supported"],
    );
    // A Map has no positional order, so slicing one points at the alternatives.
    assert_contains_all("slicing a Map explains itself", &server.evaluate("order.counts[0..1]"), &["is a Map"]);

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

