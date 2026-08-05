// Debug tools schema definitions
//
// Tool argument schemas are generated from the typed structs in `crate::args` (schemars), so the
// advertised schema always matches what the handler deserializes. Tools with no arguments use an
// empty object schema.

use crate::args::{
    AttachArgs, CheckStaleArgs, ClearBreakpointArgs, EvaluateArgs, EvaluateChainArgs, ForceReturnArgs,
    GetLastEventArgs, GetStackArgs, GetTracesArgs, LaunchArgs, ListClassesArgs, ListFieldsArgs,
    ListInstancesArgs, ListMethodsArgs, ListThreadsArgs, PopFrameArgs, ReloadClassArgs, ResumeThreadArgs,
    RunNamedQueryArgs, SetBreakpointArgs, SetExceptionBreakpointArgs, SetMethodBreakpointArgs,
    SetMonitorStopArgs, SetValueArgs, SetWatchpointArgs, SourceArgs, StepArgs, SuspendThreadArgs,
    ThreadDumpArgs, ToggleBreakpointArgs,
};
use crate::protocol::Tool;
use serde_json::json;

/// Convert a schemars-generated schema into the JSON value the MCP protocol carries, with `session_id`
/// added.
fn to_val(s: schemars::Schema) -> serde_json::Value {
    with_session_id(serde_json::to_value(s).unwrap_or_else(|_| json!({"type": "object", "properties": {}})))
}

/// Schema for a tool that takes no arguments of its own.
fn empty() -> serde_json::Value {
    with_session_id(json!({"type": "object", "properties": {}, "additionalProperties": false}))
}

/// What `session_id` does, written once for all thirty-eight tools.
///
/// **`concat!` of one-line pieces, not a `\` continuation.** A continuation reads better in source and does
/// not survive `cargo fmt`: it joined the pieces into a single literal and kept each line's indentation as
/// *content*, so v0.17.0 shipped this description with runs of thirty-three spaces in it, on every tool. A
/// description is the caller's documentation (`docs/toolkit-contract.md`, and DOC-7/#108 is the time that
/// mattered), so it is assembled from fragments that carry no leading whitespace to lose.
const SESSION_ID_DESC: &str = concat!(
    "Which debug session to act on, from `debug.list_sessions`. Omitted, the CURRENT session is used — ",
    "the most recently attached one — which is what every example here assumes. Give it when more than one ",
    "JVM is attached, because the reply of a tool that hit the wrong one looks entirely normal: that is the ",
    "whole reason a misspelling of this argument is now refused rather than ignored (DOC-9). ",
    "`debug.list_sessions` accepts it and ignores it, having nothing to select.",
);

/// Publish `session_id` as an argument of every tool (DOC-9, #132).
///
/// **It was an argument of all thirty-eight and a documented field of none.**
/// `RequestHandler::resolve_session` reads it from the raw arguments for every tool, so it is a typed field on
/// no `*Args` struct — see the NOTE in `args.rs` and the reason `crate::args::parse` strips it — and schemars
/// only publishes what it can see. The result was an argument that existed, worked, and appeared in no
/// `inputSchema`: invisible to a client that builds its calls from the schema, and absent from
/// `mcp-server/tests/argument-schemas.txt`, whose whole job is to make an argument change get read by
/// somebody. It was described in prose in two of the thirty-eight tool descriptions, which is not the same
/// thing as being published.
///
/// Injected in the one place every schema passes through rather than declared thirty times. The description is
/// written once here for the same reason.
///
/// `debug.list_sessions` accepts it and ignores it, as it always has — it lists every session, so there is
/// nothing for it to select. That is published rather than special-cased, because the schema's job is to say
/// what is accepted and an exception here would be a second rule to remember.
fn with_session_id(mut schema: serde_json::Value) -> serde_json::Value {
    let Some(object) = schema.as_object_mut() else { return schema };
    // `deny_unknown_fields` on every args struct makes schemars publish this, and the six tools with no
    // arguments of their own set it above. Stated for both, so the schema says that the field list is the
    // whole of what is accepted.
    object.insert("additionalProperties".to_string(), json!(false));
    if let Some(properties) = object.get_mut("properties").and_then(|p| p.as_object_mut()) {
        properties.insert(
            crate::args::SESSION_ID_ARG.to_string(),
            json!({
                "type": ["string", "null"],
                "default": null,
                "description": SESSION_ID_DESC
            }),
        );
    }
    schema
}

/// Every `debug.*` tool the server advertises.
///
/// Split into themed groups purely so each stays readable — the MCP client sees one flat list, in
/// the order assembled here.
pub fn get_tools() -> Vec<Tool> {
    let mut tools = session_tools();
    tools.extend(stop_point_tools());
    tools.extend(execution_tools());
    tools.extend(inspection_tools());
    tools
}

/// Connecting to and disconnecting from a JVM.
fn session_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "debug.attach".to_string(),
            description: "Open a debug session against a JVM that is ALREADY RUNNING with -agentlib:jdwp — host + port; nothing is launched for you, and the JVM must have been started with the agent before you got here. THE FIRST THING TO SETTLE IS WHOSE JVM THIS IS, because it decides what the rest of the session may safely do and nothing here can work it out for you: a deployed app server, a staging box, anything serving requests other people are waiting on is SHARED, and on a shared JVM every suspension freezes their in-flight requests too — so stop points should be trace:true only (they snapshot and resume, never freezing anything) and debug.pause / debug.step_* should be left alone unless you accept holding the whole VM. On a JVM that is yours alone — a local program, a container nobody else is hitting — suspending costs nothing and the steppers are the point. The port looks identical either way. Two guards exist because it is usually shared: read_only:true (or the JDWP_READONLY env var, which cannot be relaxed per-attach) refuses invocation, set_value, force_return and reload_class while leaving every read working, and a watchdog auto-resumes a VM left suspended after JDWP_WATCHDOG_SECS (default 120) — disabling the stop point that froze it, so the rescue can't be undone by the next hit. Optional source_roots / class_roots configure debug.source and debug.reload_class / debug.check_stale; each REPLACES its environment default for this session rather than adding to it. Concurrent sessions are supported: the newest becomes current, every tool takes an optional session_id, and debug.list_sessions finds one you lost. Attaching suspends nothing and changes nothing in the debuggee — though a JVM started with suspend=y is already frozen at startup waiting for you, and debug.continue is what releases it.".to_string(),
            input_schema: to_val(schemars::schema_for!(AttachArgs)),
        },
        Tool {
            name: "debug.launch".to_string(),
            description: "START a JVM under the debugger and attach to it, instead of attaching to one someone else started. Give main_class (+ classpath) or jar, plus optional jvm_args, args, working_dir and java_home; the -agentlib:jdwp argument and a free port are added for you, and the reply names the exact command and JDK it ran. THE REASON TO USE IT: suspend defaults to TRUE, so the JVM is held BEFORE ITS FIRST INSTRUCTION — static initialisers, framework bootstrap, anything that runs once during startup — which is unreachable when you attach, because by the time you can connect it has all already run. Arm your stop points, then debug.continue. THE JVM IS YOURS, which changes the advice on every other tool: no other requests are on it, so debug.pause, the steppers and suspending stop points cost nobody anything, and the shared-instance cautions elsewhere are about somebody else's JVM. THE LIFETIME IS THIS SERVER'S PROBLEM, and there are three things to know about it: debug.disconnect TERMINATES the JVM (pass detach_on_disconnect:true at launch to keep it running instead, after which nothing here tracks it); if this server is SIGKILLed the JVM survives as an orphan and the reply names its pid so you can kill it; and its stdout/stderr are CAPTURED rather than printed, because this server's stdout is the MCP transport — the last lines come back from debug.disconnect, and immediately if the JVM dies during startup, which is what turns \"could not connect\" into \"your classpath was wrong, here is what java said\". NOT for app servers: a long-running deployment should be started however it normally is and attached to with debug.attach.".to_string(),
            input_schema: to_val(schemars::schema_for!(LaunchArgs)),
        },
        Tool {
            name: "debug.list_sessions".to_string(),
            description: "List every live debug session — its host:port, whether it is the current one (all tools default to that), whether it is suspended, which threads it is holding one at a time with debug.suspend_thread and for how long (kept apart from SUSPENDED on purpose: that means the whole VM is stopped and nobody's requests are served, while a held worker leaves the JVM serving — and the remedies differ, debug.resume_thread against debug.continue), how many stop points/traces/events it holds, and how many JDWP packets it has cost. Use it when you have lost a session_id, or to check what is still attached before walking away. A session whose JVM has gone is shown as DEAD. A session that has hot-reloaded a class is flagged with the count, deliberately regardless of whose session it is: a session someone ELSE left behind is the case that matters, and this listing is the only place a third party can discover that a JVM is running installed bytecode.".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.disconnect".to_string(),
            description: "Disconnect from a JVM debug session, leaving the JVM RUNNING with nothing armed: it clears every event request and resumes every thread in one round trip before dropping the session, so disconnecting while suspended at a breakpoint cannot freeze the debuggee forever (SAFE-1). Reports whether the VM had been suspended, names any thread this session was holding with debug.suspend_thread (Dispose resumes thread-level suspends as many times as necessary, so those go too), and names any class this session installed with debug.reload_class — that outlives the session and only a redeploy restores it, so this is the last moment anyone is told.".to_string(),
            input_schema: empty(),
        },
    ]
}

/// The things that can stop the VM — line breakpoints, exception breakpoints, field watchpoints —
/// plus listing/clearing them and the panic escape hatch that drops them all.
fn stop_point_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "debug.set_line_stop".to_string(),
            description: "Set a breakpoint at a location (class_pattern + line and/or method). class_pattern takes AN EXACT CLASS, A WILDCARD, OR A LIST OF EITHER. An exact name arms one breakpoint (deferred on a CLASS_PREPARE watch if the class isn't loaded yet, arming itself when it loads). A wildcard (com.example.*, *.OrderService, *Order*) requires `method` and REFUSES `line` — :412 is a different statement in every class it matches — and arms one breakpoint per matching loaded class, each with its own bp_ id, PLUS a watch that arms matching classes loading later: the answer to \"break at the entry of handle on every implementation of this interface\" or on a generated proxy whose exact name you cannot predict. The family is addressable as one bpset_ id, so debug.clear_stop_point / debug.toggle_stop_point can drop or silence all of it — including the watch — in one call, and the individual bp_ ids still work on their own. Bounded by max_classes (default 20): the reply says how many classes it armed and what it left out, because that count is the one thing a wildcard hides from you. The cap bounds the COST as well as the count — a family that is full parks its class-load watch instead of paying for an event on every class load it could only refuse, and the reply says when that has happened; clearing a member frees a slot and it starts watching again by itself. A list (['com.example.Order', 'com.example.*Repo']) resolves each entry independently and reports every entry's outcome — 2 armed, 1 deferred, 1 refused is a normal batch result, so nothing is aborted by one entry failing. PASS condition TO STOP ONLY WHEN SOMETHING IS TRUE, and note what it now costs: a hit whose condition is FALSE holds only the thread that hit it, evaluates the expression there, and lets it go — every other thread keeps running, so a conditional stop point is cheap on a hot line on a shared instance rather than the most expensive thing you can arm. The trade is at the other end and no reply can show it to you: when the condition DOES hold, the debugger suspends the rest of the VM at that moment, and the other threads run on for the one round trip that takes — so the state you read is the state just AFTER the hit, not at the instant of it. The hit frame itself is exactly as the condition saw it; what it points at may have moved. A stop point with no condition has no such gap, because the JVM freezes everything before it says anything. If that suspend fails, debug.get_last_event reports BOTH facts — that the condition matched and whether the application is still running — instead of either half alone. condition does not compose with hit_count the way it reads: hit_count counts hits, not matches, so hit_count:5 checks the condition on the 5th hit and is then spent whatever the answer. Pass trace:true to make it a non-suspending logpoint that snapshots and resumes instead of freezing the thread — the right choice on the shared 8180; read snapshots with debug.get_traces. It does not FREEZE the VM, which is not the same as not slowing it: capture is serialised, so a traced stop point tops out at ~720 hits/s (~1160 with trace_frames:0) and hits past that queue. Under a few hundred hits/s that is nearly free; trace_max_hits (default 200) keeps even a hot line to a sub-second blip. A traced hit also records the calling chain (trace_frames, default 3) as class.method:line, so you can see which path reached it. Captured values are truncated AT CAPTURE TIME — 100 chars per in-scope local, 200 for the trace_expr result — and the cut string is what the buffer stores, so debug.get_traces can never recover the rest; raise both with trace_max_length (ceiling 4000) when the thing you are tracing is a JSON body, a SOAP envelope or a built SQL string. A request above the ceiling is clamped and the reply says so. TRACE_EXPR TAKES A LIST AS WELL AS A STRING, and it is how you see a DISAGREEMENT rather than a value: [\"tenant.getIdentificador()\", \"sessao.getNmSchema()\"] records both against the SAME hit, which two stop points on the line cannot do — they record into two independently budgeted streams you then have to join by hand. Each element is evaluated in turn against the same frame and gets its own numbered slot (Trace expr[0], Trace expr[1]), so one that errors leaves the others intact — a chain going null on some hits and not others is the normal case. ONE STRING IS UNCHANGED, down to the rendering. Capped at 4 expressions, because each is a full resolution inside the capture window and capture is serialised, so they divide the same throughput budget trace_max_hits is charged against; over the cap it keeps the first 4 and the reply names what it dropped. debug.list_stop_points then reports what this stop point is actually costing on this JVM, so you need not take the ~720 figure on trust. THE NON-SUSPENDING PROMISE IS NOT THIS STOP POINT'S TO KEEP, AND THE REPLY NOW SAYS WHEN IT IS BROKEN: a JDWP composite carries ONE suspend policy for the whole event set — the strongest any member asked for — so a SUSPENDING stop point at the same bytecode location turns every trace:true stop point there into a VM-freezing one, on every hit (measured identical on Temurin 17/21/25). Arming either way round is ACCEPTED rather than refused, because suspending on a line you are already tracing is a legitimate thing to want; what changes is that the reply names the stop points whose behaviour just changed — in both directions, so a trace armed onto an already-suspending line is told it will not be cheap — and debug.list_stop_points marks them `(trace — SUSPEND POLICY OVERRIDDEN)` rather than a bare `(trace)`. Clearing the suspending stop point restores snapshot-and-resume. Two traced stop points on one line are unaffected, since EventThread plus EventThread is still EventThread. STALE BYTECODE IS REPORTED UNASKED: if a class root is configured (debug.attach {class_roots}, JDWP_CLASS_ROOTS) the reply appends a warning when the JVM's line table for the method you just armed does not match your compiled .class — the case where a breakpoint at :412 resolves against last week's build and then never fires, or fires with locals that make no sense for the code you are reading, which is indistinguishable from a wrong hypothesis. It speaks ONLY when it has a proof: no class root, no class file, or no line table on either side all stay silent rather than guessing, and a silent reply is not a claim that your build is current — ask debug.check_stale for that. Costs no extra JDWP packets, because arming already read the line table. ONE SOURCE LINE CAN BE SEVERAL BYTECODE LOCATIONS, AND ALL OF THEM ARE ARMED: javac does not compile a finally body once and jump to it, it inlines a copy per exit path, so a line inside a finally is in the line table twice — once for normal completion and once for the exception path. Arming only the first (which is what this did before) means the stop point reports the calls that SUCCEEDED and goes silent on the one that failed, which is the case you are debugging and is indistinguishable from the code never running. A finally is the idiomatic logpoint site precisely because the request and the response are both still in scope on both paths. The reply and debug.list_stop_points now say `Armed at N locations` whenever N > 1, and a line that maps to exactly one location reads exactly as it always has. It stays ONE stop point: one bp_ id, listed once, cleared or toggled once, and a traced hit charges trace_max_hits once per hit rather than once per armed location. THE SAME IS TRUE OF CLASSLOADERS: an exact class name is not unique in a JVM, because every classloader that has loaded it defines its own type with its own statics, and on WildFly a library packed into more than one deployment's WEB-INF/lib is genuinely loaded once per war. All of those copies are armed, the reply says `Armed on N classloaders`, and debug.list_stop_points names each loader so a read can be pinned to one. Arming a single copy is how a stop point reported \"armed\" and then never fired, which is indistinguishable from a wrong hypothesis about the code path. AND IT KEEPS WATCHING: an exact class name holds a class-load watch for the stop point's whole life, so a copy of that class loaded LATER — under a new module classloader, which is exactly what a redeploy produces — is armed into the SAME bp_ id with no re-arm from you. Before this it watched once and stopped, so the loop `set breakpoint, edit, recompile, redeploy, fire the request` went quiet: the stop point stayed enabled, stayed listed, and watched the retired deployment's copy, and an empty debug.get_traces reads as \"the code path I predicted is not the one running\" rather than as a stale arm. debug.list_stop_points keeps the two facts apart — how many copies are armed NOW, how many have loaded since you armed it, and whether it is still watching — because `Armed on 4 classloaders` alone cannot tell a library packed into four wars from three redeploys of one. A stop point that is NOT watching says so. PASS instance_id TO SCOPE THE STOP POINT TO ONE OBJECT — the @0x… handle any reply prints, from a trace snapshot's locals, an expanded field tree or debug.list_instances. This is JDWP's InstanceOnly modifier, so the JVM does the matching: a hit on any other instance costs no packet, no snapshot and no suspension at all, which is what makes \"trace salvar() on THIS Reserva, not all 400 in flight\" cheap on a shared instance. That is the difference from condition, which we evaluate on our side and therefore pay for on EVERY hit. REFUSED IN TWO CASES, BOTH BECAUSE THE ALTERNATIVE IS A SILENT LIE: on a STATIC method, which has no `this` to match — HotSpot accepts the modifier there and then fires for every hit, with no error and a reply claiming the stop point is scoped (measured on Temurin 17/21/25); and on a class that is NOT LOADED yet, because a deferred stop point cannot be checked for the first case and no instance of an unfetched class can exist anyway, so the handle would belong to something else. THE HANDLE IS A WEAK REFERENCE (ADR-0022): if the debuggee collects the object the filter simply stops matching and the stop point goes quiet, which is indistinguishable from the code never running — debug.list_stop_points checks for that and says so.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetBreakpointArgs)),
        },
        Tool {
            name: "debug.set_exception_stop".to_string(),
            description: "Break when an exception is thrown. Give class_pattern (e.g. java.lang.NullPointerException or a custom ErrorException) to target one type + its subclasses — ideal for silent-catch bugs where a swallowed exception hides the failure. Omit class_pattern to catch ALL exceptions (noisy). class_pattern also takes a WILDCARD (*.ValidationException) or a LIST (['java.lang.IllegalStateException', '*.TimeoutException']), arming one exc_ per resolved class and reporting each — bounded by max_classes. NOTE what a wildcard can and cannot do here: an exception request needs a concrete reference type (JDWP has no ClassMatch for this event kind, which is also why none of these can be deferred), so a wildcard matches only classes LOADED NOW and nothing arms itself later. An exception class the JVM has not needed yet is invisible to it — trigger it once, then arm. caught/uncaught select which throws to report. The hit is reported via debug.get_last_event with the exception type, its message, and the throw/catch location. The message is often the whole answer: on JDK 15+ a NullPointerException says which subexpression was null (\"because the return value of X.getY() is null\"), which is what you would otherwise bisect by hand with debug.evaluate. Reported in trace mode too — normally read straight off the exception with no invocation, and for a plain java.lang.NullPointerException (whose message the JVM computes on demand and never stores) by one bounded getMessage() call, which is the JDK's own native computation and runs no application code. Pass trace:true to collect throws WITHOUT suspending — required on a shared instance, where the default freezes every thread on each throw; read them with debug.get_traces. Not suspending is not the same as not costing anything: capture is serialised at ~720 hits/s (~1160 with trace_frames:0), so a throw site firing thousands of times a second gets throttled, and trace_max_hits:0 makes that sustained rather than a blip. A traced throw also records the calling chain (trace_frames, default 3), which is usually the actual question for a swallowed exception: which request path reached the catch. Captured values are truncated AT CAPTURE TIME — 100 chars per in-scope local, 200 for the trace_expr result — and the cut string is what the buffer stores, so debug.get_traces can never recover the rest; raise both with trace_max_length (ceiling 4000). A request above the ceiling is clamped and the reply says so. TRACE_EXPR TAKES A LIST AS WELL AS A STRING, and it is how you see a DISAGREEMENT rather than a value: [\"tenant.getIdentificador()\", \"sessao.getNmSchema()\"] records both against the SAME hit, which two stop points on the line cannot do — they record into two independently budgeted streams you then have to join by hand. Each element is evaluated in turn against the same frame and gets its own numbered slot (Trace expr[0], Trace expr[1]), so one that errors leaves the others intact — a chain going null on some hits and not others is the normal case. ONE STRING IS UNCHANGED, down to the rendering. Capped at 4 expressions, because each is a full resolution inside the capture window and capture is serialised, so they divide the same throughput budget trace_max_hits is charged against; over the cap it keeps the first 4 and the reply names what it dropped. On a framework that rethrows — an EJB interceptor chain, a Spring proxy — one exception instance throws many times, so those sightings are FOLDED: the original throw and the point where it escapes are both kept, the layers between become a `↻ rethrow of #<seq> (+N collapsed)` note on the escaping record, and a collapsed rethrow does not spend trace_max_hits. Without that a budget of 30 was gone on one instance walking WildFly's interceptors, and the only informative record was the 9th. debug.list_stop_points reports the cost this request is actually incurring once throws have landed. PASS hit_count TO FIRE ONLY ON THE Nth THROW, and read that literally: it is JDWP's Count modifier, so the JVM reports the Nth occurrence and then DELETES THE REQUEST ITSELF. The stop point fires once and is gone — debug.list_stop_points shows it SPENT rather than armed, debug.clear_stop_point on it sends nothing to the debuggee and says so, and debug.toggle_stop_point is what re-arms it with the same count. It is NOT \"the first N\": that is trace_max_hits, counted on this side, and combining the two gives you ONE snapshot rather than trace_max_hits of them — the arm reply says so instead of reporting two numbers that cannot both apply. The retry loop is what it is for: a supplier consulta retried after a sleep, where the SECOND attempt is the interesting one and the first failing is expected. PASS instance_id TO SCOPE THE STOP POINT TO THROWS FROM ONE OBJECT — the @0x… handle any reply prints. This is JDWP's InstanceOnly modifier and the JVM applies it, so a throw from any other instance costs nothing at all on either side; it matches the `this` of the frame that threw, so it only narrows throws from an INSTANCE method. Measured working on Temurin 17/21/25 against two instances throwing the same type from the same line — this is the one stop-point kind where HotSpot both accepts AND applies the modifier, which is why the other kinds refuse it rather than offering it (FILT-9, ADR-0027). THE HANDLE IS A WEAK REFERENCE (ADR-0022): if the debuggee collects the object the filter stops matching and the stop point goes quiet, which is indistinguishable from the exception never being thrown — debug.list_stop_points checks and says so. PASS condition TO FILTER THE THROWS YOU DO NOT WANT (FILT-6), and note what the exception instance is called: it is reachable as `exception`, so exception.cdException != 42 reads a field the frame cannot give you — the hit's top frame belongs to the THROWING method, so `this` is the thrower. That matters here because a type used for validation control flow throws far more often than it fails, and its message is often null (a constructor that calls no super(...) never sets one), which leaves the exception's own field as the only usable discriminator. `!`, `&&` and `||` all work. A condition-skipped hit is NOT charged to the trace budget, so a budget of N still means N matches rather than N throws. On a SUSPENDING exception stop the VM is frozen while the condition is evaluated; on a traced one only the hit thread is held, which is what makes this the cheap way to filter an exception trace on a shared instance.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetExceptionBreakpointArgs)),
        },
        Tool {
            name: "debug.set_field_stop".to_string(),
            description: "Break when a field is read or written — answers \"who mutates this?\" for a field that changes behind your back (a config flag, an id, a status). Give class_name + field_name; modify:true (default) breaks on writes and reports the mutating location with old → new value, access:true also breaks on reads (noisy). The class must already be loaded — watchpoints can't be deferred. class_name also takes a WILDCARD (com.example.*) or a LIST, arming one watch per matching loaded class that actually HAS the field; a class that matches but declares no such field is reported, not treated as an error, since that is the expected majority for a broad pattern. Bounded by max_classes, and keep it narrow for a reason stronger than noise: a watched field cannot be JIT-optimised, so a wildcard de-optimises the field in EVERY class it armed. Hits come back via debug.get_last_event; pass trace:true to collect them WITHOUT suspending (required on a shared instance) and read them with debug.get_traces — non-suspending, but still ~720 captures/s at most, so a field written thousands of times a second will be throttled unless trace_max_hits (default 200) stops it first. A traced hit also records the calling chain (trace_frames, default 3), so \"who mutates this?\" is answered with the path that got there, not just the innermost setter. Captured values are truncated AT CAPTURE TIME — 100 chars per in-scope local, 200 for the old → new pair and the trace_expr result — and the cut string is what the buffer stores, so debug.get_traces can never recover the rest; raise all of them with trace_max_length (ceiling 4000), which is what you need when the watched field holds a payload rather than a flag. A request above the ceiling is clamped and the reply says so. TRACE_EXPR TAKES A LIST AS WELL AS A STRING, and it is how you see a DISAGREEMENT rather than a value: [\"tenant.getIdentificador()\", \"sessao.getNmSchema()\"] records both against the SAME hit, which two stop points on the line cannot do — they record into two independently budgeted streams you then have to join by hand. Each element is evaluated in turn against the same frame and gets its own numbered slot (Trace expr[0], Trace expr[1]), so one that errors leaves the others intact — a chain going null on some hits and not others is the normal case. ONE STRING IS UNCHANGED, down to the rendering. Capped at 4 expressions, because each is a full resolution inside the capture window and capture is serialised, so they divide the same throughput budget trace_max_hits is charged against; over the cap it keeps the first 4 and the reply names what it dropped. debug.list_stop_points reports what the watch is actually costing once hits have landed. A watched field can't be JIT-optimised, so clear it when done. PASS hit_count TO FIRE ONLY ON THE Nth TOUCH, with JDWP's Count semantics: the JVM reports the Nth write (or read) and then deletes the request itself, so the watch fires ONCE and is then SPENT — debug.list_stop_points says so rather than listing it as armed, clearing it sends nothing to the debuggee, and debug.toggle_stop_point re-arms it with the same count. Not \"the first N\", which is trace_max_hits; the two together yield one snapshot, and the arm reply says so. PASS instance_id TO WATCH ONE OBJECT'S COPY OF THE FIELD — the @0x… handle any reply prints. This is JDWP's InstanceOnly modifier, so the JVM does the matching and a write to any other instance's copy costs no packet and no snapshot: the way to answer \"who is clearing THIS session's total?\" on an instance where 400 of them are live. REFUSED ON A STATIC FIELD, because a static write has no `this` for the modifier to match — and HotSpot accepts it anyway and then reports every write, with no error and a reply claiming the watch is scoped (measured on Temurin 17/21/25). A static field is one copy in the whole JVM, so there is nothing to narrow to; watch it unfiltered and use trace_expr or a condition. THE HANDLE IS A WEAK REFERENCE (ADR-0022): if the debuggee collects the object the watch goes quiet, which reads as the field never being touched — debug.list_stop_points checks and says so. PASS condition TO FILTER THE WRITES YOU DO NOT WANT (FILT-6). The INCOMING value is reachable as `newValue` — newValue == 999, or newValue.getStatus() for a reference field — which reading the field cannot give you, because a FIELD_MODIFICATION event is reported BEFORE the write lands. There is deliberately no oldValue: at condition time the field still holds it, so its own name reads it, and status != newValue asks whether this write actually changes anything. `!`, `&&` and `||` all work. A condition-skipped hit is NOT charged to the trace budget. On a SUSPENDING watch the VM is frozen while the condition is evaluated.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetWatchpointArgs)),
        },
        Tool {
            name: "debug.set_method_exit_stop".to_string(),
            description: "Report what a method actually RETURNED, without having to guess which `return` statement runs — for a method with several returns, or one whose value comes from a chain you can't easily break on. Each hit gives the return site (so you know which path was taken) plus the returned value. Give class_pattern + method; the method filter is applied our side because JDWP has no method-name modifier, so omitting it reports every method of the class. class_pattern takes a leading/trailing * and now also a LIST (['*.OrderService', '*.PaymentService']), arming one mexit_ per pattern. A wildcard costs nothing extra here and needs no expansion — the JVM does the matching, so one request covers every class the pattern matches, including classes that load later. That is why this tool had pattern support when the others did not. UNLIKE the other stop points, trace defaults to TRUE: a suspending method exit on a hot method is the fastest way to freeze a shared JVM, so trace:false is refused unless you name one concrete class AND one method. Read hits with debug.get_traces (trace) or debug.get_last_event (suspending). The returned value is truncated AT CAPTURE TIME at 200 chars (100 for each in-scope local), and the cut string is what the buffer stores, so debug.get_traces can never recover the rest — raise both with trace_max_length (ceiling 4000) when the method returns a JSON/XML payload rather than a status. A request above the ceiling is clamped and the reply says so. TRACE_EXPR TAKES A LIST AS WELL AS A STRING, and it is how you see a DISAGREEMENT rather than a value: [\"tenant.getIdentificador()\", \"sessao.getNmSchema()\"] records both against the SAME hit, which two stop points on the line cannot do — they record into two independently budgeted streams you then have to join by hand. Each element is evaluated in turn against the same frame and gets its own numbered slot (Trace expr[0], Trace expr[1]), so one that errors leaves the others intact — a chain going null on some hits and not others is the normal case. ONE STRING IS UNCHANGED, down to the rendering. Capped at 4 expressions, because each is a full resolution inside the capture window and capture is serialised, so they divide the same throughput budget trace_max_hits is charged against; over the cap it keeps the first 4 and the reply names what it dropped. Composes with thread_id, trace_max_hits and trace_frames; debug.list_stop_points reports what a traced request is actually costing once hits have landed. A JVM below JDWP 1.6 degrades to the return site without the value, and says so. hit_count IS ACCEPTED ONLY WITHOUT method, AND THE REFUSAL IS THE HONEST ANSWER RATHER THAN A MISSING FEATURE. JDWP applies Count to the REQUEST, and this request is a ClassMatch firing for every method of the class; `method` is filtered on our side afterwards. So hit_count:3 with method:\"save\" would ask the JVM for the 3rd exit of ANY method of the class — almost certainly a getter — which we then drop as the wrong method, leaving a stop point that reported nothing and that the JVM has already deleted. There is no way to make it mean what it reads like, so it is refused with that explanation. WITHOUT method it means exactly what it says: the Nth return out of the class, whichever method produced it, after which the request is gone and debug.list_stop_points reports it SPENT. PASS exclude_classes TO STOP THE EVENTS BEING GENERATED AT ALL, which is what makes a WILDCARD class_pattern usable here: the JVM does the ClassMatch, so a broad pattern sweeps in every proxy and interceptor the container generates and each unwanted exit costs a real event before the method-name filter on this side can discard it. A ClassExclude is applied by the JVM instead. There is deliberately NO default set here, unlike the stepping tools — a method-exit class_pattern is something you wrote, and silently subtracting from it would answer a different question from the one you asked. Measured: a request accepts at least 5000 exclusion patterns, so the list is not a resource to ration. instance_id IS ACCEPTED AND ALWAYS REFUSED HERE, AND THE REFUSAL IS THE ANSWER RATHER THAN A MISSING FEATURE. A method exit HAS a `this`, so nothing about the request looks wrong — HotSpot accepts an InstanceOnly modifier on it, replies success, and then records exits from every instance regardless (measured on Temurin 17/21/25, re-run on its own to be sure: FILT-9, ADR-0027). There is no signal of any kind that the scope was dropped, so passing it through would mean a reply saying the stop point is scoped to one object while it reports all 400 — which is the exact failure this server is built to refuse. Use a condition instead (evaluated on our side, so it works on every kind), or put the stop point on a LINE inside an instance method, where instance_id does work. PASS condition TO FILTER THE RETURNS YOU DO NOT WANT (FILT-6), evaluated on the returning method's own frame — so its locals and `this` are in scope. `!`, `&&` and `||` all work, and a condition-skipped hit is NOT charged to the trace budget. Two costs worth knowing: on a SUSPENDING request the VM is frozen while the condition is evaluated, and this request receives EVERY method of a matching class, so a condition on a broad class_pattern is evaluated far more often than it fires — a method filter is the cheaper narrowing.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetMethodBreakpointArgs)),
        },
        Tool {
            name: "debug.set_monitor_stop".to_string(),
            description: "Watch LOCK CONTENTION as it happens, WITHOUT SUSPENDING ANYTHING — the answer to \"requests are hanging on a lock\" on an instance other people are using. Until this existed that question was the one thing that forced a freeze of a shared JVM: debug.thread_dump cannot read a running thread's monitors, so it needs suspend:true, and a dump is one instant rather than a stream. This arms JDWP's monitor events instead, at event-thread policy, and snapshots each one into the trace buffer. KINDS ARE TWO PAIRS, NOT FOUR THINGS. blocked = a thread started waiting for a lock somebody else owns; acquired = that thread got it; wait = a thread entered Object.wait(), which RELEASES the lock; waited = its wait() returned. Omitting kinds arms [\"blocked\",\"acquired\"], the contended pair, which is what a wedged server is asked about — blocking is involuntary and a long one is a fault, while wait() is voluntary and a long one is often just an idle worker. Pass kinds:[\"all\"] for every kind. THE DURATION IS MEASURED BY THIS SERVER AND EVERY REPLY SAYS SO, because NO MONITOR EVENT CARRIES ONE: MONITOR_CONTENDED_ENTERED reports that a thread got the lock and nothing about how long it waited, and MONITOR_WAIT's timeout is the number the caller PASSED to wait(…), not what it got — a wait(5000) that returns in 3ms still reports 5000. So \"blocked for 4200ms\" is computed here, between the two events of a pair, and it includes this server's own capture latency (~0.86ms per hit before caller frames). Trustworthy at the multi-second scale a wedged lock shows; noisy below ~10ms. ARMING ONE HALF OF A PAIR IS LEGAL AND CHEAPER — one request instead of two, answering \"is anything blocking at all\" — and its snapshots say the duration was not measurable rather than printing a zero. VOLUME IS THE REAL RISK AND IT IS UNLIKE ANY OTHER STOP POINT. An uncontended lock produces nothing at all, but a hot contended one produces two events per acquisition, and contention is not a site you chose — it is wherever threads happen to collide, INCLUDING inside the JDK (a seven-thread probe produced 434 events in 3 seconds, with a ReferenceQueue$Lock among them). Capture is serialised at roughly 720 hits/s with the default 3 caller frames, so this is the easiest way yet to reach that ceiling; trace_max_hits (default 200) is what stops it, and 0 removes the protection. PASS thread_id — IT IS THE ONLY FILTER THAT REDUCES DEBUGGEE COST, applied inside the JVM, so a non-matching event costs no packet and no capture. PASS min_duration_ms TO SEE ONLY THE BLOCKS THAT HURT, and read what it is honestly: JDWP has no duration modifier, so the event has already been generated, has already cost the debuggee its notification and has already arrived here — it filters what you READ. It needs both halves of the pair (there is nothing to measure otherwise, and it is refused rather than silently arming a stop point that can never record), and it changes what the opening event does: that event stops producing snapshots and becomes pure timestamping, because at that instant nothing has elapsed to compare — otherwise the whole hit budget would go on \"started blocking\" lines. debug.list_stop_points still counts every hit, so Hits: 900 with no snapshots means \"contended constantly, never for that long\", which is a different finding from Hits: 0. monitor_class IS ACCEPTED ONLY WITH wait/waited AND REFUSED ON blocked/acquired, and the asymmetry is JDWP's rather than this tool's. The spec defines ClassOnly per event kind, and the monitor reading applies only to the wait pair; on the contended pair HotSpot tests the class of the CODE THAT BLOCKED instead. Measured on Temurin 11.0.32 over 3s windows: a ClassOnly naming the lock's type gave 0 events on blocked and 74 on wait, one naming the blocking code's class gave 45 on blocked and 0 on wait. So it is refused where it would not mean what it reads like, rather than passed through under a reply claiming the stop point is scoped to a lock type. instance_id IS ACCEPTED AND ALWAYS REFUSED, for the same reason it is on method exits: InstanceOnly tests the frame's `this`, which is NOT the monitor, and HotSpot accepts it and ignores it anyway — measured against a probe whose every frame is static, so nothing could legitimately match, and the request still reported all three of its locks. THERE IS DELIBERATELY NO condition ON THIS KIND, and it is a safety decision rather than an omission: a condition is evaluated on the hit thread, and a thread suspended at a monitorenter is blocked on the very lock in the snapshot — an expression that invokes anything needing that monitor cannot complete, so the debugger would wedge the thread it is reporting on. min_duration_ms is this kind's filter and it needs nothing from the debuggee. AND AN INVOKING trace_expr IS NOW REFUSED ON blocked FOR THE SAME REASON, because a caution was not enough: a getter that reads a field under synchronized looks exactly like one that does not. blocked is the ONE of the four kinds where the hit thread does not own the monitor its own snapshot names — at acquired it has just entered, at wait it still holds the lock (Java requires that to call wait() at all), and at waited it has re-acquired it, all measured rather than assumed. Measured too: the cost of being wrong is not the two-second budget but a thread suspended for ever — the call completes when the lock is finally released and the JVM re-suspends that thread AT THAT MOMENT, 1.2s after this server gave up and resumed it, and nothing clears it, because the watchdog resumes a suspended VM and the VM is running. FIELD READS ARE ACCEPTED EVERYWHERE and invoking is accepted on the other three kinds. What no arming check can see is an expression naming a DIFFERENT lock, which can stall on any kind — that one is reported by the timeout message when it happens. trace DEFAULTS TO TRUE AND trace:false REQUIRES A thread_id. A suspending monitor stop is the most dangerous thing this server can arm — every other kind has a line, class, method or field to narrow it to because you chose where it fires, and contention gives you nothing to narrow to, so a VM-wide freeze lands on the next acquisition of any hot lock and can re-fire the instant you resume. Snapshots also carry the CALLER CHAIN (trace_frames, default 3), which is usually the actual answer: the same synchronized block is entered from every request path, so the lock and the line rarely identify the problem while the path does. A JVM without canRequestMonitorEvents is told so in a sentence that names the fallback, not a bare NOT_IMPLEMENTED.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetMonitorStopArgs)),
        },
        Tool {
            name: "debug.list_stop_points".to_string(),
            description: "List all active breakpoints, including deferred ones, exception breakpoints, field watchpoints, and wildcard families (bpset_…). A family's line is the only place you can learn what a wildcard has BECOME since you armed it: how many breakpoints it holds now, which classes it armed after the reply you read, and whether it has stopped taking new ones because it is full at max_classes — a full family also PARKS its class-load watch, so it costs the JVM nothing while it is full and starts watching again by itself the moment you clear a member. The line distinguishes all four watch states, because they answer \"will this catch the class my next deployment generates?\" differently: watching (yes), parked because full (not until a slot frees), disabled (not until you re-arm the family), and could-not-be-registered (never). Each traced (non-suspending) stop point also reports what it has ACTUALLY cost: the mean capture per hit, the rate hits are arriving at, and the share of the window spent capturing — measured on your JVM rather than taken from the ~720 hits/s figure in the other tools' descriptions, which is 1/mean and so recoverable from the mean reported here. Call it after arming a trace on a hot site to find out whether it is hurting the instance. A traced stop point that has captured nothing says so explicitly rather than reporting zero. A TRACED STOP POINT THAT IS IN FACT FREEZING THE VM IS MARKED `(trace — SUSPEND POLICY OVERRIDDEN)` AND NOT A BARE `(trace)`, with a line naming what escalated it: a JDWP composite carries one suspend policy for the whole event set, the strongest any member asked for, so a suspending stop point at the same bytecode location revokes the non-suspending promise of every trace there. This is the place to look when the answer to \"why did the VM freeze?\" is a stop point you thought was cheap. EVERY stop point reports Hits: N — how many times the JVM has reported it firing — AND ZERO IS PRINTED RATHER THAN OMITTED, because \"armed, no Hits line\" would read as \"this code never ran\" and that is the reading this number exists to remove. It counts HITS, not the hits you were told about: a hit whose condition was FALSE still counts (so `Hits: 400` beside a condition that never matched is a different diagnosis from `Hits: 0`, and they used to look identical), a rethrow of an already-captured exception counts although it does not spend trace_max_hits (so on a traced stop point Hits and the capture count answer different questions rather than repeating one), and a method-exit request counts only exits of the method you ASKED for, not the every-method-of-the-class traffic JDWP delivers it. Counted once per hit and not once per armed location, so a finally line armed at 2 locations reports the number of times it ran. Cumulative across a toggle_stop_point disable and re-arm, since that keeps the same stop point rather than making a new one. A STOP POINT ARMED WITH hit_count IS REPORTED **SPENT** ONCE IT HAS FIRED, which is a third state and not a synonym for disabled: disabled is something YOU did with toggle_stop_point and can undo, spent is something the DEBUGGEE did — JDWP's Count modifier makes the JVM report the Nth occurrence and delete the request itself. Before this existed such a stop point was listed as armed forever, which is the same silence-reads-as-an-answer failure the Hits line above removes. toggle_stop_point re-arms a spent stop point with the same count.".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.clear_stop_point".to_string(),
            description: "Clear a specific breakpoint (bp_…), exception breakpoint (exc_…), watchpoint (watch_…), method-exit request (mexit_…), or WILDCARD FAMILY (bpset_…) by its id. A bpset_ id clears everything one wildcard armed — every breakpoint AND the watch that arms matching classes as they load, which is the part that would otherwise keep growing a family you thought you had dropped. Clearing a single bp_ that belongs to a family works too and leaves the rest of the family alone — and it frees a slot under that family's max_classes, so a family that had gone quiet because it was full starts watching for new classes again; the reply says so when that happens, since it changes what the family will do with the next class the JVM loads.".to_string(),
            input_schema: to_val(schemars::schema_for!(ClearBreakpointArgs)),
        },
        Tool {
            name: "debug.toggle_stop_point".to_string(),
            description: "Silence or re-arm a line breakpoint (bp_…), or a whole wildcard family (bpset_…, which toggles every member AND its watch for classes loading later — a family silenced without that would keep quietly arming new classes; re-arming a family that is still FULL deliberately does not put its watch back, and the reply says so), without losing its condition/trace_expr — disabling clears the JDWP request but keeps the definition, enabling re-arms it at the same location. Pass enabled:false/true, or omit to flip. Handy to quiet a chatty breakpoint on a shared JVM without having to retype it.".to_string(),
            input_schema: to_val(schemars::schema_for!(ToggleBreakpointArgs)),
        },
        Tool {
            name: "debug.panic".to_string(),
            description: "Safety: clear ALL stop points — breakpoints, exception breakpoints, watchpoints AND method-exit requests, traced or not — and resume ALL threads. Use to unfreeze a JVM if a breakpoint left a thread suspended. It also RELEASES EVERY THREAD debug.suspend_thread is holding, naming each one, and verifies each against the JVM's own suspend count rather than trusting the command — a VM-wide resume stops as soon as the thread it happens to probe reaches zero, so a per-thread suspend could otherwise survive a panic that reported \"resumed all threads\". Method-exit requests matter most here: a suspending one on a hot method re-freezes the VM on the very next return, so resuming without clearing them would be no rescue at all. ONE THING IT CANNOT PUT BACK: a class installed by debug.reload_class keeps serving that bytecode through the panic and after you disconnect, to everyone else on the instance, until the artifact is redeployed — so the reply NAMES any such class rather than letting a clean-looking result imply the JVM is as you found it.".to_string(),
            input_schema: empty(),
        },
    ]
}

/// Driving the VM forward, and changing what it does next.
fn execution_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "debug.continue".to_string(),
            description: "Resume the WHOLE VM after a suspending hit, a debug.pause, or a step — the call the debuggee is waiting for while you inspect it, and on a shared instance the call that gives other people's requests back. It resumes for REAL rather than issuing one resume and hoping: JDWP suspensions are COUNTED, so a debug.pause on top of a breakpoint hit needs more than one resume, and the JVM will happily acknowledge a resume that left the VM still frozen — so this clears the whole suspend depth, verifies the VM is running, and tells you when it could NOT get it running instead of reporting a rescue that didn't happen (SAFE-7). It also drops any pending single-step request first, which would otherwise re-fire the instant threads run again. It does NOT clear your stop points: the next hit suspends the VM again, which is what you want when you are stepping through hits and is a trap when you have walked away — debug.panic is the one that clears everything and resumes, and the watchdog (JDWP_WATCHDOG_SECS, default 120) is what happens if you forget both, resuming the VM and disabling the stop point that froze it so the freeze can't immediately recur. IT IS ABOUT THE WHOLE VM'S SUSPEND DEPTH, which is a different subject from a thread you froze with debug.suspend_thread: releasing that one is debug.resume_thread. The two are not fully independent, and the reply says which way it fell — VirtualMachine.Resume decrements EVERY thread's count by one, so a continue takes one suspend off a thread you were holding as well, and a thread you suspended twice survives it. Either way this re-reads the JVM's own count for every thread this session holds and names the ones still suspended, rather than leaving an invisible freeze behind. debug.panic and debug.disconnect clear both outright.".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.pause".to_string(),
            description: "Suspend EVERY THREAD IN THE JVM, wherever they happen to be — no location, no thread argument, no filter, which makes this the bluntest call in this server. On a shared instance it freezes every in-flight request for as long as you hold it, including requests nobody told you about, and it is the call that has actually frozen a VM here: a forgotten debug.pause is why the watchdog was extended to cover manual pauses (SAFE-4). It ends exactly three ways — debug.continue, debug.panic, or the watchdog auto-resuming after JDWP_WATCHDOG_SECS (default 120; the reply states the number, and with JDWP_WATCHDOG_SECS=0 nothing will ever resume it and the reply says that instead). Before reaching for it, note what does the same job without holding the VM: debug.suspend_thread with a thread_id freezes ONE thread, which is all you need to read a frame's locals, walk an object's fields or write a local — the usual reason this tool used to be reached for. Note what a pause does NOT buy that people assume it does: method INVOCATION needs a thread suspended by an EVENT, so evaluating a getter or a Map subscript answers INVALID_THREAD after a debug.pause exactly as it does after a debug.suspend_thread (measured) — only a suspending stop point gives you that. debug.thread_dump with suspend:true takes its own bounded suspension (max_suspend_ms, default 2000) and verifies the resume, which covers the main honest use of a pause — \"it's wedged, who is blocked on what?\" — and any stop point with trace:true snapshots without suspending at all. Use this when you need the VM held while you ask several questions of it, and on a JVM you own. Idempotent on purpose: pausing an already-suspended VM would build a suspend depth that one debug.continue cannot undo, so it reports what is already holding the VM, how long it has been held, and changes nothing (SAFE-7).".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.suspend_thread".to_string(),
            description: "Suspend ONE NAMED THREAD and leave every other thread in the JVM running and serving requests. THIS IS THE CHEAP WAY TO READ A LIVE FRAME, and on a shared instance it is the only affordable one: debug.pause and a non-traced stop point both freeze the WHOLE VM. Freeze one WildFly worker, read its whole stack with locals, walk an object's fields, write a local, release it — while the other 299 keep serving. WHAT IT UNLOCKS, measured rather than assumed: debug.get_stack with locals, debug.evaluate of a local or a field chain, expand_objects (which reads fields and invokes nothing), debug.set_value on a local, and this thread's own monitors in debug.thread_dump. WHAT IT DOES NOT: method INVOCATION. JDWP permits an invoke only on a thread suspended BY AN EVENT, so a Map subscript, a getter, .toArray() and a toString() answer INVALID_THREAD on a thread you suspended — and, measured, on one held by debug.pause too, so that was never the alternative it looked like. Only a suspending stop point gives you an invocable frame. debug.force_return and debug.pop_frame act on the TOP frame, so a worker parked in Thread.sleep or a socket read (most idle pool threads) answers OPAQUE_FRAME; the Java frames below it still read and write via frame_index. thread_id is REQUIRED and comes from debug.list_threads (they are hex, 0x7f2c…); nothing is guessed, because guessing freezes a worker nobody asked about. Nor does a suspended thread still the world around it: other threads' frames and the monitor/lock GRAPH still need THOSE threads suspended, which is debug.thread_dump with suspend:true. SUSPENDS ARE COUNTED: the reply reads the depth back from the JVM, so a thread already held by a stop point or a debug.pause is reported at 2 and needs two resumes — debug.resume_thread takes ONE off per call and says what is left. A FINISHED thread (ran to completion, JDWP says ZOMBIE) can be named but never suspended, and a VANISHED one has no id left at all; both are refused with the right reading rather than a bare error. THE INVOCATION BUDGET, for when you do have an event-suspended frame: it bounds how long YOU wait (2s) and cancels nothing in the JVM — JDWP has no way to — so an invoke that cannot complete leaves the request outstanding on that thread. It ends four ways: debug.resume_thread, debug.panic, debug.disconnect, or the watchdog after JDWP_WATCHDOG_SECS (default 120) — which covers per-thread suspends too, because a worker frozen inside a synchronized block stalls everyone behind that lock. Visible meanwhile in debug.list_threads and debug.list_sessions, at no extra JDWP cost.".to_string(),
            input_schema: to_val(schemars::schema_for!(SuspendThreadArgs)),
        },
        Tool {
            name: "debug.resume_thread".to_string(),
            description: "Give back ONE thread that debug.suspend_thread is holding. thread_id is optional when this session holds exactly one thread — the usual case — and required when it holds several, which the reply then lists rather than guessing among. IT DECREMENTS ONE SUSPEND, NOT ALL OF THEM, because JDWP counts suspends and this server refuses to pretend otherwise: it issues the resume, then ASKS the JVM whether the thread is running, and if it is not it says \"STILL suspended, N left\" instead of reporting a success it did not achieve. So suspending twice and resuming once leaves the thread suspended, and you are told. Extra depth comes from another debug.suspend_thread, a stop point that suspended this thread, or a debug.pause — and the last of those is a DIFFERENT SUBJECT: debug.pause freezes every thread and only debug.continue clears that, which this tool says when it applies. Frame ids and values read from the thread are stale the moment it runs again. NOT a way to resume the VM: debug.continue does that, debug.panic clears everything, and the watchdog does it for you if you walk away.".to_string(),
            input_schema: to_val(schemars::schema_for!(ResumeThreadArgs)),
        },
        Tool {
            name: "debug.step_over".to_string(),
            description: "Run the current line to completion and stop on the next line of the SAME frame — anything that line calls runs without stopping inside it. Needs a thread already suspended: the one from the last hit, or thread_id. STEPPING HOLDS THE WHOLE VM, AND HOLDS IT BETWEEN CALLS: the step resumes every thread, and JDWP's step event suspends every thread again when it lands, so a stepping session is a chain of full-VM freezes with your thinking time inside each one. On a shared instance that is the most expensive thing this server can do — every other request is stopped while you read the reply — so step only on a JVM you own, or accept that cost knowingly. Each freeze ends only with debug.continue, debug.panic, or the watchdog (JDWP_WATCHDOG_SECS, default 120), which will resume the VM out from under you mid-step. Stepping is also the one thing a snapshot cannot replace, so it is worth reserving for that: if you only need to know which path reached a line and what state it saw, a trace:true stop point records exactly that plus its caller chain and never suspends anything. Only one step request is live at a time — a new step clears the previous one, and debug.continue drops it. THE REPLY DOES NOT SAY WHERE IT STOPPED: call debug.get_last_event for the new location. CLASS FILTERING IS ON BY DEFAULT, WHICH IS A BEHAVIOUR CHANGE AND IS DELIBERATE: this steps OVER java.*, javax.*, jakarta.*, sun.*, com.sun.*, jdk.*, org.jboss.*, io.undertow.*, org.wildfly.* and org.hibernate.*, so it lands on the next line of YOUR code instead of inside the JDK or a container proxy. On a JAX-RS request through WildFly that is the difference between one step and a dozen — nearly every call crosses a Weld client proxy or an EJB interceptor chain first. Nothing in the default set can match an application package, which is why it is safe to have on. Pass exclude_classes:[] for the old behaviour (step into everything), your own list to replace the default, or only_classes:[\"br.com.example.*\"] for the inverse — keep stepping until we are back in my package. The reply always says which filtering was in force, so a step that lands somewhere surprising is explicable.".to_string(),
            input_schema: to_val(schemars::schema_for!(StepArgs)),
        },
        Tool {
            name: "debug.step_into".to_string(),
            description: "Stop at the FIRST LINE OF THE METHOD the current line calls — how you get inside a call whose behaviour is the question — falling through to the next line if it calls nothing. Needs a thread already suspended: the one from the last hit, or thread_id. It steps into framework, proxy and JDK code just as readily as your own, so a line with several calls can land somewhere you did not mean and cost several more steps to escape (debug.step_out); a line breakpoint in the method you actually want is often one call instead of five, and debug.list_methods shows you the name to aim at. STEPPING HOLDS THE WHOLE VM, AND HOLDS IT BETWEEN CALLS: the step resumes every thread, and JDWP's step event suspends every thread again when it lands, so a stepping session is a chain of full-VM freezes with your thinking time inside each one — on a shared instance every other request is stopped while you read the reply. Each freeze ends only with debug.continue, debug.panic, or the watchdog (JDWP_WATCHDOG_SECS, default 120), which will resume the VM out from under you mid-step. Only one step request is live at a time — a new step clears the previous one. THE REPLY DOES NOT SAY WHERE IT STOPPED: call debug.get_last_event for the new location. CLASS FILTERING IS ON BY DEFAULT, WHICH IS A BEHAVIOUR CHANGE AND IS DELIBERATE: this steps OVER java.*, javax.*, jakarta.*, sun.*, com.sun.*, jdk.*, org.jboss.*, io.undertow.*, org.wildfly.* and org.hibernate.*, so it lands on the next line of YOUR code instead of inside the JDK or a container proxy. On a JAX-RS request through WildFly that is the difference between one step and a dozen — nearly every call crosses a Weld client proxy or an EJB interceptor chain first. Nothing in the default set can match an application package, which is why it is safe to have on. Pass exclude_classes:[] for the old behaviour (step into everything), your own list to replace the default, or only_classes:[\"br.com.example.*\"] for the inverse — keep stepping until we are back in my package. The reply always says which filtering was in force, so a step that lands somewhere surprising is explicable.".to_string(),
            input_schema: to_val(schemars::schema_for!(StepArgs)),
        },
        Tool {
            name: "debug.step_out".to_string(),
            description: "Run the current method to its return and stop at the CALL SITE in the caller's frame — the way out of a method you have seen enough of, or out of framework code debug.step_into landed you in. Needs a thread already suspended: the one from the last hit, or thread_id. It does NOT report what the method returned; debug.set_method_exit_stop is the tool that answers that, it names WHICH return statement ran, and it does it without suspending anything. STEPPING HOLDS THE WHOLE VM, AND HOLDS IT BETWEEN CALLS: the step resumes every thread, and JDWP's step event suspends every thread again when it lands, so a stepping session is a chain of full-VM freezes with your thinking time inside each one — on a shared instance every other request is stopped while you read the reply. Each freeze ends only with debug.continue, debug.panic, or the watchdog (JDWP_WATCHDOG_SECS, default 120), which will resume the VM out from under you mid-step. Only one step request is live at a time — a new step clears the previous one. THE REPLY DOES NOT SAY WHERE IT STOPPED: call debug.get_last_event for the new location. CLASS FILTERING IS ON BY DEFAULT, WHICH IS A BEHAVIOUR CHANGE AND IS DELIBERATE: this steps OVER java.*, javax.*, jakarta.*, sun.*, com.sun.*, jdk.*, org.jboss.*, io.undertow.*, org.wildfly.* and org.hibernate.*, so it lands on the next line of YOUR code instead of inside the JDK or a container proxy. On a JAX-RS request through WildFly that is the difference between one step and a dozen — nearly every call crosses a Weld client proxy or an EJB interceptor chain first. Nothing in the default set can match an application package, which is why it is safe to have on. Pass exclude_classes:[] for the old behaviour (step into everything), your own list to replace the default, or only_classes:[\"br.com.example.*\"] for the inverse — keep stepping until we are back in my package. The reply always says which filtering was in force, so a step that lands somewhere surprising is explicable.".to_string(),
            input_schema: to_val(schemars::schema_for!(StepArgs)),
        },
        Tool {
            name: "debug.set_value".to_string(),
            description: "Write a value to a local variable, a static field (e.g. ConfigDefaultUtils.dsInfra — flip tenant/infra on a live JVM without a restart), an instance field (this.status, reserva.total), or one element of an array/List/Map (numbers[0], counts[\"key\"] — via ArrayReference.SetValues, List.set or Map.put, reporting the value it displaced). Value is either a literal (int, long like 123L, a double like 1.5, a float like 2.0f, a char like 'a', true/false, null, or \"string\") coerced to the target's declared type, OR another live expression whose value is copied by reference (this.cfg = other.cfg, reserva.cliente = clienteValido) — a type-incompatible source is refused, naming both types. Locals, instance fields and elements need a suspended thread; statics don't. THE CHEAP WAY TO GET ONE is debug.suspend_thread with a thread_id, which freezes a single thread and leaves the rest of the JVM serving — writing a local that way is measured working, and you need neither debug.pause nor a breakpoint for it. Pass frame_index if the thread is parked in a native frame (a sleeping or waiting worker is), since frame 0 then has no variable table. ONE EXCEPTION: writing an element of a List or a Map calls set()/put() in the debuggee, and an invoke needs a thread suspended BY AN EVENT — an array element does not, and neither does anything else here.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetValueArgs)),
        },
        Tool {
            name: "debug.force_return".to_string(),
            description: "Force the current method (top frame of a suspended thread) to return immediately with the given value, skipping the rest of its body — e.g. make a rejecting salvar() return true without redeploying. Value is coerced to the method's return type; omit for void. GETTING A SUSPENDED THREAD: debug.suspend_thread with a thread_id freezes one thread and leaves the rest of the JVM running, and a breakpoint hit or debug.pause also works. But this acts on the TOP frame, and a thread you froze at an arbitrary moment is usually parked in a native one (Thread.sleep, a socket read), which answers OPAQUE_FRAME — so a stop point at the method you actually mean to short-circuit is the reliable route. Then debug.resume_thread — or debug.continue if the whole VM is held.".to_string(),
            input_schema: to_val(schemars::schema_for!(ForceReturnArgs)),
        },
        Tool {
            name: "debug.reload_class".to_string(),
            description: "HOT RELOAD: ship a freshly compiled .class into the running JVM and have it replace the loaded one, with no redeploy and no restart — JDWP's RedefineClasses, what an IDE calls \"reload changed classes\". Warm state, connection pools, the app context and any in-flight request all survive, including a request suspended at a breakpoint: change the method, debug.pop_frame, debug.continue, and the fix is exercised without re-issuing the call that got you there. Compiling is still yours (mvn compile / gradle classes); this reads the OUTPUT. Give class_name (must already be loaded) and the bytes are looked for at <class root>/<package as directories>/<SimpleName>.class — roots come from debug.attach {class_roots:[...]} or JDWP_CLASS_ROOTS, class_roots on the call overrides both, and class_file names one file directly. THE LIMIT THAT MATTERS: HotSpot accepts METHOD BODY changes only. Add or remove a method or a field, change a signature, a modifier or the hierarchy, and the JVM refuses — the reply says which of those you did and that a real redeploy is the only route, rather than leaving you to re-try a swap that can never land. A refusal changes nothing: redefinition is all-or-nothing. ASK debug.check_stale FIRST IF YOU WANT TO KNOW BEFORE TRYING: it compares the class file's shape against the loaded type and names every one of those restrictions your build would trip, before a byte is sent (DISC-13), and a refusal here says when it was one of the six that were foreseeable. Also reports whether the thread you are stopped on is INSIDE the class, because a frame already on the stack keeps running the bytecode it entered with. dry_run:true reports what would be shipped and sends nothing; it does NOT compare shapes, so it is check_stale that answers whether a swap would be refused at all. Refused in a read-only session (dry_run still works) — on a shared instance this is an unannounced deploy, not a debugger read. A CLASS NAME CAN RESOLVE TO SEVERAL COPIES: if more than one classloader has loaded it, this reads the first and appends a caveat naming every copy, because each has its own statics and answering confidently from whichever sorted first is a wrong answer rather than one of two readings. Pin a specific copy by suffixing the loader id it printed, as in com.example.Utils@0x7f3a1c. A selector that matches nothing is refused rather than quietly answered from another copy.".to_string(),
            input_schema: to_val(schemars::schema_for!(ReloadClassArgs)),
        },
        Tool {
            name: "debug.pop_frame".to_string(),
            description: "Rewind a suspended thread to the CALL SITE of a method it is running: the frame is discarded, the operand stack restored, and debug.continue re-executes the call. Two uses. After debug.reload_class, it is how the new bytecode actually gets entered — a frame already on the stack keeps the code it entered with, so a swap of the very method you are stopped in looks like it did nothing until the frame is popped. On its own, it re-runs a method you stepped through, with locals or fields you have since changed via debug.set_value. frame is indexed as debug.get_stack numbers them (0 = innermost), and every frame above the one you name goes too — that is JDWP's behaviour, not a convenience. Needs a suspended thread and canPopFrames; a native frame in the way (OPAQUE_FRAME) and the outermost frame both refuse, and the reply says which. debug.suspend_thread with a thread_id is the cheap way to get that thread — it freezes one thread and leaves the JVM serving, released afterwards by debug.resume_thread — but note that a frame below a native one cannot be popped either, so a worker frozen mid-Thread.sleep answers OPAQUE_FRAME at every index. A stop point in the method you want to re-enter puts the thread where this can work. WHAT IT DOES NOT UNDO: side effects. Anything the popped invocation already wrote to a field, a file, a queue or the network stays written — only the frame is rewound. Refused in a read-only session.".to_string(),
            input_schema: to_val(schemars::schema_for!(PopFrameArgs)),
        },
    ]
}

/// Reading what the VM is doing: where it stopped, its stacks, threads, and values.
fn inspection_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "debug.get_last_event".to_string(),
            description: "Get the last breakpoint/event received. Includes a machine-readable [event] line with thread id and source location (class.method:line); for an exception hit the type, its message and the catch location, and for a watchpoint hit the field with its old → new value. On JDK 15+ an NPE's message names the failing subexpression itself (\"because the return value of X.getY() is null\"), so it is usually the diagnosis rather than a restatement of the type. Absent when the exception carries no message — the key is omitted rather than reported empty. Events are buffered, so a burst of hits isn't lost: the reply says how many older ones are pending — pass limit to read them (oldest first), drain:true to discard what you've read. [suspended] says whether the VM is stopped waiting for you. An [escalation] line appears only in one situation, and it is a state worth recognising: a stop point with a condition matched, and suspending the rest of the VM afterwards did not succeed — so the hit is real and the frame is readable, but the application may still be running underneath it. The line says which, checked against the JVM rather than assumed, and [suspended] agrees with it.".to_string(),
            input_schema: to_val(schemars::schema_for!(GetLastEventArgs)),
        },
        Tool {
            name: "debug.get_stack".to_string(),
            description: "Get stack frames (compact: one line per frame `#i class.method:line`, locals indented beneath). A local whose DECLARED type says more than its value does gets that type in front of it — `java.util.List<Reserva> lines = java.util.ArrayList @0x5` (DISC-12), which is the difference between writing lines[0].getSku() and guessing at it. Only where it adds something: an int or a String local is unchanged, because the value beside it already says what it is. Objects show as `Type (id=…)` by default; pass expand_objects:true to expand each local into a field tree (with max_depth / max_children) — costly, so narrow the stack with max_frames/package_filter first.".to_string(),
            input_schema: to_val(schemars::schema_for!(GetStackArgs)),
        },
        Tool {
            name: "debug.evaluate".to_string(),
            description: "Evaluate a Java expression in frame context. Heads: a local, this, a class name, a BARE FIELD NAME of the frame's own class, or an OBJECT HANDLE (@0x1f4c); then chain .field and .method(args), including static fields and static methods (ConfigDefaultUtils.getUrl()). A ONE-SEGMENT name resolves the way Java reads it — local, then a field of `this`, then a static of the class this frame is executing in, each walking superclasses — so inside SpentCheck.tick the bare `calls` finds SpentCheck.calls and needs no qualification. A local still shadows both fields, and an instance field still wins over a static of the same name inherited from a supertype. When a bare name is none of those, the error NAMES the class it searched and the qualified form to type instead. Arguments may be literals or expressions passed by reference (svc.matches(reserva), foo.handle(this)). Method calls need a suspended thread; a plain static-field read does not. Subscripts work on arrays/List/Map: lines[0] (index, keeps chaining), counts[\"key\"] (map lookup), lines[2..5] (half-open slice), lines[?qty > 3] (filter, whose left side resolves against each element; filtering a Map tests its values and keeps the keys as key → value). A subscript, slice or filter on a java.util.HashMap, LinkedHashMap, ConcurrentHashMap or ArrayList needs NO SUSPENDED THREAD: those four are read by walking their own fields (table[] → Node.key/value/next, elementData[0..size]), which JDWP does without stopping anything — so a cache lookup on a shared instance is a plain read now, and it works under read_only too. Any other implementation (a Map subclass, a Collections.synchronizedMap wrapper) falls back to invoking get()/entrySet()/toArray() and still needs a suspended thread, because guessing at internals it does not recognise would be a confidently wrong answer. THE REPLY SAYS WHICH PATH IT TOOK, and a structural read also says it took no lock — a value read while another thread resizes the map is a sample, not a transaction. Pass expand_objects:true to get a recursive field tree instead of one line — it walks nested objects, arrays, and List/Set/Map/Optional contents to max_depth, detects cycles, and unboxes Integer/Long/etc. NOTE: the DEFAULT rendering calls the value's toString() in the debuggee, and on some framework objects that cannot complete (it may need a lock held by another suspended thread) — it is bounded to 2s and the expiry is reported in the value. expand_objects:true reads FIELDS and invokes nothing, so on those objects it is both faster and more informative than the default. A byte[] or char[] renders as DECODED TEXT with the encoding named (byte[73] ISO-8859-1 \"<?xml …\") instead of a list of numbers, and arr.length works on any array. A trailing #<charset> on the expression picks the encoding — UTF-8 (default), ISO-8859-1 (alias latin1), US-ASCII, or #raw for the element list when the array really is binary; octets that do not decode are marked \\xNN rather than replaced. ISO-8859-1 is the reading a marshalled supplier envelope usually needs. Reading a LOCAL or a field chain needs a suspended thread, and debug.suspend_thread with a thread_id is the cheap way to get one — it freezes a single thread and leaves the rest of the JVM serving. INVOKING is stricter and the difference is measured, not assumed: a method call, a Map/List subscript, a slice or filter, and the default toString() rendering all need a thread suspended BY AN EVENT (a stop point hit or a step landing). Neither debug.suspend_thread nor debug.pause qualifies — both answer INVALID_THREAD — so an invoke needs a suspending stop point on the code you want to ask about. expand_objects:true is the way round it: it reads FIELDS and invokes nothing, so it works against a thread you suspended yourself, and on framework objects it is both faster and more informative than the default line. A handle is the @0x… that every reply prints beside an object (a trace snapshot's locals and captures, an expanded field tree, debug.list_instances), and pasting one back in reaches THAT object with no suspended frame and no root to reach it from — which is how you drill into a snapshot after the fact. It must be the FIRST segment. A JDWP object id is a WEAK reference, so a handle can stop working: the reply then says `Vanished: @0x…` and explains which of the two readings it is (the debuggee says the object was collected, or it has no record of the id at all). Nothing pins objects to prevent that, deliberately — see ADR-0022. ONE READ IS DESTRUCTIVE AND read_only DOES NOT STOP IT: a JAX-RS Response entity is SINGLE-PASS, so evaluating response.readEntity(String.class) CONSUMES it and the application's own read afterwards gets an empty body — you corrupt the live request by looking at it. read_only lets it through correctly (it invokes a method you asked for, writes no field, forces no return), and nothing here can know which of the debuggee's methods tolerate being asked twice. Read at or AFTER the assignment to a local, where the entity is a re-readable String, or capture the returned value with debug.set_method_exit_stop on the reading method. Suspended BEFORE the read, only getStatus() and getHeaders() are safe. The same caution applies to any one-shot stream — an InputStream, a Scanner, an Iterator — where reading it is what spends it. A CLASS NAME IN THE EXPRESSION CAN RESOLVE TO SEVERAL COPIES, AND A MEMBER LOOKUP SEARCHES ALL OF THEM: every classloader that loaded the name defines its own type with its own members, and a redeploy leaves the retired deployment's copy loaded and sorting FIRST — so a method whose signature you have just changed, or a field you have just added, is missing from the copy that would otherwise have been inspected. If a later copy has the member it answers, and the reply names which copy did. If none does, the error says how many were searched — which rules the stale copy out, instead of blaming an arity that was never wrong. This is not an argument-type problem and never can be: overload scoring matches by JNI signature string, which is identical for the same class name under every loader. Pin a specific copy by suffixing the loader id, as in com.example.Utils@0x7f3a1c.".to_string(),
            input_schema: to_val(schemars::schema_for!(EvaluateArgs)),
        },
        Tool {
            name: "debug.evaluate_chain".to_string(),
            description: "Answer \"WHICH LINK of this chain went null?\" in one call. Takes the same chained expression debug.evaluate takes (a.getB().getC()[0].getD(), including an @0x… object handle as the head) and walks it left to right, printing every link with its value and naming the first one that is null — plus how many links after it were never evaluated. Use it when a chain yields null or an empty collection and you want to know how far down the value survived; that otherwise costs one debug.evaluate per link, bisecting by hand. Each method in the chain runs EXACTLY ONCE (links resolve against the previous link's value, not by re-evaluating longer and longer prefixes), and no toString() is invoked. A [\"key\"] or [i] link on a HashMap/LinkedHashMap/ConcurrentHashMap/ArrayList is read by walking its fields instead of invoking, so such a chain can be walked with nothing suspended; the reply names the path each collection link took. NOTE: if the chain THROWS rather than returning null, you usually don't need this — on JDK 15+ the NullPointerException's own message names the failing subexpression, and debug.set_exception_stop reports it. Takes the same trailing #<charset> selector debug.evaluate does. A CLASS NAME AT THE HEAD CAN RESOLVE TO SEVERAL COPIES, AND A MEMBER LOOKUP SEARCHES ALL OF THEM, exactly as debug.evaluate does: a redeploy leaves the retired deployment's copy loaded and sorting first, so a member added or re-signed since is absent from it. A later copy that has the member answers, and the walk's notes name which copy did.".to_string(),
            input_schema: to_val(schemars::schema_for!(EvaluateChainArgs)),
        },
        Tool {
            name: "debug.list_threads".to_string(),
            description: "List threads by name (one `0x<id> <name>` line each). A JVM like WildFly has hundreds of threads — filter with name_filter, and note the last thread that hit a breakpoint is already reported by debug.get_last_event. When there are more threads than `limit`, the ones shown are chosen the same way debug.thread_dump chooses them: by NAME FAMILY (the name with digits collapsed, so `task-3` and `task-91` are one family), one thread from each family before a second from any, so no single pool spends every slot. NOT the order the JVM lists them in, which is CREATION order — an app server starts its request pool last, so on a real WildFly the first 40 in that order were all JVM internals and selectors and not one application thread. ANY THREAD THIS SESSION IS HOLDING WITH debug.suspend_thread IS MARKED ON ITS ROW with how long it has been held — an invisible suspension is the kind that gets forgotten — and one held but off the current page is named below the listing rather than hidden by `limit`. That mark is read from session state and costs no extra JDWP packets. The reply states the rule when it truncated, names the biggest groups it left out, and reports what it spent: one packet per thread NAME, against a dump's ~8 per thread it shows, so this is still the cheap call to run FIRST to decide what to dump.".to_string(),
            input_schema: to_val(schemars::schema_for!(ListThreadsArgs)),
        },
        Tool {
            name: "debug.list_classes".to_string(),
            description: "List the classes the debuggee has actually LOADED — the first step when you do not already know the fully-qualified name a stop point needs. Only the JVM can answer this for a generated proxy, a shaded/relocated class, or a deployment whose build differs from your checkout. Narrow with filter, matched against the dotted name (com.example.Order), never the JNI signature: prefix 'com.example.*', suffix '*.OrderService', or a bare substring. A JVM like WildFly loads thousands of types, so the reply shows a page and reports matched-against-loaded rather than dumping everything — raise limit or narrow the filter to see more. Array types are excluded unless include_arrays:true. A class the JVM has not loaded yet does not appear at all (classes load on first use), which is NOT the same as the class not existing.".to_string(),
            input_schema: to_val(schemars::schema_for!(ListClassesArgs)),
        },
        Tool {
            name: "debug.list_methods".to_string(),
            description: "List a loaded class's methods with signatures rendered as Java source types (static boolean matches(java.lang.String, int)) — what you need to compose a debug.evaluate call, since overload resolution matches on the runtime types of the arguments you supply, or to check a method name before naming it in debug.set_line_stop. TYPE PARAMETERS AND ARGUMENTS ARE SHOWN WHERE THE CLASS FILE CARRIES THEM (DISC-12): <T> List<T> firstOf(List<T>, Map<String, T>) rather than the erased List firstOf(List, Map). A method with no generic signature — which is most of them, and every one compiled without the attribute — renders exactly what it always did. Note what the type arguments do NOT change: overload resolution still matches on ERASED runtime types, because that is all the JVM has at the moment of the call. Overloads all appear, so the parameter lists can be compared side by side. static/abstract/native are marked; abstract and native have no body to put a line breakpoint in. Declared methods only unless inherited:true walks the superclass chain (each inherited row says which class it came from). <clinit> is omitted; constructors (<init>) are kept. If the class is not loaded the reply says so and points at debug.list_classes, because JDWP cannot tell a wrong name from a not-yet-loaded one. A CLASS NAME CAN RESOLVE TO SEVERAL COPIES: if more than one classloader has loaded it, this reads the first and appends a caveat naming every copy, because each has its own statics and answering confidently from whichever sorted first is a wrong answer rather than one of two readings. Pin a specific copy by suffixing the loader id it printed, as in com.example.Utils@0x7f3a1c. A selector that matches nothing is refused rather than quietly answered from another copy.".to_string(),
            input_schema: to_val(schemars::schema_for!(ListMethodsArgs)),
        },
        Tool {
            name: "debug.list_fields".to_string(),
            description: "List what state a loaded class HOLDS — the other half of debug.list_methods, for when you have a type but no instance: a static holder, a class you are about to breakpoint into, a vendored or shaded class your checkout cannot show you. Each field is rendered the way Java source spells it (static final java.lang.String INFRA, int qty), so static and instance fields are told apart at a glance and the declared type is a name you can use. TYPE ARGUMENTS ARE SHOWN WHERE THE CLASS FILE CARRIES THEM (DISC-12): a field declared List<ReservaHotel> reads as that rather than as a bare java.util.List, nested and wildcard types included — which is what lets you compose lines[0].getSku() from one listing instead of guessing the element type and retrying. A generic signature is an OPTIONAL class-file attribute, so a member that has none renders exactly what it always did, and a blank type is never printed. Statics are listed FIRST, because those are the ones debug.evaluate can read with no instance and no suspended thread. final and volatile are marked too: a final may refuse a debug.set_value and will never fire a debug.set_field_stop, and a volatile is being written by something else. Declared fields only unless inherited:true walks the superclass chain (each inherited row says which class it came from) — note that expanding an actual object shows inherited state either way, so the default here is deliberately the narrower question. Bounded like the other discovery tools: raise limit or narrow with name_filter. It reads NO values — debug.evaluate reads a named static, and expand_objects renders an instance. A class that declares nothing says so as an answer rather than looking like a failed lookup; a class that is not loaded gets the same reply debug.list_methods gives, pointing at debug.list_classes, because JDWP cannot tell a wrong name from a not-yet-loaded one. A CLASS NAME CAN RESOLVE TO SEVERAL COPIES: if more than one classloader has loaded it, this reads the first and appends a caveat naming every copy, because each has its own statics and answering confidently from whichever sorted first is a wrong answer rather than one of two readings. Pin a specific copy by suffixing the loader id it printed, as in com.example.Utils@0x7f3a1c. A selector that matches nothing is refused rather than quietly answered from another copy.".to_string(),
            input_schema: to_val(schemars::schema_for!(ListFieldsArgs)),
        },
        Tool {
            name: "debug.list_instances".to_string(),
            description: "Find the LIVE OBJECTS of one or more loaded classes, returned as @0x… handles that debug.evaluate accepts as expression heads — the only way to reach an object that no local, `this` and no static field can name. That is the container case: an @ApplicationScoped bean's caches, a producer-injected component, anything whose only reference is a Weld proxy held by the container. Also reports how many instances each type has, which is the cheap half (counts_only:true).\nTHIS IS NOT FREE, AND IT LOOKS FREE. JDWP requires no suspend for these commands and none is issued — yet the JVM STOPS THE WORLD for a full live-heap walk on every call: measured at 522 ms of held application threads on a 2,000,000-object heap to answer with 7 objects, against 54 ms on a 20,000-object heap for THE SAME 7 OBJECTS. The cost tracks the LIVE HEAP, not the answer, so on a multi-GB app server a single call can stall every in-flight request for seconds. The reply reports the duration it actually held them, and how many walks it took. Nothing here refuses on heap size; you are being told the price, not protected from it.\nASK ABOUT SEVERAL TYPES AT ONCE. The count for a whole batch is one walk (three types measured at 604 ms, about the price of one), so class_names is a list and using it is nearly free; calling this once per type is not.\nEXACT TYPE, NOT SUBTYPE-INCLUSIVE — the thing most likely to mislead you. Instances of a base class or an interface are NOT included: Widget answers 7 with two live SubWidgets in the heap, not 9. On a CDI/EJB codebase the name you reach for is usually the interface or the bean class while the live objects are …_$$_WeldClientProxy, so this can answer a confident 0 about a type with hundreds of live instances. Name the subclasses and the proxy explicitly — they ride the same walk.\nOnly strongly-reachable objects are reported. max_instances clamps the handles (0 = all) but never the reported count, so a clamped listing still tells you how many there are. Handles are WEAK references and nothing pins them, so one can report Vanished later.".to_string(),
            input_schema: to_val(schemars::schema_for!(ListInstancesArgs)),
        },
        Tool {
            name: "debug.run_named_query".to_string(),
            description: "Run a NAMED JPA QUERY through the application's own EntityManager and report the row count plus a bounded, INVOKE-FREE read of each row. The question this answers is whether a query returns what its author believes, and the shape it was built for is a lookup whose parameters are all OPTIONAL and null-guarded — WHERE (:codigo is null or r.codigo = :codigo) AND (:status is null or r.status = :status) — which matches the ENTIRE TABLE when both arrive null, so a call meant to find one row returns thousands and the caller takes the first. Rebuilding that predicate in a SQL client cannot reproduce it: you lose the persistence context, the parameter binding and, on a multi-tenant setup, the resolved tenant.\nIT INVOKES, so it needs a thread suspended BY AN EVENT (a stop point, not debug.pause and not debug.suspend_thread), and a read_only session REFUSES IT OUTRIGHT — correctly, because the query also reaches the DATABASE, which no guard here can undo.\nIT DOES NOT FLUSH, AND THAT TOOK WORK. JPA's default flush mode is AUTO, under which the provider pushes every pending change in the persistence context to the database BEFORE answering a query — so merely asking would write, and on a shared instance it would commit somebody else's half-finished work. This sets FlushModeType.COMMIT on the Query it created, which suppresses that for this query alone and touches neither the EntityManager nor anyone else's. THE TRADE IS IN EVERY REPLY: with the flush suppressed the rows do NOT reflect uncommitted changes in that persistence context, so a row just saved and not committed will not be found. allow_flush:true is how you ask for the other reading, and it is a write.\nTHE COUNT IS THE TRUE ONE AND THE PER-ROW READ IS BOUNDED — different knobs, because the over-match case needs the real number. max_rows caps what is RENDERED and never the count. max_fetch caps what the DEBUGGEE BUILDS (setMaxResults), which is the cost worth knowing about: a query matching a whole table constructs every one of those entities in its heap before this tool sees any of them — and with max_fetch in force the reported count becomes a FLOOR rather than a total, which the reply says where the number is.\nTHE ROWS ARE READ, NEVER INVOKED. Each row is read by its FIELDS (ObjectReference.GetValues), walking the superclass chain so a mapped-superclass id still shows, and nothing is called on it — so no getter runs and NOTHING IS FETCHED. That is not achievable by bounding a depth: a shallow render calls toString(), which on a JPA entity routinely names its associations, and a deep one invokes toArray()/entrySet() on a collection field — the first level is already the hazard. A nested object shows as its type plus an @0x… handle, which debug.evaluate accepts as an expression head — @0x1f4c.getItens() fetches it deliberately, when you have chosen to pay for it.\nFINDING THE ENTITY MANAGER: pass entity_manager to skip discovery (a local, this.em, a static field, or an @0x… handle). Otherwise the SUSPENDED FRAME is searched — `this`'s fields first, then the in-scope locals and arguments — matching on the INTERFACE each object implements (jakarta.persistence.EntityManager or javax.persistence.EntityManager, both supported since the target stack straddles the split) rather than on a type name, because a container-managed bean's runtime type is a proxy nobody can predict. That costs a handful of packets, suspends nothing and invokes nothing. THERE IS NO HEAP FALLBACK, and the reason is measured rather than assumed: ReferenceType.Instances answers about an object's EXACT runtime class, so asking it for the EntityManager INTERFACE returns 0 however many beans are alive, and JDWP has no command for \"which classes implement this interface\". When the frame has none, the reply refuses and names the two-step precisely — debug.list_instances on the CONCRETE implementation class (org.hibernate.internal.SessionImpl, a container wrapper like org.jboss.as.jpa.container.TransactionScopedEntityManager, or whatever debug.list_classes shows for your provider), then its @0x… handle as entity_manager.\nAN UNKNOWN QUERY NAME IS ITS OWN ANSWER, not a generic evaluation failure — but it CANNOT be answered with a list of the names that exist, because EntityManager publishes no method that enumerates them. The reply says the provider rejected the name and where such names are declared (@NamedQuery, @NamedQueries, orm.xml's <named-query>), and notes that the entity prefix is part of the name.\nPARAMETERS bind by name (parameters) or by 1-based position (positional_parameters, JPQL's ?1), one form or the other and never both. THE REPLY NAMES THE JAVA TYPE EACH ONE WAS BOUND AS, because that is the silent hazard: JPA binds by object and compares with equals, so a Long id column given an Integer matches NOTHING with no exception and no warning — an empty result that reads like a fact about the data. Whole numbers therefore bind as Long, and parameter_expressions takes the full debug.evaluate grammar for anything JSON cannot spell (an exact Integer, 42L, 2.0f, 'a', an enum constant read as a static field, an @0x… handle, or a value out of the frame). A collection parameter — IN (:names) — is REFUSED rather than stringified, pointing at parameter_expressions to name a List that already exists.\nTHE QUERY TEXT is reported best-effort via getQueryString(), which is Hibernate's rather than JPA's, so its absence is normal and said plainly. It is the JPQL AS WRITTEN and never the generated SQL — no published API reaches that — and the reply labels it so, because calling it the SQL would be a small lie about the one line a caller would act on. Ad-hoc JPQL strings are out of scope.".to_string(),
            input_schema: to_val(schemars::schema_for!(RunNamedQueryArgs)),
        },
        Tool {
            name: "debug.source".to_string(),
            description: "What file a loaded class was COMPILED FROM, and — when source roots are configured — the source lines around a given line. Two independent halves. The JVM half needs no local files at all and is the one that settles whether your checkout is the code that is actually running: a class reporting Order.java in a tree where that file was renamed months ago is the answer, and reading local source would never have shown it. A JSR-45 source debug extension (JSP, Kotlin, Groovy) is reported when the class carries one, meaning the .java name is only an intermediate. The disk half turns a stack frame's class.method:line into text: pass line to get a bounded window around it with line numbers (context, default 20 either side) — that is the intended use, since a caller chasing one frame should not pull a 2000-line file into context. whole_file:true returns everything, still capped by max_lines (default 400), and the reply always states which lines of how many it is showing. Roots come from debug.attach {source_roots:[...]} or the JDWP_SOURCE_ROOTS environment variable, and source_roots on the call overrides both ([] reads no file). A root is where the PACKAGE TREE starts: the file is looked up at <root>/<package as directories>/<the name the JVM reported>, which is why an inner class (com.example.Order$Line) correctly resolves to its enclosing Order.java. Plain directories only — sources inside JARs are not read. The failure modes stay distinct: class not loaded, loaded but compiled with no SourceFile attribute (javac -g:none, or a synthetic class), no configured root holds the file, and found-but-unreadable each say something different about what to fix. THE WINDOW IS CHECKED AGAINST THE RUNNING BYTECODE WHENEVER A CLASS ROOT IS CONFIGURED (debug.attach {class_roots:[...]}, JDWP_CLASS_ROOTS, or class_roots on the call), because source read off disk is confidently wrong in a way nothing else in the reply would show: the case this was measured against is a class root byte-identical to the deployed jar and two commits BEHIND src/main/java, so the statement you are shown is a later version of the one that is running, and the enum or registry you are reading has entries the JVM has never heard of. TWO INDEPENDENT AXES, REPORTED SEPARATELY because they have different remedies — the JVM's line tables against your compiled .class (fix by redeploying, or debug.reload_class to install it; debug.check_stale has the per-method detail), and your .class against the .java being displayed (fix by recompiling). On the second axis a source file too SHORT to hold a line the compiler emitted a table entry for is a PROOF, while a .java whose mtime is newer than the .class is only a TIMESTAMP and is said as one, because a checkout moves an mtime without changing a byte. A build that matches adds NOTHING to the reply. With no class root nothing is compared and the reply says THAT, which is not the same as saying it passed. The check costs one Method.LineTable per method of the class, which is why a configured class root is what turns it on; pass class_roots:[] to skip it. A translated class (JSR-45) gets no length check, because its line numbers are positions in the .jsp rather than in the file on disk. A CLASS NAME CAN RESOLVE TO SEVERAL COPIES: if more than one classloader has loaded it, this reads the first and appends a caveat naming every copy, because each has its own statics and answering confidently from whichever sorted first is a wrong answer rather than one of two readings. Pin a specific copy by suffixing the loader id it printed, as in com.example.Utils@0x7f3a1c. A selector that matches nothing is refused rather than quietly answered from another copy.".to_string(),
            input_schema: to_val(schemars::schema_for!(SourceArgs)),
        },
        Tool {
            name: "debug.check_stale".to_string(),
            description: "Is the JVM running the code you just compiled, or an older build? Compares the line tables the JVM holds for a loaded class against the ones in your freshly compiled .class, method by method. This is the check that stops twenty tool calls being spent debugging the PROGRAM when the fact is that the deployed bytecode is last week's — a breakpoint that never fires, or fires with locals that make no sense for the code you are reading, looks identical to a wrong hypothesis, and nothing else here can tell them apart. Different from debug.source, which settles WHICH FILE a class was compiled from: SourceFile is a compile-time string and is identical across every build of the file, so it cannot see the common case of same class, same Order.java, older bytecode. Bytes come from the same place debug.reload_class reads them — <class root>/<package as directories>/<SimpleName>.class, from debug.attach {class_roots:[...]}, JDWP_CLASS_ROOTS, class_roots on the call, or class_file directly. WHAT THE ANSWER MEANS: it compares line tables, so it catches an edit that MOVED a line (which is what makes a stop point at :412 point somewhere else) and is blind to one that changes a body without moving any line — a clean result means \"no line moved\", not \"byte-for-byte identical\", and the reply says so. A method the running class has and your build does not (or the reverse) is reported separately, because that is a different class shape and not something a hot reload could fix. A class with no line tables at all (javac -g:none, an interface, all-abstract) is reported as CANNOT TELL rather than as a match. Costs one JDWP packet per method, and says how many it spent. PASS bytecode:true FOR THE EDIT LINE TABLES CANNOT SEE (DISC-9): it also compares each method's actual code, catching a body change that moved no line — `x < 5` to `x <= 5`, a changed constant, a swapped operator — which is the commonest edit in a compile-and-retest loop and completely invisible to the default comparison. It is also the only evidence that works on a -g:none build, which has code and no line numbers, turning a CANNOT TELL into a real answer. Costs one more packet per method, so it is opt-in. The reply always names which evidence it used and whether the two agree; a difference in bytecode with matching line tables is called out, together with the one other thing that produces it (a build compiled by a different javac, since constant-pool indices live in the operands — same compiler and same source is byte-identical). IT ALSO FORECASTS WHETHER YOUR BUILD COULD BE INSTALLED AT ALL, which is a DIFFERENT question from whether the JVM is behind (DISC-13): a class can be both stale and illegal to swap, and the two need different remedies — a redeploy fixes the first, only a restart fixes the second. Before anything is attempted it compares declared fields, declared methods, their modifiers, the class modifiers, the superclass and the interface list against the loaded type, and names every HotSpot restriction the change would trip — SCHEMA_CHANGE_NOT_IMPLEMENTED (64) for a field added, removed or re-modified, ADD_METHOD (63), DELETE_METHOD (67), METHOD_MODIFIERS_CHANGE (71), CLASS_MODIFIERS_CHANGE (70), HIERARCHY_CHANGE (66). Worth having because the answer is close to a coin flip rather than usually yes: across the 300 most recent .java-touching commits in the target repository, 151 were structural and 149 body-only, and the churn concentrates in the classes where a redefine is most awkward. THE TWO VERDICTS ARE HELD TO DIFFERENT STANDARDS ON PURPOSE. A refusal is stated confidently, naming the code and what in your build produces it. A pass says only NO STRUCTURAL CHANGE DETECTED and explicitly does NOT promise the swap will succeed, because the refusals a static comparison cannot see are a verifier rejection, INVALID_TYPESTATE against instances that already exist, and a class-file version this JVM will not read — and canAddMethod / canUnrestrictedlyRedefineClasses vary between JVMs. debug.reload_class {dry_run:true} is the authority on what this VM can do, and the swap itself is the only proof. A changed method SIGNATURE is reported as both an add and a delete, because that is what it is to the JVM: one member gone, another arrived. All predicted refusals are listed rather than just one, since the JVM stops at the first restriction it reaches and clearing that can reveal the next. One false positive is possible and the reply says so: a member differing only because the two builds came from different javac versions — a bridge method, a synthetic accessor — reads here as an added or removed method. Costs about six more JDWP packets on top of the per-method line tables. A CLASS NAME CAN RESOLVE TO SEVERAL COPIES: if more than one classloader has loaded it, this reads the first and appends a caveat naming every copy, because each has its own statics and answering confidently from whichever sorted first is a wrong answer rather than one of two readings. Pin a specific copy by suffixing the loader id it printed, as in com.example.Utils@0x7f3a1c. A selector that matches nothing is refused rather than quietly answered from another copy.".to_string(),
            input_schema: to_val(schemars::schema_for!(CheckStaleArgs)),
        },
        Tool {
            name: "debug.thread_dump".to_string(),
            description: "Every thread's stack in ONE call, plus which monitors each thread holds and which one it is blocked entering — the \"it's wedged, who is blocked on what?\" question, which list_threads (names only) and get_stack (one thread) can't answer. A thread waiting on a lock someone else holds is annotated `← held by 0x<id> \"<name>\"`, so a deadlock cycle is readable off the output. IMPORTANT: JDWP can only read a SUSPENDED thread's stack and locks, so on a running VM every thread comes back unreadable — pass suspend:true to freeze it briefly (it is resumed and verified before the reply) or only_suspended:true to list just the readable ones. It never suspends on its own. Narrow the cost with name_filter / limit / max_frames / package_filter; for the deadlock question alone, monitors_only:true reads the lock graph without the frames. The reply reports how many JDWP packets it spent, WHAT EACH ONE COST on this connection (round trip plus our processing), and, when it suspended, how many milliseconds it held the VM — bounded by max_suspend_ms (default 2000), which truncates loudly rather than silently and tells you what finishing would have cost at the rate it was running. Those figures are measured against the JVM you are attached to, so you never have to judge whether a number measured elsewhere applies: a dump of a 306-thread pool with 60-frame stacks costs ~258 packets and ~65ms at the defaults, or ~1,625 packets and ~700ms for every thread and every frame. THREADS WITH AN IDENTICAL STACK COLLAPSE INTO ONE ENTRY WITH A COUNT — `×40 \"default task-#\" [monitor] — 40 thread(s) with an IDENTICAL stack`, followed by the ids and the stack ONCE. A pool of 200 parked in socketRead0 beneath one call site is one fact, and printing it 200 times used to spend the whole limit hiding it. Two threads are one entry only when their name family, status, debugger-suspension, frame list AND lock state all match, so two pools at the same site stay two rows (which one is exhausted is the diagnosis) and threads holding or waiting on DIFFERENT locks never merge — different locks are different object ids. A collapsed entry states its shared lock once, keeping the `← held by` correlation a deadlock investigation reads. COLLAPSED IS NOT OMITTED, TRUNCATED OR VANISHED, and the reply keeps all four apart: every thread in a group was read and is counted in the header's total, and it costs NO extra packets because grouping happens over rows already collected. But a group's count is over the threads this dump READ — selection happens before any stack is fetched, so if the footer says threads were withheld, whether THEY share the stack is unknown and only a larger limit can settle it. A dump whose stacks are all distinct is byte-for-byte what it was before this existed. Works in a read-only session (it invokes nothing).".to_string(),
            input_schema: to_val(schemars::schema_for!(ThreadDumpArgs)),
        },
        Tool {
            name: "debug.get_traces".to_string(),
            description: "Return snapshots captured by non-suspending trace mode — debug.set_line_stop, debug.set_exception_stop or debug.set_field_stop with trace:true. Each shows where it fired, its calling chain as `← class.method:line` entries (nearest caller first, locations only), the thread, in-scope locals/args, every trace_expr result the stop point asked for — one per expression, in the order given, so two values captured at the same instant can be compared directly (a trace_expr may carry a trailing #<charset> — e.g. entry.dsRequest#ISO-8859-1 — to decode a byte[] payload, and .length works there too), plus the exception type/message/catch site or the field's old → new value. A record marked `↻ rethrow of #<seq>` is the escaping end of a rethrow chain; `#<seq>` is the original throw, which is the one with the application frame and the cause. Bounded ring buffer (most recent kept). Narrow with bp_id (one stop point), class_filter (substring), or since (only records newer than a #seq you already saw, for polling). Pass clear:true to empty the buffer after reading. EVERY OBJECT-VALUED ENTRY CARRIES AN @0x… HANDLE, which debug.evaluate accepts as an expression head — so a snapshot is a starting point rather than a dead end: @0x1f4c.getStatus() reads the same object afterwards, with nothing suspended. The id is a WEAK reference and nothing pins it, so a handle whose object has been collected reports `Vanished` rather than a JDWP error; on a pool that retires workers that is the ordinary outcome. A hit inside an ANONYMOUS inner class (Outer$2 — a Callable or Runnable handed to a pool) also carries a `captured{val$…=…, this$0=…}` group: javac compiles the enclosing method's captured locals to synthetic fields that are NOT in the worker method's variable table, so this is the only place the submitter's context — which request, which session, which supplier — crosses the thread boundary. Read as fields, invoking nothing. Lambdas need no such group and get none; their captures are already ordinary parameters.".to_string(),
            input_schema: to_val(schemars::schema_for!(GetTracesArgs)),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    // The client sees one flat list; guard against a group being dropped from `get_tools` or a name
    // appearing twice after a regroup.
    #[test]
    fn tool_names_are_unique_and_complete() {
        let tools = get_tools();
        let names: std::collections::BTreeSet<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), tools.len(), "duplicate tool name in get_tools()");
        for expected in [
            "debug.attach",
            "debug.launch",
            "debug.list_sessions",
            "debug.disconnect",
            "debug.set_line_stop",
            "debug.set_exception_stop",
            "debug.set_field_stop",
            "debug.set_method_exit_stop",
            "debug.set_monitor_stop",
            "debug.list_stop_points",
            "debug.clear_stop_point",
            "debug.toggle_stop_point",
            "debug.panic",
            "debug.continue",
            "debug.pause",
            "debug.suspend_thread",
            "debug.resume_thread",
            "debug.step_over",
            "debug.step_into",
            "debug.step_out",
            "debug.set_value",
            "debug.force_return",
            "debug.reload_class",
            "debug.pop_frame",
            "debug.get_last_event",
            "debug.get_stack",
            "debug.evaluate",
            "debug.evaluate_chain",
            "debug.list_threads",
            "debug.list_classes",
            "debug.list_methods",
            "debug.list_fields",
            "debug.list_instances",
            "debug.run_named_query",
            "debug.source",
            "debug.check_stale",
            "debug.thread_dump",
            "debug.get_traces",
        ] {
            assert!(names.contains(expected), "missing tool: {expected}");
        }
    }

    /// Every tool description is one enormous single-line string literal, and this is what stops a merge
    /// shredding one again (DOC-7, #108).
    ///
    /// Two merges in the v0.9.0 range did exactly that. `debug.evaluate` shipped reading "…usually needs.
    /// **Anees n thread at all.** Reading a LOCAL…" and "it reads FIELDS and **invokhing**", with the `@0x…`
    /// object handle deleted from its head list and an ungrammatical fragment about it stranded 3000
    /// characters later; `debug.evaluate_chain` had the same clause moved out of the parenthesis it qualified
    /// and left dangling past the final full stop. Git resolved both **without a conflict**, because on a
    /// one-line string there is nothing to conflict on — the line is a single hunk and whichever side wins
    /// takes its neighbour's words with it.
    ///
    /// Nothing caught it. The test above asserts on tool *names*, nothing at all asserted on argument shapes,
    /// and no human reads a 4000-character line in a diff. But these descriptions are the caller's only
    /// documentation, and `docs/toolkit-contract.md` names them as the mitigation for five of the downstream
    /// toolkit's six silent failure modes — so gibberish here is a caller-visible defect, and it shipped.
    ///
    /// That middle clause used to read "the schema snapshots on argument shapes", and there were none
    /// (DOC-8, #120). A sentence over-stating what is verified, inside the explanation written *because*
    /// "nothing caught it", is the same defect one level up: the next person deciding whether an argument
    /// change needs a guard reads it and concludes one is already there. `argument_schemas_match_the_committed_snapshot`
    /// is what makes the claim true now, and it is stated in the past tense here because it was not true then.
    ///
    /// The patterns are the *shapes* that kind of damage leaves rather than a spell-check: a stranded clause
    /// starts with punctuation, a spliced sentence doubles a word or a space, a truncated one ends mid-token.
    /// Measured across all 36 descriptions they produced 5 hits — the 4 real defects and 1 false positive —
    /// which is why this gates rather than merely reports.
    #[test]
    fn no_tool_description_carries_the_marks_of_a_bad_merge() {
        // The one legitimate hit: `VirtualMachine.Resume` is JDWP's own command name, and it trips the
        // missing-space-after-period pattern for the same reason any qualified identifier would. Listed by
        // exact substring rather than by suppressing the whole rule, so the rule still guards every other
        // description — including the rest of this one.
        let allowed = ["VirtualMachine.Resume"];

        // Plain substring scanning rather than a regex crate: the shapes are trivial and a dev-dependency for
        // five of them would be its own argument. One list, so there is nothing for a second list to disagree
        // with — which is the failure mode this whole test is about.
        //
        //   " . " and " ,"  a clause stranded by a splice starts with the punctuation that joined it
        //   ". ,"           a fragment appended after a completed sentence
        //   "—."            a dash left dangling where its clause was taken away
        //   "  "            two sides' whitespace both surviving the join
        // Hoisted and cleared per tool rather than allocated inside the loop — `scripts/doctor.sh` fails on
        // the latter and `cargo clippy` does not, which is ADR-0007 in one line.
        let mut complaints: Vec<String> = Vec::new();

        for tool in get_tools() {
            let d = &tool.description;
            complaints.clear();

            for needle in [" . ", ". ,", "—.", "  ", " ,"] {
                if let Some(at) = d.find(needle) {
                    let from = at.saturating_sub(70);
                    let to = (at + needle.len() + 70).min(d.len());
                    let window = &d[char_boundary(d, from)..char_boundary(d, to)];
                    if allowed.iter().any(|ok| window.contains(ok)) {
                        continue;
                    }
                    complaints.push(format!("{needle:?} at {at}: …{window}…"));
                }
            }

            // A description that ends without terminal punctuation is the other shape a splice leaves — the
            // sentence carrying the full stop went to the other side of the merge.
            if !d.trim_end().ends_with(['.', '!', '?', ')']) {
                complaints.push(format!(
                    "ends mid-sentence: …{}",
                    &d[char_boundary(d, d.len().saturating_sub(80))..]
                ));
            }

            // A doubled word of four letters or more. Short ones have legitimate repeats ("that that", "had
            // had"); four-plus does not, and "the the" is caught by the double-space rule anyway.
            let words: Vec<&str> = d.split_whitespace().collect();
            for pair in words.windows(2) {
                if pair[0].len() >= 4 && pair[0].eq_ignore_ascii_case(pair[1]) {
                    complaints.push(format!("doubled word {:?}", pair[0]));
                }
            }

            assert!(
                complaints.is_empty(),
                "{}'s description carries the marks of a bad merge — these are the shapes an interleaved \
                 one-line string leaves, and DOC-7 (#108) shipped four of them:\n  {}",
                tool.name,
                complaints.join("\n  ")
            );
        }
    }

    /// `&str` slicing panics on a non-char-boundary index, and every description here is full of em dashes and
    /// ellipses. Rounding down to the nearest boundary keeps the error *message* from becoming the failure.
    fn char_boundary(s: &str, mut at: usize) -> usize {
        while at > 0 && !s.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    /// The descriptions, word-wrapped, as a committed snapshot — and the reason this exists as well as the
    /// pattern scan above (DOC-7, #108).
    ///
    /// The scan catches the *shapes* a splice leaves. Checked by restoring each of the four v0.9.0 defects, it
    /// caught three: the stranded `—.`, the `. ,` fragment, and a description ending mid-sentence. It did not
    /// catch the fourth. `"Anees n thread at all."` — the wreckage of "A plain static-field read needs no
    /// thread at all." — has no punctuation artifact at all; it is shaped like a sentence and only a reader
    /// knows it is not one. No cheap pattern finds that, and pretending otherwise would be the kind of
    /// reassurance this repo keeps writing guards against.
    ///
    /// A snapshot does find it, because it does not have to understand the text — any change fails. What it
    /// costs is that a deliberate edit must regenerate the file, and that cost *is* the feature: it forces the
    /// review moment that was missing when a merge rewrote a 4000-character line and no human read it.
    ///
    /// Word-wrapped at 110 columns rather than split into sentences, because sentence-splitting on `". "` is
    /// wrong in a file where `debug.evaluate` and `Klass.CONSTANT` are ordinary text. Wrapping needs no
    /// heuristic and still makes a corrupted clause a two-line diff.
    ///
    /// Regenerate with `UPDATE_TOOL_DESCRIPTIONS=1 cargo test --bin jdwp-mcp _snapshot`, then read the
    /// diff — that is the whole point of it. The filter is `_snapshot` rather than this test's own name
    /// because there are two of these now and one command has to rewrite both: a filter naming one of them
    /// leaves the other failing against a file it was never given the chance to update, which is a
    /// regeneration path that teaches you to run it twice (DOC-8, #120).
    #[test]
    fn tool_descriptions_match_the_committed_snapshot() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tool-descriptions.txt");
        let current = render_description_snapshot();

        if std::env::var_os("UPDATE_TOOL_DESCRIPTIONS").is_some() {
            std::fs::write(&path, &current).expect("write the tool-description snapshot");
            println!("rewrote {} — read the diff before committing it", path.display());
            return;
        }

        // Read at runtime rather than `include_str!` so regenerating does not need a rebuild first.
        let committed = std::fs::read_to_string(&path).unwrap_or_else(|why| {
            panic!(
                "cannot read the tool-description snapshot at {}: {why}. Create it with \
                 UPDATE_TOOL_DESCRIPTIONS=1 cargo test --bin jdwp-mcp _snapshot",
                path.display()
            )
        });

        if committed != current {
            let differing =
                committed.lines().zip(current.lines()).enumerate().find(|(_, (was, now))| was != now);
            let first = match differing {
                Some((n, (was, now))) => {
                    format!("line {}:\n  committed: {was}\n  current:   {now}", n + 1)
                }
                // Every shared line matches, so one side simply has more of them — a description was added,
                // removed, or grew past a wrap boundary.
                None => format!(
                    "no differing line, so the length changed: {} committed vs {} current",
                    committed.lines().count(),
                    current.lines().count()
                ),
            };
            panic!(
                "a tool description changed without its snapshot being updated.\n\n{first}\n\nIf the change \
                 was deliberate: UPDATE_TOOL_DESCRIPTIONS=1 cargo test --bin jdwp-mcp _snapshot, then \
                 READ THE DIFF — a caller-visible change behind an unchanged tool name is what \
                 docs/toolkit-contract.md is for. If it was not deliberate, a merge has shredded a \
                 one-line string literal again (DOC-7, #108)."
            );
        }
    }

    fn render_description_snapshot() -> String {
        let mut tools = get_tools();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        let mut out = String::from(
            "# Every debug.* tool description, word-wrapped at 110 columns. GENERATED — do not hand-edit:\n\
             #     UPDATE_TOOL_DESCRIPTIONS=1 cargo test --bin jdwp-mcp _snapshot\n\
             #\n\
             # This file exists so that a change to a tool description has to be READ by somebody. Two merges\n\
             # in the v0.9.0 range interleaved two of these single-line string literals and shipped the\n\
             # gibberish; nothing failed, because the tests asserted on names and nothing else, and no human\n\
             # reads a 4000-character line in a diff (DOC-7, #108). Argument schemas are guarded separately,\n\
             # in argument-schemas.txt, which the same command regenerates (DOC-8, #120).\n",
        );
        for tool in tools {
            out.push_str("\n## ");
            out.push_str(&tool.name);
            out.push('\n');
            for line in wrap(&tool.description, 110) {
                out.push_str(&line);
                out.push('\n');
            }
        }
        out
    }

    /// Greedy word wrap. Counts CHARACTERS, not bytes: these descriptions are full of em dashes and ellipses,
    /// and wrapping by byte length would put the break in a different place depending on how much punctuation
    /// a line happened to carry.
    fn wrap(text: &str, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let mut line = String::new();
        for word in text.split_whitespace() {
            let would_be = line.chars().count() + usize::from(!line.is_empty()) + word.chars().count();
            if !line.is_empty() && would_be > width {
                lines.push(std::mem::take(&mut line));
            } else if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            lines.push(line);
        }
        lines
    }

    // DOC-9 (#132) shipped `session_id`'s description with runs of thirty-three spaces in it, on all
    // thirty-eight tools: a `\`-continuation in the source, joined by `cargo fmt` with each line's
    // indentation kept as CONTENT. A description is the caller's documentation (`docs/toolkit-contract.md`).
    //
    // **The two description snapshots cannot catch this, which is why a separate test has to.** Both render
    // the text WORD-WRAPPED to 110 columns, and wrapping normalises whitespace — so fixing all
    // thirty-eight of those descriptions produced a **byte-identical** `argument-schemas.txt`. The wrapping
    // is deliberate and right for what it was written for (a corrupted clause becomes a two-line diff
    // instead of a one-character one, DOC-7/#120), and it makes this class invisible by construction: the
    // snapshot is a guard on what a description SAYS, not on what it is made of.
    //
    // Cheap and exact: no prose anyone writes deliberately carries a run of spaces mid-line, so the run is
    // the defect.
    #[test]
    fn no_description_carries_the_indentation_of_the_source_it_was_written_in() {
        let mut offenders: Vec<String> = Vec::new();
        for tool in get_tools() {
            let mut check = |what: &str, text: &str| {
                // Per line, and only after that line's own indentation: a description is markdown, so a
                // continuation indented under a bullet is correct authoring and `class_pattern` has one. The
                // defect is a run of spaces in the MIDDLE of a line, which is the only way source
                // indentation reaches a caller.
                for line in text.lines() {
                    let body = line.trim_start();
                    if body.contains("  ") {
                        offenders.push(format!("{} {what}: …{}…", tool.name, &body[..body.len().min(90)]));
                        break;
                    }
                }
            };
            check("description", &tool.description);
            if let Some(properties) = tool.input_schema.get("properties").and_then(|p| p.as_object()) {
                for (argument, schema) in properties {
                    if let Some(text) = schema.get("description").and_then(|d| d.as_str()) {
                        check(&format!("{{{argument}}}"), text);
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "{} description(s) contain a run of two or more spaces, which is source indentation that \
             reached the caller rather than anything a human typed. Assemble the string from `concat!` \
             pieces that carry no leading whitespace; a `\\` continuation does not survive `cargo \
             fmt`.\n  {}",
            offenders.len(),
            offenders.join("\n  ")
        );
    }

    /// The generated argument schemas, as a committed snapshot — the coverage the comment above used to
    /// claim (DOC-8, #120).
    ///
    /// `no_tool_description_carries_the_marks_of_a_bad_merge` said "the schema snapshots on argument shapes"
    /// as part of its argument for why a *description* snapshot had to exist. There were no schema snapshots.
    /// The only test over the generated schemas asserted that they GENERATE, so an argument's type, its
    /// default or its description could change with nothing failing — and the sentence claiming otherwise was
    /// inside the explanation written *because* "nothing caught it", which is the same defect one level up:
    /// the next person deciding whether an argument change needs a guard would read it and conclude one was
    /// already there.
    ///
    /// It was not hypothetical. Renaming a term across the caller-facing surface in v0.14.1 changed **five**
    /// argument descriptions and the description snapshot showed no diff at all, because it renders
    /// `tool.description` and never touches `input_schema`. Those lines are published: `schemars` turns them
    /// into the `inputSchema` property descriptions, so a caller reads them, and
    /// `docs/toolkit-contract.md` names them as a mitigation for the downstream toolkit's silent failure
    /// modes. 37 tool descriptions were guarded and the 173 argument descriptions behind them were not.
    ///
    /// **Generated from `get_tools()`, so it cannot drift.** That is the difference from
    /// `crate::args::tests::all_schemas_generate`, which is a hand-maintained list of 19 arg structs and has
    /// already fallen behind the tool surface — `SetMonitorStopArgs`, `EvaluateChainArgs`, `ListClassesArgs`
    /// and others are not in it. A list of what to check is a second place to forget something; walking the
    /// advertised tools is not. That test still stands, and this one subsumes its coverage.
    ///
    /// The whole property schema is rendered rather than a summary of it, minus the description, so a change
    /// to a `format`, a `minimum`, an `anyOf` arm or a `$ref` fails as loudly as a changed type does. The
    /// description follows it word-wrapped, on the same reasoning as the description snapshot: wrapping makes
    /// a corrupted clause a two-line diff and needs no heuristic.
    ///
    /// One honest limit, recorded because the issue weighed it. DOC-7's failure mode was a 4000-character
    /// single-line literal no human reads in a diff; argument descriptions are ordinary multi-line `///`
    /// comments and do diff readably, so this is not the same exposure to a bad merge. What it guards is the
    /// other thing the description snapshot guards — a caller-visible change behind an unchanged tool name
    /// has to be READ by somebody.
    ///
    /// Regenerate with `UPDATE_TOOL_DESCRIPTIONS=1 cargo test --bin jdwp-mcp _snapshot`, the same one
    /// command that regenerates the description snapshot, then read the diff.

    #[test]
    fn argument_schemas_match_the_committed_snapshot() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/argument-schemas.txt");
        let current = render_argument_schema_snapshot();

        if std::env::var_os("UPDATE_TOOL_DESCRIPTIONS").is_some() {
            std::fs::write(&path, &current).expect("write the argument-schema snapshot");
            println!("rewrote {} — read the diff before committing it", path.display());
            return;
        }

        let committed = std::fs::read_to_string(&path).unwrap_or_else(|why| {
            panic!(
                "cannot read the argument-schema snapshot at {}: {why}. Create it with \
                 UPDATE_TOOL_DESCRIPTIONS=1 cargo test --bin jdwp-mcp _snapshot",
                path.display()
            )
        });

        if committed != current {
            let differing =
                committed.lines().zip(current.lines()).enumerate().find(|(_, (was, now))| was != now);
            let first = match differing {
                Some((n, (was, now))) => {
                    format!("line {}:\n  committed: {was}\n  current:   {now}", n + 1)
                }
                None => format!(
                    "no differing line, so the length changed: {} committed vs {} current — an argument was \
                     added or removed, or a description grew past a wrap boundary",
                    committed.lines().count(),
                    current.lines().count()
                ),
            };
            panic!(
                "an argument's type, default or description changed without its snapshot being \
                 updated.\n\n{first}\n\nIf the change was deliberate: UPDATE_TOOL_DESCRIPTIONS=1 cargo test \
                 --bin jdwp-mcp _snapshot, then READ THE DIFF — these are published to callers as \
                 the inputSchema, and a caller-visible change behind an unchanged tool name is what \
                 docs/toolkit-contract.md is for."
            );
        }
    }

    fn render_argument_schema_snapshot() -> String {
        let mut tools = get_tools();
        tools.sort_by(|a, b| a.name.cmp(&b.name));

        // Counted rather than described, and put at the top on purpose: an added or removed argument is then
        // a one-line diff in the header as well as a block further down. `docs/toolkit-contract.md` insists
        // the downstream audit count ARGUMENTS rather than tools — a tool-level check passes while an
        // argument is documented nowhere, which is how `force_initialize` went missing.
        let arg_count: usize = tools
            .iter()
            .filter_map(|t| t.input_schema.get("properties").and_then(serde_json::Value::as_object))
            .map(serde_json::Map::len)
            .sum();
        let mut out = format!(
            "# Every debug.* tool's ARGUMENT schemas, as advertised to callers. GENERATED — do not hand-edit:\n\
             #     UPDATE_TOOL_DESCRIPTIONS=1 cargo test --bin jdwp-mcp _snapshot\n\
             #\n\
             # {} tools, {arg_count} arguments.\n\
             #\n\
             # Each argument shows its full generated schema minus the description — so a changed type,\n\
             # default, format, minimum or anyOf arm fails as loudly as a renamed field — then the description\n\
             # word-wrapped to 110 columns including its 4-space indent. schemars publishes these as the\n\
             # inputSchema, so they are the caller's documentation for every argument.\n\
             #\n\
             # This file exists so that such a change has to be READ by somebody. Five argument descriptions\n\
             # changed in v0.14.1 and nothing in the suite noticed (DOC-8, #120).\n",
            tools.len()
        );
        for tool in tools {
            out.push_str("\n## ");
            out.push_str(&tool.name);
            out.push('\n');

            let required: Vec<&str> = tool
                .input_schema
                .get("required")
                .and_then(serde_json::Value::as_array)
                .map(|r| r.iter().filter_map(serde_json::Value::as_str).collect())
                .unwrap_or_default();
            let props = tool.input_schema.get("properties").and_then(serde_json::Value::as_object);

            match props.filter(|p| !p.is_empty()) {
                // Said explicitly rather than left blank: "no arguments" and "the schema failed to render"
                // must not look the same, which is the rule the rest of this server is built on.
                None => out.push_str("(no arguments)\n"),
                Some(props) => {
                    for (name, schema) in props {
                        let mut shape = schema.clone();
                        let description = shape
                            .as_object_mut()
                            .and_then(|o| o.remove("description"))
                            .and_then(|d| d.as_str().map(str::to_string));
                        let req = if required.contains(&name.as_str()) { " REQUIRED" } else { "" };
                        // `write!` rather than `push_str(&format!(…))`: doctor fails on the latter and
                        // `cargo clippy` does not, which is ADR-0007 in one line.
                        let _ = writeln!(out, "- {name}:{req} {shape}");
                        match description {
                            // An argument with no description is a real finding, not a formatting gap: the
                            // downstream toolkit's audit counts arguments, and that is how `force_initialize`
                            // turned out to be documented nowhere.
                            None => out.push_str("    (NO DESCRIPTION)\n"),
                            Some(d) => {
                                for line in wrap(&d, 106) {
                                    out.push_str("    ");
                                    out.push_str(&line);
                                    out.push('\n');
                                }
                            }
                        }
                    }
                }
            }
        }
        out
    }
}
