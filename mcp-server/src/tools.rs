// Debug tools schema definitions
//
// Tool argument schemas are generated from the typed structs in `crate::args` (schemars), so the
// advertised schema always matches what the handler deserializes. Tools with no arguments use an
// empty object schema.

use crate::args::{AttachArgs, SetBreakpointArgs, ClearBreakpointArgs, StepArgs, GetStackArgs, EvaluateArgs, ListThreadsArgs, SetValueArgs, GetTracesArgs, SetExceptionBreakpointArgs, SetWatchpointArgs, ForceReturnArgs};
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
            name: "debug.set_breakpoint".to_string(),
            description: "Set a breakpoint at a location (class_pattern + line and/or method). Pass trace:true to make it a non-suspending logpoint that snapshots and resumes instead of freezing the thread — safe on the shared 8180; read snapshots with debug.get_traces.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetBreakpointArgs)),
        },
        Tool {
            name: "debug.set_exception_breakpoint".to_string(),
            description: "Break when an exception is thrown. Give class_pattern (e.g. java.lang.NullPointerException or a custom ErrorException) to target one type + its subclasses — ideal for silent-catch bugs where a swallowed exception hides the failure. Omit class_pattern to catch ALL exceptions (noisy). caught/uncaught select which throws to report. The hit is reported via debug.get_last_event with the exception type + throw/catch location.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetExceptionBreakpointArgs)),
        },
        Tool {
            name: "debug.set_watchpoint".to_string(),
            description: "Break when a field is read or written — answers \"who mutates this?\" for a field that changes behind your back (a config flag, an id, a status). Give class_name + field_name; modify:true (default) breaks on writes and reports the mutating location with old → new value, access:true also breaks on reads (noisy). The class must already be loaded — watchpoints can't be deferred. Hits come back via debug.get_last_event. A watched field can't be JIT-optimised, so clear it when done.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetWatchpointArgs)),
        },
        Tool {
            name: "debug.list_breakpoints".to_string(),
            description: "List all active breakpoints, including deferred ones, exception breakpoints, and field watchpoints".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.clear_breakpoint".to_string(),
            description: "Clear a specific breakpoint, exception breakpoint (exc_…), or watchpoint (watch_…) by its id".to_string(),
            input_schema: to_val(schemars::schema_for!(ClearBreakpointArgs)),
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
            description: "Write a value to a local variable, a static field (e.g. ConfigDefaultUtils.dsInfra — flip tenant/infra on a live JVM without a restart), or an instance field (this.status, reserva.total). Value is a literal (int, long like 123L, true/false, null, or \"string\") coerced to the target's declared type. Locals/instance fields need a suspended thread; statics don't.".to_string(),
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
            description: "Get the last breakpoint/event received. Includes a machine-readable [event] line with thread id and source location (class.method:line); for an exception hit the type and catch location, and for a watchpoint hit the field with its old → new value.".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.get_stack".to_string(),
            description: "Get stack frames (compact: one line per frame `#i class.method:line`, locals indented beneath)".to_string(),
            input_schema: to_val(schemars::schema_for!(GetStackArgs)),
        },
        Tool {
            name: "debug.evaluate".to_string(),
            description: "Evaluate a Java expression in frame context. Heads: a local, this, or a class name; then chain .field and .method(args), including static fields and static methods (ConfigDefaultUtils.getUrl()). Arguments may be literals or expressions passed by reference (svc.matches(reserva), foo.handle(this)). Method calls need a suspended thread; a plain static-field read does not.".to_string(),
            input_schema: to_val(schemars::schema_for!(EvaluateArgs)),
        },
        Tool {
            name: "debug.list_threads".to_string(),
            description: "List threads by name (one `0x<id> <name>` line each). A JVM like WildFly has hundreds of threads — filter with name_filter, and note the last thread that hit a breakpoint is already reported by debug.get_last_event.".to_string(),
            input_schema: to_val(schemars::schema_for!(ListThreadsArgs)),
        },
        Tool {
            name: "debug.get_traces".to_string(),
            description: "Return snapshots captured by trace/logpoint breakpoints (debug.set_breakpoint with trace:true): each shows where it fired, the thread, in-scope locals/args, and any trace_expr result. Bounded ring buffer (most recent kept). Pass clear:true to empty the buffer after reading.".to_string(),
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
            "debug.disconnect",
            "debug.set_breakpoint",
            "debug.set_exception_breakpoint",
            "debug.set_watchpoint",
            "debug.list_breakpoints",
            "debug.clear_breakpoint",
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
            "debug.get_traces",
        ] {
            assert!(names.contains(expected), "missing tool: {expected}");
        }
    }
}
