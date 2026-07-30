// Debug tools schema definitions
//
// Tool argument schemas are generated from the typed structs in `crate::args` (schemars), so the
// advertised schema always matches what the handler deserializes. Tools with no arguments use an
// empty object schema.

use crate::args::{
    AttachArgs, CheckStaleArgs, ClearBreakpointArgs, EvaluateArgs, EvaluateChainArgs, ForceReturnArgs,
    GetLastEventArgs, GetStackArgs, GetTracesArgs, LaunchArgs, ListClassesArgs, ListFieldsArgs,
    ListMethodsArgs, ListThreadsArgs, PopFrameArgs, ReloadClassArgs, SetBreakpointArgs,
    SetExceptionBreakpointArgs, SetMethodBreakpointArgs, SetValueArgs, SetWatchpointArgs, SourceArgs,
    StepArgs, ThreadDumpArgs, ToggleBreakpointArgs,
};
use crate::protocol::Tool;
use serde_json::json;

/// Convert a schemars-generated schema into the JSON value the MCP protocol carries.
fn to_val(s: schemars::Schema) -> serde_json::Value {
    serde_json::to_value(s).unwrap_or_else(|_| json!({"type": "object", "properties": {}}))
}

/// Schema for a tool that takes no arguments.
fn empty() -> serde_json::Value {
    json!({"type": "object", "properties": {}})
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
            description: "List every live debug session — its host:port, whether it is the current one (all tools default to that), whether it is suspended, how many stop points/traces/events it holds, and how many JDWP packets it has cost. Use it when you have lost a session_id, or to check what is still attached before walking away. A session whose JVM has gone is shown as DEAD. A session that has hot-reloaded a class is flagged with the count, deliberately regardless of whose session it is: a session someone ELSE left behind is the case that matters, and this listing is the only place a third party can discover that a JVM is running installed bytecode.".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.disconnect".to_string(),
            description: "Disconnect from a JVM debug session, leaving the JVM RUNNING with nothing armed: it clears every event request and resumes every thread in one round trip before dropping the session, so disconnecting while suspended at a breakpoint cannot freeze the debuggee forever (SAFE-1). Reports whether the VM had been suspended, and names any class this session installed with debug.reload_class — that outlives the session and only a redeploy restores it, so this is the last moment anyone is told.".to_string(),
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
            description: "Set a breakpoint at a location (class_pattern + line and/or method). class_pattern takes AN EXACT CLASS, A WILDCARD, OR A LIST OF EITHER. An exact name arms one breakpoint (deferred on a CLASS_PREPARE watch if the class isn't loaded yet, arming itself when it loads). A wildcard (com.example.*, *.OrderService, *Order*) requires `method` and REFUSES `line` — :412 is a different statement in every class it matches — and arms one breakpoint per matching loaded class, each with its own bp_ id, PLUS a watch that arms matching classes loading later: the answer to \"break at the entry of handle on every implementation of this interface\" or on a generated proxy whose exact name you cannot predict. The family is addressable as one bpset_ id, so debug.clear_stop_point / debug.toggle_stop_point can drop or silence all of it — including the watch — in one call, and the individual bp_ ids still work on their own. Bounded by max_classes (default 20): the reply says how many classes it armed and what it left out, because that count is the one thing a wildcard hides from you. The cap bounds the COST as well as the count — a family that is full parks its class-load watch instead of paying for an event on every class load it could only refuse, and the reply says when that has happened; clearing a member frees a slot and it starts watching again by itself. A list (['com.example.Order', 'com.example.*Repo']) resolves each entry independently and reports every entry's outcome — 2 armed, 1 deferred, 1 refused is a normal batch result, so nothing is aborted by one entry failing. Pass trace:true to make it a non-suspending logpoint that snapshots and resumes instead of freezing the thread — the right choice on the shared 8180; read snapshots with debug.get_traces. It does not FREEZE the VM, which is not the same as not slowing it: capture is serialised, so a traced stop point tops out at ~720 hits/s (~1160 with trace_frames:0) and hits past that queue. Under a few hundred hits/s that is nearly free; trace_max_hits (default 200) keeps even a hot line to a sub-second blip. A traced hit also records the calling chain (trace_frames, default 3) as class.method:line, so you can see which path reached it. Captured values are truncated AT CAPTURE TIME — 100 chars per in-scope local, 200 for the trace_expr result — and the cut string is what the buffer stores, so debug.get_traces can never recover the rest; raise both with trace_max_length (ceiling 4000) when the thing you are tracing is a JSON body, a SOAP envelope or a built SQL string. A request above the ceiling is clamped and the reply says so. debug.list_stop_points then reports what this stop point is actually costing on this JVM, so you need not take the ~720 figure on trust. STALE BYTECODE IS REPORTED UNASKED: if a class root is configured (debug.attach {class_roots}, JDWP_CLASS_ROOTS) the reply appends a warning when the JVM's line table for the method you just armed does not match your compiled .class — the case where a breakpoint at :412 resolves against last week's build and then never fires, or fires with locals that make no sense for the code you are reading, which is indistinguishable from a wrong hypothesis. It speaks ONLY when it has a proof: no class root, no class file, or no line table on either side all stay silent rather than guessing, and a silent reply is not a claim that your build is current — ask debug.check_stale for that. Costs no extra JDWP packets, because arming already read the line table.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetBreakpointArgs)),
        },
        Tool {
            name: "debug.set_exception_stop".to_string(),
            description: "Break when an exception is thrown. Give class_pattern (e.g. java.lang.NullPointerException or a custom ErrorException) to target one type + its subclasses — ideal for silent-catch bugs where a swallowed exception hides the failure. Omit class_pattern to catch ALL exceptions (noisy). class_pattern also takes a WILDCARD (*.ValidationException) or a LIST (['java.lang.IllegalStateException', '*.TimeoutException']), arming one exc_ per resolved class and reporting each — bounded by max_classes. NOTE what a wildcard can and cannot do here: an exception request needs a concrete reference type (JDWP has no ClassMatch for this event kind, which is also why none of these can be deferred), so a wildcard matches only classes LOADED NOW and nothing arms itself later. An exception class the JVM has not needed yet is invisible to it — trigger it once, then arm. caught/uncaught select which throws to report. The hit is reported via debug.get_last_event with the exception type, its message, and the throw/catch location. The message is often the whole answer: on JDK 15+ a NullPointerException says which subexpression was null (\"because the return value of X.getY() is null\"), which is what you would otherwise bisect by hand with debug.evaluate. Reported in trace mode too — normally read straight off the exception with no invocation, and for a plain java.lang.NullPointerException (whose message the JVM computes on demand and never stores) by one bounded getMessage() call, which is the JDK's own native computation and runs no application code. Pass trace:true to collect throws WITHOUT suspending — required on a shared instance, where the default freezes every thread on each throw; read them with debug.get_traces. Not suspending is not the same as not costing anything: capture is serialised at ~720 hits/s (~1160 with trace_frames:0), so a throw site firing thousands of times a second gets throttled, and trace_max_hits:0 makes that sustained rather than a blip. A traced throw also records the calling chain (trace_frames, default 3), which is usually the actual question for a swallowed exception: which request path reached the catch. Captured values are truncated AT CAPTURE TIME — 100 chars per in-scope local, 200 for the trace_expr result — and the cut string is what the buffer stores, so debug.get_traces can never recover the rest; raise both with trace_max_length (ceiling 4000). A request above the ceiling is clamped and the reply says so. On a framework that rethrows — an EJB interceptor chain, a Spring proxy — one exception instance throws many times, so those sightings are FOLDED: the original throw and the point where it escapes are both kept, the layers between become a `↻ rethrow of #<seq> (+N collapsed)` note on the escaping record, and a collapsed rethrow does not spend trace_max_hits. Without that a budget of 30 was gone on one instance walking WildFly's interceptors, and the only informative record was the 9th. debug.list_stop_points reports the cost this request is actually incurring once throws have landed.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetExceptionBreakpointArgs)),
        },
        Tool {
            name: "debug.set_field_stop".to_string(),
            description: "Break when a field is read or written — answers \"who mutates this?\" for a field that changes behind your back (a config flag, an id, a status). Give class_name + field_name; modify:true (default) breaks on writes and reports the mutating location with old → new value, access:true also breaks on reads (noisy). The class must already be loaded — watchpoints can't be deferred. class_name also takes a WILDCARD (com.example.*) or a LIST, arming one watch per matching loaded class that actually HAS the field; a class that matches but declares no such field is reported, not treated as an error, since that is the expected majority for a broad pattern. Bounded by max_classes, and keep it narrow for a reason stronger than noise: a watched field cannot be JIT-optimised, so a wildcard de-optimises the field in EVERY class it armed. Hits come back via debug.get_last_event; pass trace:true to collect them WITHOUT suspending (required on a shared instance) and read them with debug.get_traces — non-suspending, but still ~720 captures/s at most, so a field written thousands of times a second will be throttled unless trace_max_hits (default 200) stops it first. A traced hit also records the calling chain (trace_frames, default 3), so \"who mutates this?\" is answered with the path that got there, not just the innermost setter. Captured values are truncated AT CAPTURE TIME — 100 chars per in-scope local, 200 for the old → new pair and the trace_expr result — and the cut string is what the buffer stores, so debug.get_traces can never recover the rest; raise all of them with trace_max_length (ceiling 4000), which is what you need when the watched field holds a payload rather than a flag. A request above the ceiling is clamped and the reply says so. debug.list_stop_points reports what the watch is actually costing once hits have landed. A watched field can't be JIT-optimised, so clear it when done.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetWatchpointArgs)),
        },
        Tool {
            name: "debug.set_method_exit_stop".to_string(),
            description: "Report what a method actually RETURNED, without having to guess which `return` statement runs — for a method with several returns, or one whose value comes from a chain you can't easily break on. Each hit gives the return site (so you know which path was taken) plus the returned value. Give class_pattern + method; the method filter is applied our side because JDWP has no method-name modifier, so omitting it reports every method of the class. class_pattern takes a leading/trailing * and now also a LIST (['*.OrderService', '*.PaymentService']), arming one mexit_ per pattern. A wildcard costs nothing extra here and needs no expansion — the JVM does the matching, so one request covers every class the pattern matches, including classes that load later. That is why this tool had pattern support when the others did not. UNLIKE the other stop points, trace defaults to TRUE: a suspending method exit on a hot method is the fastest way to freeze a shared JVM, so trace:false is refused unless you name one concrete class AND one method. Read hits with debug.get_traces (trace) or debug.get_last_event (suspending). The returned value is truncated AT CAPTURE TIME at 200 chars (100 for each in-scope local), and the cut string is what the buffer stores, so debug.get_traces can never recover the rest — raise both with trace_max_length (ceiling 4000) when the method returns a JSON/XML payload rather than a status. A request above the ceiling is clamped and the reply says so. Composes with thread_id, trace_max_hits and trace_frames; debug.list_stop_points reports what a traced request is actually costing once hits have landed. A JVM below JDWP 1.6 degrades to the return site without the value, and says so.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetMethodBreakpointArgs)),
        },
        Tool {
            name: "debug.list_stop_points".to_string(),
            description: "List all active breakpoints, including deferred ones, exception breakpoints, field watchpoints, and wildcard families (bpset_…). A family's line is the only place you can learn what a wildcard has BECOME since you armed it: how many breakpoints it holds now, which classes it armed after the reply you read, and whether it has stopped taking new ones because it is full at max_classes — a full family also PARKS its class-load watch, so it costs the JVM nothing while it is full and starts watching again by itself the moment you clear a member. The line distinguishes all four watch states, because they answer \"will this catch the class my next deployment generates?\" differently: watching (yes), parked because full (not until a slot frees), disabled (not until you re-arm the family), and could-not-be-registered (never). Each traced (non-suspending) stop point also reports what it has ACTUALLY cost: the mean capture per hit, the rate hits are arriving at, and the share of the window spent capturing — measured on your JVM rather than taken from the ~720 hits/s figure in the other tools' descriptions, which is 1/mean and so recoverable from the mean reported here. Call it after arming a trace on a hot site to find out whether it is hurting the instance. A traced stop point that has captured nothing says so explicitly rather than reporting zero.".to_string(),
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
            description: "Safety: clear ALL stop points — breakpoints, exception breakpoints, watchpoints AND method-exit requests, traced or not — and resume ALL threads. Use to unfreeze a JVM if a breakpoint left a thread suspended. Method-exit requests matter most here: a suspending one on a hot method re-freezes the VM on the very next return, so resuming without clearing them would be no rescue at all. ONE THING IT CANNOT PUT BACK: a class installed by debug.reload_class keeps serving that bytecode through the panic and after you disconnect, to everyone else on the instance, until the artifact is redeployed — so the reply NAMES any such class rather than letting a clean-looking result imply the JVM is as you found it.".to_string(),
            input_schema: empty(),
        },
    ]
}

/// Driving the VM forward, and changing what it does next.
fn execution_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "debug.continue".to_string(),
            description: "Resume the WHOLE VM after a suspending hit, a debug.pause, or a step — the call the debuggee is waiting for while you inspect it, and on a shared instance the call that gives other people's requests back. It resumes for REAL rather than issuing one resume and hoping: JDWP suspensions are COUNTED, so a debug.pause on top of a breakpoint hit needs more than one resume, and the JVM will happily acknowledge a resume that left the VM still frozen — so this clears the whole suspend depth, verifies the VM is running, and tells you when it could NOT get it running instead of reporting a rescue that didn't happen (SAFE-7). It also drops any pending single-step request first, which would otherwise re-fire the instant threads run again. It does NOT clear your stop points: the next hit suspends the VM again, which is what you want when you are stepping through hits and is a trap when you have walked away — debug.panic is the one that clears everything and resumes, and the watchdog (JDWP_WATCHDOG_SECS, default 120) is what happens if you forget both, resuming the VM and disabling the stop point that froze it so the freeze can't immediately recur.".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.pause".to_string(),
            description: "Suspend EVERY THREAD IN THE JVM, wherever they happen to be — no location, no thread argument, no filter, which makes this the bluntest call in this server. On a shared instance it freezes every in-flight request for as long as you hold it, including requests nobody told you about, and it is the call that has actually frozen a VM here: a forgotten debug.pause is why the watchdog was extended to cover manual pauses (SAFE-4). It ends exactly three ways — debug.continue, debug.panic, or the watchdog auto-resuming after JDWP_WATCHDOG_SECS (default 120; the reply states the number, and with JDWP_WATCHDOG_SECS=0 nothing will ever resume it and the reply says that instead). Before reaching for it, note what does the same job without holding the VM: debug.thread_dump with suspend:true takes its own bounded suspension (max_suspend_ms, default 2000) and verifies the resume, which covers the main honest use of a pause — \"it's wedged, who is blocked on what?\" — and any stop point with trace:true snapshots without suspending at all. Use this when you need the VM held while you ask several questions of it, and on a JVM you own. Idempotent on purpose: pausing an already-suspended VM would build a suspend depth that one debug.continue cannot undo, so it reports what is already holding the VM, how long it has been held, and changes nothing (SAFE-7).".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.step_over".to_string(),
            description: "Run the current line to completion and stop on the next line of the SAME frame — anything that line calls runs without stopping inside it. Needs a thread already suspended: the one from the last hit, or thread_id. STEPPING HOLDS THE WHOLE VM, AND HOLDS IT BETWEEN CALLS: the step resumes every thread, and JDWP's step event suspends every thread again when it lands, so a stepping session is a chain of full-VM freezes with your thinking time inside each one. On a shared instance that is the most expensive thing this server can do — every other request is stopped while you read the reply — so step only on a JVM you own, or accept that cost knowingly. Each freeze ends only with debug.continue, debug.panic, or the watchdog (JDWP_WATCHDOG_SECS, default 120), which will resume the VM out from under you mid-step. Stepping is also the one thing a snapshot cannot replace, so it is worth reserving for that: if you only need to know which path reached a line and what state it saw, a trace:true stop point records exactly that plus its caller chain and never suspends anything. Only one step request is live at a time — a new step clears the previous one, and debug.continue drops it. THE REPLY DOES NOT SAY WHERE IT STOPPED: call debug.get_last_event for the new location.".to_string(),
            input_schema: to_val(schemars::schema_for!(StepArgs)),
        },
        Tool {
            name: "debug.step_into".to_string(),
            description: "Stop at the FIRST LINE OF THE METHOD the current line calls — how you get inside a call whose behaviour is the question — falling through to the next line if it calls nothing. Needs a thread already suspended: the one from the last hit, or thread_id. It steps into framework, proxy and JDK code just as readily as your own, so a line with several calls can land somewhere you did not mean and cost several more steps to escape (debug.step_out); a line breakpoint in the method you actually want is often one call instead of five, and debug.list_methods shows you the name to aim at. STEPPING HOLDS THE WHOLE VM, AND HOLDS IT BETWEEN CALLS: the step resumes every thread, and JDWP's step event suspends every thread again when it lands, so a stepping session is a chain of full-VM freezes with your thinking time inside each one — on a shared instance every other request is stopped while you read the reply. Each freeze ends only with debug.continue, debug.panic, or the watchdog (JDWP_WATCHDOG_SECS, default 120), which will resume the VM out from under you mid-step. Only one step request is live at a time — a new step clears the previous one. THE REPLY DOES NOT SAY WHERE IT STOPPED: call debug.get_last_event for the new location.".to_string(),
            input_schema: to_val(schemars::schema_for!(StepArgs)),
        },
        Tool {
            name: "debug.step_out".to_string(),
            description: "Run the current method to its return and stop at the CALL SITE in the caller's frame — the way out of a method you have seen enough of, or out of framework code debug.step_into landed you in. Needs a thread already suspended: the one from the last hit, or thread_id. It does NOT report what the method returned; debug.set_method_exit_stop is the tool that answers that, it names WHICH return statement ran, and it does it without suspending anything. STEPPING HOLDS THE WHOLE VM, AND HOLDS IT BETWEEN CALLS: the step resumes every thread, and JDWP's step event suspends every thread again when it lands, so a stepping session is a chain of full-VM freezes with your thinking time inside each one — on a shared instance every other request is stopped while you read the reply. Each freeze ends only with debug.continue, debug.panic, or the watchdog (JDWP_WATCHDOG_SECS, default 120), which will resume the VM out from under you mid-step. Only one step request is live at a time — a new step clears the previous one. THE REPLY DOES NOT SAY WHERE IT STOPPED: call debug.get_last_event for the new location.".to_string(),
            input_schema: to_val(schemars::schema_for!(StepArgs)),
        },
        Tool {
            name: "debug.set_value".to_string(),
            description: "Write a value to a local variable, a static field (e.g. ConfigDefaultUtils.dsInfra — flip tenant/infra on a live JVM without a restart), an instance field (this.status, reserva.total), or one element of an array/List/Map (numbers[0], counts[\"key\"] — via ArrayReference.SetValues, List.set or Map.put, reporting the value it displaced). Value is either a literal (int, long like 123L, true/false, null, or \"string\") coerced to the target's declared type, OR another live expression whose value is copied by reference (this.cfg = other.cfg, reserva.cliente = clienteValido) — a type-incompatible source is refused, naming both types. Locals, instance fields and elements need a suspended thread; statics don't.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetValueArgs)),
        },
        Tool {
            name: "debug.force_return".to_string(),
            description: "Force the current method (top frame of a suspended thread) to return immediately with the given value, skipping the rest of its body — e.g. make a rejecting salvar() return true without redeploying. Value is coerced to the method's return type; omit for void. Then debug.continue.".to_string(),
            input_schema: to_val(schemars::schema_for!(ForceReturnArgs)),
        },
        Tool {
            name: "debug.reload_class".to_string(),
            description: "HOT RELOAD: ship a freshly compiled .class into the running JVM and have it replace the loaded one, with no redeploy and no restart — JDWP's RedefineClasses, what an IDE calls \"reload changed classes\". Warm state, connection pools, the app context and any in-flight request all survive, including a request suspended at a breakpoint: change the method, debug.pop_frame, debug.continue, and the fix is exercised without re-issuing the call that got you there. Compiling is still yours (mvn compile / gradle classes); this reads the OUTPUT. Give class_name (must already be loaded) and the bytes are looked for at <class root>/<package as directories>/<SimpleName>.class — roots come from debug.attach {class_roots:[...]} or JDWP_CLASS_ROOTS, class_roots on the call overrides both, and class_file names one file directly. THE LIMIT THAT MATTERS: HotSpot accepts METHOD BODY changes only. Add or remove a method or a field, change a signature, a modifier or the hierarchy, and the JVM refuses — the reply says which of those you did and that a real redeploy is the only route, rather than leaving you to re-try a swap that can never land. A refusal changes nothing: redefinition is all-or-nothing. Also reports whether the thread you are stopped on is INSIDE the class, because a frame already on the stack keeps running the bytecode it entered with. dry_run:true reports what would be shipped and sends nothing. Refused in a read-only session (dry_run still works) — on a shared instance this is an unannounced deploy, not a debugger read.".to_string(),
            input_schema: to_val(schemars::schema_for!(ReloadClassArgs)),
        },
        Tool {
            name: "debug.pop_frame".to_string(),
            description: "Rewind a suspended thread to the CALL SITE of a method it is running: the frame is discarded, the operand stack restored, and debug.continue re-executes the call. Two uses. After debug.reload_class, it is how the new bytecode actually gets entered — a frame already on the stack keeps the code it entered with, so a swap of the very method you are stopped in looks like it did nothing until the frame is popped. On its own, it re-runs a method you stepped through, with locals or fields you have since changed via debug.set_value. frame is indexed as debug.get_stack numbers them (0 = innermost), and every frame above the one you name goes too — that is JDWP's behaviour, not a convenience. Needs a suspended thread and canPopFrames; a native frame in the way (OPAQUE_FRAME) and the outermost frame both refuse, and the reply says which. WHAT IT DOES NOT UNDO: side effects. Anything the popped invocation already wrote to a field, a file, a queue or the network stays written — only the frame is rewound. Refused in a read-only session.".to_string(),
            input_schema: to_val(schemars::schema_for!(PopFrameArgs)),
        },
    ]
}

/// Reading what the VM is doing: where it stopped, its stacks, threads, and values.
fn inspection_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "debug.get_last_event".to_string(),
            description: "Get the last breakpoint/event received. Includes a machine-readable [event] line with thread id and source location (class.method:line); for an exception hit the type, its message and the catch location, and for a watchpoint hit the field with its old → new value. On JDK 15+ an NPE's message names the failing subexpression itself (\"because the return value of X.getY() is null\"), so it is usually the diagnosis rather than a restatement of the type. Absent when the exception carries no message — the key is omitted rather than reported empty. Events are buffered, so a burst of hits isn't lost: the reply says how many older ones are pending — pass limit to read them (oldest first), drain:true to discard what you've read.".to_string(),
            input_schema: to_val(schemars::schema_for!(GetLastEventArgs)),
        },
        Tool {
            name: "debug.get_stack".to_string(),
            description: "Get stack frames (compact: one line per frame `#i class.method:line`, locals indented beneath). Objects show as `Type (id=…)` by default; pass expand_objects:true to expand each local into a field tree (with max_depth / max_children) — costly, so narrow the stack with max_frames/package_filter first.".to_string(),
            input_schema: to_val(schemars::schema_for!(GetStackArgs)),
        },
        Tool {
            name: "debug.evaluate".to_string(),
            description: "Evaluate a Java expression in frame context. Heads: a local, this, or a class name; then chain .field and .method(args), including static fields and static methods (ConfigDefaultUtils.getUrl()). Arguments may be literals or expressions passed by reference (svc.matches(reserva), foo.handle(this)). Method calls need a suspended thread; a plain static-field read does not. Subscripts work on arrays/List/Map: lines[0] (index, keeps chaining), counts[\"key\"] (map lookup), lines[2..5] (half-open slice), lines[?qty > 3] (filter, whose left side resolves against each element; filtering a Map tests its values and keeps the keys as key → value). Pass expand_objects:true to get a recursive field tree instead of one line — it walks nested objects, arrays, and List/Set/Map/Optional contents to max_depth, detects cycles, and unboxes Integer/Long/etc. NOTE: the DEFAULT rendering calls the value's toString() in the debuggee, and on some framework objects that cannot complete (it may need a lock held by another suspended thread) — it is bounded to 2s and the expiry is reported in the value. expand_objects:true reads FIELDS and invokes nothing, so on those objects it is both faster and more informative than the default.".to_string(),
            input_schema: to_val(schemars::schema_for!(EvaluateArgs)),
        },
        Tool {
            name: "debug.evaluate_chain".to_string(),
            description: "Answer \"WHICH LINK of this chain went null?\" in one call. Takes the same chained expression debug.evaluate takes (a.getB().getC()[0].getD()) and walks it left to right, printing every link with its value and naming the first one that is null — plus how many links after it were never evaluated. Use it when a chain yields null or an empty collection and you want to know how far down the value survived; that otherwise costs one debug.evaluate per link, bisecting by hand. Each method in the chain runs EXACTLY ONCE (links resolve against the previous link's value, not by re-evaluating longer and longer prefixes), and no toString() is invoked. NOTE: if the chain THROWS rather than returning null, you usually don't need this — on JDK 15+ the NullPointerException's own message names the failing subexpression, and debug.set_exception_stop reports it.".to_string(),
            input_schema: to_val(schemars::schema_for!(EvaluateChainArgs)),
        },
        Tool {
            name: "debug.list_threads".to_string(),
            description: "List threads by name (one `0x<id> <name>` line each). A JVM like WildFly has hundreds of threads — filter with name_filter, and note the last thread that hit a breakpoint is already reported by debug.get_last_event. When there are more threads than `limit`, the ones shown are chosen the same way debug.thread_dump chooses them: by NAME FAMILY (the name with digits collapsed, so `task-3` and `task-91` are one family), one thread from each family before a second from any, so no single pool spends every slot. NOT the order the JVM lists them in, which is CREATION order — an app server starts its request pool last, so on a real WildFly the first 40 in that order were all JVM internals and selectors and not one application thread. The reply states the rule when it truncated, names the biggest groups it left out, and reports what it spent: one packet per thread NAME, against a dump's ~8 per thread it shows, so this is still the cheap call to run FIRST to decide what to dump.".to_string(),
            input_schema: to_val(schemars::schema_for!(ListThreadsArgs)),
        },
        Tool {
            name: "debug.list_classes".to_string(),
            description: "List the classes the debuggee has actually LOADED — the first step when you do not already know the fully-qualified name a stop point needs. Only the JVM can answer this for a generated proxy, a shaded/relocated class, or a deployment whose build differs from your checkout. Narrow with filter, matched against the dotted name (com.example.Order), never the JNI signature: prefix 'com.example.*', suffix '*.OrderService', or a bare substring. A JVM like WildFly loads thousands of types, so the reply shows a page and reports matched-against-loaded rather than dumping everything — raise limit or narrow the filter to see more. Array types are excluded unless include_arrays:true. A class the JVM has not loaded yet does not appear at all (classes load on first use), which is NOT the same as the class not existing.".to_string(),
            input_schema: to_val(schemars::schema_for!(ListClassesArgs)),
        },
        Tool {
            name: "debug.list_methods".to_string(),
            description: "List a loaded class's methods with signatures rendered as Java source types (static boolean matches(java.lang.String, int)) — what you need to compose a debug.evaluate call, since overload resolution matches on the runtime types of the arguments you supply, or to check a method name before naming it in debug.set_line_stop. Overloads all appear, so the parameter lists can be compared side by side. static/abstract/native are marked; abstract and native have no body to put a line breakpoint in. Declared methods only unless inherited:true walks the superclass chain (each inherited row says which class it came from). <clinit> is omitted; constructors (<init>) are kept. If the class is not loaded the reply says so and points at debug.list_classes, because JDWP cannot tell a wrong name from a not-yet-loaded one.".to_string(),
            input_schema: to_val(schemars::schema_for!(ListMethodsArgs)),
        },
        Tool {
            name: "debug.list_fields".to_string(),
            description: "List what state a loaded class HOLDS — the other half of debug.list_methods, for when you have a type but no instance: a static holder, a class you are about to breakpoint into, a vendored or shaded class your checkout cannot show you. Each field is rendered the way Java source spells it (static final java.lang.String INFRA, int qty), so static and instance fields are told apart at a glance and the declared type is a name you can use. Statics are listed FIRST, because those are the ones debug.evaluate can read with no instance and no suspended thread. final and volatile are marked too: a final may refuse a debug.set_value and will never fire a debug.set_field_stop, and a volatile is being written by something else. Declared fields only unless inherited:true walks the superclass chain (each inherited row says which class it came from) — note that expanding an actual object shows inherited state either way, so the default here is deliberately the narrower question. Bounded like the other discovery tools: raise limit or narrow with name_filter. It reads NO values — debug.evaluate reads a named static, and expand_objects renders an instance. A class that declares nothing says so as an answer rather than looking like a failed lookup; a class that is not loaded gets the same reply debug.list_methods gives, pointing at debug.list_classes, because JDWP cannot tell a wrong name from a not-yet-loaded one.".to_string(),
            input_schema: to_val(schemars::schema_for!(ListFieldsArgs)),
        },
        Tool {
            name: "debug.source".to_string(),
            description: "What file a loaded class was COMPILED FROM, and — when source roots are configured — the source lines around a given line. Two independent halves. The JVM half needs no local files at all and is the one that settles whether your checkout is the code that is actually running: a class reporting Order.java in a tree where that file was renamed months ago is the answer, and reading local source would never have shown it. A JSR-45 source debug extension (JSP, Kotlin, Groovy) is reported when the class carries one, meaning the .java name is only an intermediate. The disk half turns a stack frame's class.method:line into text: pass line to get a bounded window around it with line numbers (context, default 20 either side) — that is the intended use, since a caller chasing one frame should not pull a 2000-line file into context. whole_file:true returns everything, still capped by max_lines (default 400), and the reply always states which lines of how many it is showing. Roots come from debug.attach {source_roots:[...]} or the JDWP_SOURCE_ROOTS environment variable, and source_roots on the call overrides both ([] reads no file). A root is where the PACKAGE TREE starts: the file is looked up at <root>/<package as directories>/<the name the JVM reported>, which is why an inner class (com.example.Order$Line) correctly resolves to its enclosing Order.java. Plain directories only — sources inside JARs are not read. The failure modes stay distinct: class not loaded, loaded but compiled with no SourceFile attribute (javac -g:none, or a synthetic class), no configured root holds the file, and found-but-unreadable each say something different about what to fix.".to_string(),
            input_schema: to_val(schemars::schema_for!(SourceArgs)),
        },
        Tool {
            name: "debug.check_stale".to_string(),
            description: "Is the JVM running the code you just compiled, or an older build? Compares the line tables the JVM holds for a loaded class against the ones in your freshly compiled .class, method by method. This is the check that stops twenty tool calls being spent debugging the PROGRAM when the fact is that the deployed bytecode is last week's — a breakpoint that never fires, or fires with locals that make no sense for the code you are reading, looks identical to a wrong hypothesis, and nothing else here can tell them apart. Different from debug.source, which settles WHICH FILE a class was compiled from: SourceFile is a compile-time string and is identical across every build of the file, so it cannot see the common case of same class, same Order.java, older bytecode. Bytes come from the same place debug.reload_class reads them — <class root>/<package as directories>/<SimpleName>.class, from debug.attach {class_roots:[...]}, JDWP_CLASS_ROOTS, class_roots on the call, or class_file directly. WHAT THE ANSWER MEANS: it compares line tables, so it catches an edit that MOVED a line (which is what makes a stop point at :412 point somewhere else) and is blind to one that changes a body without moving any line — a clean result means \"no line moved\", not \"byte-for-byte identical\", and the reply says so. A method the running class has and your build does not (or the reverse) is reported separately, because that is a different class shape and not something a hot reload could fix. A class with no line tables at all (javac -g:none, an interface, all-abstract) is reported as CANNOT TELL rather than as a match. Costs one JDWP packet per method, and says how many it spent. PASS bytecode:true FOR THE EDIT LINE TABLES CANNOT SEE (DISC-9): it also compares each method's actual code, catching a body change that moved no line — `x < 5` to `x <= 5`, a changed constant, a swapped operator — which is the commonest edit in a compile-and-retest loop and completely invisible to the default comparison. It is also the only evidence that works on a -g:none build, which has code and no line numbers, turning a CANNOT TELL into a real answer. Costs one more packet per method, so it is opt-in. The reply always names which evidence it used and whether the two agree; a difference in bytecode with matching line tables is called out, together with the one other thing that produces it (a build compiled by a different javac, since constant-pool indices live in the operands — same compiler and same source is byte-identical).".to_string(),
            input_schema: to_val(schemars::schema_for!(CheckStaleArgs)),
        },
        Tool {
            name: "debug.thread_dump".to_string(),
            description: "Every thread's stack in ONE call, plus which monitors each thread holds and which one it is blocked entering — the \"it's wedged, who is blocked on what?\" question, which list_threads (names only) and get_stack (one thread) can't answer. A thread waiting on a lock someone else holds is annotated `← held by 0x<id> \"<name>\"`, so a deadlock cycle is readable off the output. IMPORTANT: JDWP can only read a SUSPENDED thread's stack and locks, so on a running VM every thread comes back unreadable — pass suspend:true to freeze it briefly (it is resumed and verified before the reply) or only_suspended:true to list just the readable ones. It never suspends on its own. Narrow the cost with name_filter / limit / max_frames / package_filter; for the deadlock question alone, monitors_only:true reads the lock graph without the frames. The reply reports how many JDWP packets it spent, WHAT EACH ONE COST on this connection (round trip plus our processing), and, when it suspended, how many milliseconds it held the VM — bounded by max_suspend_ms (default 2000), which truncates loudly rather than silently and tells you what finishing would have cost at the rate it was running. Those figures are measured against the JVM you are attached to, so you never have to judge whether a number measured elsewhere applies: a dump of a 306-thread pool with 60-frame stacks costs ~258 packets and ~65ms at the defaults, or ~1,625 packets and ~700ms for every thread and every frame. Works in a read-only session (it invokes nothing).".to_string(),
            input_schema: to_val(schemars::schema_for!(ThreadDumpArgs)),
        },
        Tool {
            name: "debug.get_traces".to_string(),
            description: "Return snapshots captured by non-suspending trace mode — debug.set_line_stop, debug.set_exception_stop or debug.set_field_stop with trace:true. Each shows where it fired, its calling chain as `← class.method:line` entries (nearest caller first, locations only), the thread, in-scope locals/args, any trace_expr result, plus the exception type/message/catch site or the field's old → new value. A record marked `↻ rethrow of #<seq>` is the escaping end of a rethrow chain; `#<seq>` is the original throw, which is the one with the application frame and the cause. Bounded ring buffer (most recent kept). Narrow with bp_id (one stop point), class_filter (substring), or since (only records newer than a #seq you already saw, for polling). Pass clear:true to empty the buffer after reading.".to_string(),
            input_schema: to_val(schemars::schema_for!(GetTracesArgs)),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "debug.list_stop_points",
            "debug.clear_stop_point",
            "debug.toggle_stop_point",
            "debug.panic",
            "debug.continue",
            "debug.pause",
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
            "debug.source",
            "debug.check_stale",
            "debug.thread_dump",
            "debug.get_traces",
        ] {
            assert!(names.contains(expected), "missing tool: {expected}");
        }
    }
}
