// Debug tools schema definitions
//
// Tool argument schemas are generated from the typed structs in `crate::args` (schemars), so the
// advertised schema always matches what the handler deserializes. Tools with no arguments use an
// empty object schema.

use crate::args::*;
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

pub fn get_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "debug.attach".to_string(),
            description: "Connect to a JVM via JDWP protocol".to_string(),
            input_schema: to_val(schemars::schema_for!(AttachArgs)),
        },
        Tool {
            name: "debug.set_breakpoint".to_string(),
            description: "Set a breakpoint at a location (class_pattern + line and/or method). Pass trace:true to make it a non-suspending logpoint that snapshots and resumes instead of freezing the thread — safe on the shared 8180; read snapshots with debug.get_traces.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetBreakpointArgs)),
        },
        Tool {
            name: "debug.list_breakpoints".to_string(),
            description: "List all active breakpoints".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.clear_breakpoint".to_string(),
            description: "Clear a specific breakpoint".to_string(),
            input_schema: to_val(schemars::schema_for!(ClearBreakpointArgs)),
        },
        Tool {
            name: "debug.continue".to_string(),
            description: "Resume execution (all threads)".to_string(),
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
            name: "debug.get_stack".to_string(),
            description: "Get stack frames (compact: one line per frame `#i class.method:line`, locals indented beneath)".to_string(),
            input_schema: to_val(schemars::schema_for!(GetStackArgs)),
        },
        Tool {
            name: "debug.evaluate".to_string(),
            description: "Evaluate expression in frame context".to_string(),
            input_schema: to_val(schemars::schema_for!(EvaluateArgs)),
        },
        Tool {
            name: "debug.list_threads".to_string(),
            description: "List threads by name (one `0x<id> <name>` line each). A JVM like WildFly has hundreds of threads — filter with name_filter, and note the last thread that hit a breakpoint is already reported by debug.get_last_event.".to_string(),
            input_schema: to_val(schemars::schema_for!(ListThreadsArgs)),
        },
        Tool {
            name: "debug.pause".to_string(),
            description: "Pause execution (suspend all threads)".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.disconnect".to_string(),
            description: "Disconnect from JVM debug session".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.get_last_event".to_string(),
            description: "Get the last breakpoint/event received. Includes a machine-readable [event] line with thread id and source location (class.method:line).".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.panic".to_string(),
            description: "Safety: clear ALL breakpoints and resume ALL threads. Use to unfreeze a JVM if a breakpoint left a thread suspended.".to_string(),
            input_schema: empty(),
        },
        Tool {
            name: "debug.set_value".to_string(),
            description: "Write a value to a local variable, a static field (e.g. ConfigDefaultUtils.dsInfra — flip tenant/infra on a live JVM without a restart), or an instance field (this.status, reserva.total). Value is a literal (int, long like 123L, true/false, null, or \"string\") coerced to the target's declared type. Locals/instance fields need a suspended thread; statics don't.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetValueArgs)),
        },
        Tool {
            name: "debug.get_traces".to_string(),
            description: "Return snapshots captured by trace/logpoint breakpoints (debug.set_breakpoint with trace:true): each shows where it fired, the thread, in-scope locals/args, and any trace_expr result. Bounded ring buffer (most recent kept). Pass clear:true to empty the buffer after reading.".to_string(),
            input_schema: to_val(schemars::schema_for!(GetTracesArgs)),
        },
        Tool {
            name: "debug.set_exception_breakpoint".to_string(),
            description: "Break when an exception is thrown. Give class_pattern (e.g. java.lang.NullPointerException or a custom ErrorException) to target one type + its subclasses — ideal for silent-catch bugs where a swallowed exception hides the failure. Omit class_pattern to catch ALL exceptions (noisy). caught/uncaught select which throws to report. The hit is reported via debug.get_last_event with the exception type + throw/catch location.".to_string(),
            input_schema: to_val(schemars::schema_for!(SetExceptionBreakpointArgs)),
        },
        Tool {
            name: "debug.force_return".to_string(),
            description: "Force the current method (top frame of a suspended thread) to return immediately with the given value, skipping the rest of its body — e.g. make a rejecting salvar() return true without redeploying. Value is coerced to the method's return type; omit for void. Then debug.continue.".to_string(),
            input_schema: to_val(schemars::schema_for!(ForceReturnArgs)),
        },
    ]
}
