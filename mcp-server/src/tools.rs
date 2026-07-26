// Debug tools schema definitions
//
// Tool argument schemas are generated from the typed structs in `crate::args` (schemars), so the
// advertised schema always matches what the handler deserializes. Tools with no arguments use an
// empty object schema.

use crate::args::{AttachArgs, SetBreakpointArgs, ClearBreakpointArgs, StepArgs, GetStackArgs, EvaluateArgs, GetLastEventArgs, ListThreadsArgs, SetValueArgs, GetTracesArgs, SetExceptionBreakpointArgs, SetWatchpointArgs, ForceReturnArgs, ToggleBreakpointArgs, ThreadDumpArgs, SetMethodBreakpointArgs};
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
            description: "Connect to a JVM via JDWP protocol".to_string(),
            input_schema: to_val(schemars::schema_for!(AttachArgs)),
        },
        Tool {
            name: "debug.list_sessions".to_string(),
            description: "List every live debug session — its host:port, whether it is the current one (all tools default to that), whether it is suspended, how many stop points/traces/events it holds, and how many JDWP packets it has cost. Use it when you have lost a session_id, or to check what is still attached before walking away. A session whose JVM has gone is shown as DEAD.".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.disconnect".to_string(),
            description: "Disconnect from JVM debug session".to_string(),
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
            description: "Set a breakpoint at a location (class_pattern + line and/or method). Pass trace:true to make it a non-suspending logpoint that snapshots and resumes instead of freezing the thread — the right choice on the shared 8180; read snapshots with debug.get_traces. It does not FREEZE the VM, which is not the same as not slowing it: capture is serialised, so a traced stop point tops out at ~720 hits/s (~1160 with trace_frames:0) and hits past that queue. Under a few hundred hits/s that is nearly free; trace_max_hits (default 200) keeps even a hot line to a sub-second blip. A traced hit also records the calling chain (trace_frames, default 3) as class.method:line, so you can see which path reached it. debug.list_stop_points then reports what this stop point is actually costing on this JVM, so you need not take the ~720 figure on trust.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetBreakpointArgs)),
        },
        Tool {
            name: "debug.set_exception_stop".to_string(),
            description: "Break when an exception is thrown. Give class_pattern (e.g. java.lang.NullPointerException or a custom ErrorException) to target one type + its subclasses — ideal for silent-catch bugs where a swallowed exception hides the failure. Omit class_pattern to catch ALL exceptions (noisy). caught/uncaught select which throws to report. The hit is reported via debug.get_last_event with the exception type + throw/catch location. Pass trace:true to collect throws WITHOUT suspending — required on a shared instance, where the default freezes every thread on each throw; read them with debug.get_traces. Not suspending is not the same as not costing anything: capture is serialised at ~720 hits/s (~1160 with trace_frames:0), so a throw site firing thousands of times a second gets throttled, and trace_max_hits:0 makes that sustained rather than a blip. A traced throw also records the calling chain (trace_frames, default 3), which is usually the actual question for a swallowed exception: which request path reached the catch. debug.list_stop_points reports the cost this request is actually incurring once throws have landed.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetExceptionBreakpointArgs)),
        },
        Tool {
            name: "debug.set_field_stop".to_string(),
            description: "Break when a field is read or written — answers \"who mutates this?\" for a field that changes behind your back (a config flag, an id, a status). Give class_name + field_name; modify:true (default) breaks on writes and reports the mutating location with old → new value, access:true also breaks on reads (noisy). The class must already be loaded — watchpoints can't be deferred. Hits come back via debug.get_last_event; pass trace:true to collect them WITHOUT suspending (required on a shared instance) and read them with debug.get_traces — non-suspending, but still ~720 captures/s at most, so a field written thousands of times a second will be throttled unless trace_max_hits (default 200) stops it first. A traced hit also records the calling chain (trace_frames, default 3), so \"who mutates this?\" is answered with the path that got there, not just the innermost setter. debug.list_stop_points reports what the watch is actually costing once hits have landed. A watched field can't be JIT-optimised, so clear it when done.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetWatchpointArgs)),
        },
        Tool {
            name: "debug.set_method_exit_stop".to_string(),
            description: "Report what a method actually RETURNED, without having to guess which `return` statement runs — for a method with several returns, or one whose value comes from a chain you can't easily break on. Each hit gives the return site (so you know which path was taken) plus the returned value. Give class_pattern + method; the method filter is applied our side because JDWP has no method-name modifier, so omitting it reports every method of the class. UNLIKE the other stop points, trace defaults to TRUE: a suspending method exit on a hot method is the fastest way to freeze a shared JVM, so trace:false is refused unless you name one concrete class AND one method. Read hits with debug.get_traces (trace) or debug.get_last_event (suspending). Composes with thread_id, trace_max_hits and trace_frames; debug.list_stop_points reports what a traced request is actually costing once hits have landed. A JVM below JDWP 1.6 degrades to the return site without the value, and says so.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetMethodBreakpointArgs)),
        },
        Tool {
            name: "debug.list_stop_points".to_string(),
            description: "List all active breakpoints, including deferred ones, exception breakpoints, and field watchpoints. Each traced (non-suspending) stop point also reports what it has ACTUALLY cost: the mean capture per hit, the rate it could sustain before hits queue, and the rate hits are arriving at with the share of the window spent capturing — measured on your JVM rather than taken from the ~720 hits/s figure in the other tools' descriptions. Call it after arming a trace on a hot site to find out whether it is hurting the instance. A traced stop point that has captured nothing says so explicitly rather than reporting zero.".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.clear_stop_point".to_string(),
            description: "Clear a specific breakpoint, exception breakpoint (exc_…), or watchpoint (watch_…) by its id".to_string(),
            input_schema: to_val(schemars::schema_for!(ClearBreakpointArgs)),
        },
        Tool {
            name: "debug.toggle_stop_point".to_string(),
            description: "Silence or re-arm a line breakpoint (bp_…) without losing its condition/trace_expr — disabling clears the JDWP request but keeps the definition, enabling re-arms it at the same location. Pass enabled:false/true, or omit to flip. Handy to quiet a chatty breakpoint on a shared JVM without having to retype it.".to_string(),
            input_schema: to_val(schemars::schema_for!(ToggleBreakpointArgs)),
        },
        Tool {
            name: "debug.panic".to_string(),
            description: "Safety: clear ALL breakpoints, exception breakpoints and watchpoints, and resume ALL threads. Use to unfreeze a JVM if a breakpoint left a thread suspended.".to_string(),
            input_schema: empty(),
        },
    ]
}

/// Driving the VM forward, and changing what it does next.
fn execution_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "debug.continue".to_string(),
            description: "Resume execution (all threads)".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.pause".to_string(),
            description: "Pause execution (suspend all threads)".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.step_over".to_string(),
            description: "Step over current line".to_string(),
            input_schema: to_val(schemars::schema_for!(StepArgs)),
        },
        Tool {
            name: "debug.step_into".to_string(),
            description: "Step into method call".to_string(),
            input_schema: to_val(schemars::schema_for!(StepArgs)),
        },
        Tool {
            name: "debug.step_out".to_string(),
            description: "Step out of current method".to_string(),
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
    ]
}

/// Reading what the VM is doing: where it stopped, its stacks, threads, and values.
fn inspection_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "debug.get_last_event".to_string(),
            description: "Get the last breakpoint/event received. Includes a machine-readable [event] line with thread id and source location (class.method:line); for an exception hit the type and catch location, and for a watchpoint hit the field with its old → new value. Events are buffered, so a burst of hits isn't lost: the reply says how many older ones are pending — pass limit to read them (oldest first), drain:true to discard what you've read.".to_string(),
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
            name: "debug.list_threads".to_string(),
            description: "List threads by name (one `0x<id> <name>` line each). A JVM like WildFly has hundreds of threads — filter with name_filter, and note the last thread that hit a breakpoint is already reported by debug.get_last_event.".to_string(),
            input_schema: to_val(schemars::schema_for!(ListThreadsArgs)),
        },
        Tool {
            name: "debug.thread_dump".to_string(),
            description: "Every thread's stack in ONE call, plus which monitors each thread holds and which one it is blocked entering — the \"it's wedged, who is blocked on what?\" question, which list_threads (names only) and get_stack (one thread) can't answer. A thread waiting on a lock someone else holds is annotated `← held by 0x<id> \"<name>\"`, so a deadlock cycle is readable off the output. IMPORTANT: JDWP can only read a SUSPENDED thread's stack and locks, so on a running VM every thread comes back unreadable — pass suspend:true to freeze it briefly (it is resumed and verified before the reply) or only_suspended:true to list just the readable ones. It never suspends on its own. Narrow the cost with name_filter / limit / max_frames / package_filter; for the deadlock question alone, monitors_only:true reads the lock graph without the frames (measured: 245 packets and 33ms held against 770 and 117ms for the same 60-thread dump with stacks). The reply reports how many JDWP packets it spent and, when it suspended, how many milliseconds it held the VM — bounded by max_suspend_ms (default 2000), which truncates loudly rather than silently. Works in a read-only session (it invokes nothing).".to_string(),
            input_schema: to_val(schemars::schema_for!(ThreadDumpArgs)),
        },
        Tool {
            name: "debug.get_traces".to_string(),
            description: "Return snapshots captured by non-suspending trace mode — debug.set_line_stop, debug.set_exception_stop or debug.set_field_stop with trace:true. Each shows where it fired, its calling chain as `← class.method:line` entries (nearest caller first, locations only), the thread, in-scope locals/args, any trace_expr result, plus the exception type/catch site or the field's old → new value. Bounded ring buffer (most recent kept). Narrow with bp_id (one stop point), class_filter (substring), or since (only records newer than a #seq you already saw, for polling). Pass clear:true to empty the buffer after reading.".to_string(),
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
            "debug.get_last_event",
            "debug.get_stack",
            "debug.evaluate",
            "debug.list_threads",
            "debug.thread_dump",
            "debug.get_traces",
        ] {
            assert!(names.contains(expected), "missing tool: {expected}");
        }
    }
}
