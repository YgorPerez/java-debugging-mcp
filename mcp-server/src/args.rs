// Typed tool arguments — the single source of truth for each tool's parameters.
//
// Each struct derives both `Deserialize` (how handlers read the arguments) and `JsonSchema`
// (how tools.rs advertises the schema to the client). Because both come from ONE definition,
// the advertised schema can't drift from what the handler actually parses — the class of bug
// that left `max_variable_depth`/`timeout_ms` dead and `max_result_length` reporting 500 while
// the code used 4000. Field doc-comments become the schema `description`.

use schemars::JsonSchema;
use serde::Deserialize;

fn default_host() -> String { "localhost".to_string() }
const fn default_port() -> u16 { 5005 }
const fn default_max_frames() -> usize { 20 }
const fn default_true() -> bool { true }
const fn default_max_result_length() -> usize { 2000 }
const fn default_limit() -> usize { 40 }
const fn default_trace_limit() -> usize { 50 }

/// Parse an optional hex thread id like "0x2" (or "2") into a raw id.
pub fn parse_thread_id(s: Option<&str>) -> Option<u64> {
    s.and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
}

/// Deserialize tool arguments into a typed struct, tolerating a missing/`null` arguments value
/// (treated as an empty object so all-optional structs still get their defaults).
pub fn parse<T: serde::de::DeserializeOwned>(args: &serde_json::Value) -> Result<T, String> {
    let v = if args.is_null() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        args.clone()
    };
    serde_json::from_value(v).map_err(|e| format!("Invalid arguments: {e}"))
}

/// Arguments for debug.attach.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AttachArgs {
    /// JVM host (e.g. "localhost").
    #[serde(default = "default_host")]
    pub host: String,
    /// JDWP port (e.g. 5005 or 8787).
    #[serde(default = "default_port")]
    pub port: u16,
}

/// Arguments for `debug.set_breakpoint`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetBreakpointArgs {
    /// Class name pattern (e.g. "com.example.MyClass").
    pub class_pattern: String,
    /// Line number (optional if `method` is given).
    #[serde(default)]
    pub line: Option<i32>,
    /// Method name (optional). If given without `line`, breaks at the method's first line.
    #[serde(default)]
    pub method: Option<String>,
    /// Only stop on the Nth hit (optional).
    #[serde(default)]
    pub hit_count: Option<i32>,
    /// Only stop when this thread (hex id) hits it (optional).
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Only stop when this boolean expression is true, evaluated in the hit frame. Supports
    /// `expr OP expr` (==, !=, <, >, <=, >=) and boolean method chains, e.g.
    /// `reserva.getReservaPacote().getReservaHotelList().size() > 0`, `getSgMoeda() == "BRL"`, or
    /// `total > Config.LIMITE`. Either side may be a literal or an expression; two Strings compare
    /// by content, other objects by identity.
    #[serde(default)]
    pub condition: Option<String>,
    /// Logpoint mode: on hit, snapshot (location, thread, in-scope locals/args, optional
    /// `trace_expr`) into a ring buffer and resume immediately WITHOUT suspending — safe on a
    /// shared instance where a normal breakpoint could freeze a request thread. Read snapshots
    /// with `debug.get_traces`.
    #[serde(default)]
    pub trace: bool,
    /// Only with `trace:true` — an expression to evaluate in the hit frame and record alongside
    /// the snapshot (e.g. `reserva.getStatus()`).
    #[serde(default)]
    pub trace_expr: Option<String>,
    // NOTE: `session_id` is a cross-cutting argument handled uniformly by `resolve_session`
    // (from the raw arguments) for every tool, so it is intentionally not a typed field here.
}

/// Arguments for `debug.get_traces`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTracesArgs {
    /// Max snapshots to return, most recent last (default 50).
    #[serde(default = "default_trace_limit")]
    pub limit: usize,
    /// Clear the trace buffer after returning it.
    #[serde(default)]
    pub clear: bool,
}

/// Arguments for `debug.clear_breakpoint`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClearBreakpointArgs {
    /// Breakpoint ID from `debug.list_breakpoints`.
    pub breakpoint_id: String,
}

/// Arguments for `debug.step_over` / `step_into` / `step_out`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StepArgs {
    /// Thread ID to step (optional; defaults to the last thread that hit a breakpoint).
    #[serde(default)]
    pub thread_id: Option<String>,
}

/// Arguments for `debug.get_stack`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetStackArgs {
    /// Thread ID (optional; defaults to the last thread that hit a breakpoint/step).
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Maximum number of frames to return.
    #[serde(default = "default_max_frames")]
    pub max_frames: usize,
    /// Include local variables under each frame (set false for just the call chain).
    #[serde(default = "default_true")]
    pub include_variables: bool,
    /// Only show frames whose class name contains this substring (case-insensitive), e.g. your
    /// app package 'br.com.infotravel'; framework frames collapse into "… N frame(s) hidden".
    /// Big token saver on deep JVM stacks.
    #[serde(default)]
    pub package_filter: Option<String>,
}

/// Arguments for debug.evaluate.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EvaluateArgs {
    /// Java expression. Heads: a local, `this`, or a class (`ConfigDefaultUtils`, or fully
    /// qualified). Then chain `.field` and `.method(args)` freely, including static members
    /// (`ConfigDefaultUtils.getUrl()`). Arguments may be literals (int, `123L`, true/false, null,
    /// `"string"`) or expressions passed by reference — a local, `this.field`, or a nested call
    /// (`svc.matches(reserva)`, `foo.handle(this, cfg.getId())`).
    pub expression: String,
    /// Thread ID (optional; defaults to the last thread that hit a breakpoint).
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Stack frame index (0 = current frame).
    #[serde(default)]
    pub frame_index: usize,
    /// Maximum length of the rendered result string (raise for long toString()s).
    #[serde(default = "default_max_result_length")]
    pub max_result_length: usize,
}

/// Arguments for `debug.list_threads`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListThreadsArgs {
    /// Only threads whose name contains this substring (case-insensitive), e.g. 'Avail' or 'task'.
    #[serde(default)]
    pub name_filter: Option<String>,
    /// Only threads currently suspended (also appends each thread's run status).
    #[serde(default)]
    pub only_suspended: bool,
    /// Max threads to return; the rest are reported as a hidden count.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// Arguments for `debug.set_value`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetValueArgs {
    /// What to write. Either a local variable name (`counter`), a static field
    /// (`ConfigDefaultUtils.dsInfra` or a fully-qualified `pkg.Class.field`), or an instance field
    /// reached from a suspended frame (`this.status`, `reserva.total`). Accepts the legacy key
    /// `name`.
    #[serde(alias = "name")]
    pub target: String,
    /// Literal: int, 123L, true/false, null, or "string". Coerced to the target's declared type.
    pub value: String,
    /// Thread id (optional; defaults to last-hit thread). Needed for locals and instance fields.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Frame index (default 0). Used for locals and for resolving an instance-field prefix.
    #[serde(default)]
    pub frame_index: usize,
}

/// Arguments for `debug.set_exception_breakpoint`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetExceptionBreakpointArgs {
    /// Exception class to break on (e.g. "java.lang.NullPointerException" or
    /// "br.com.infotravel.ErrorException"); its subclasses match too. Omit to break on ALL
    /// exceptions — noisy, since the JVM throws/catches internally constantly. The class must
    /// already be loaded (trigger it once if unsure).
    #[serde(default)]
    pub class_pattern: Option<String>,
    /// Break on exceptions that ARE caught somewhere up the stack (default true).
    #[serde(default = "default_true")]
    pub caught: bool,
    /// Break on exceptions that are NOT caught (propagate out; default true).
    #[serde(default = "default_true")]
    pub uncaught: bool,
}

/// Arguments for `debug.set_watchpoint`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetWatchpointArgs {
    /// Class declaring the field (e.g. `ConfigDefaultUtils` or a fully-qualified
    /// `br.com.infotravel.util.ConfigDefaultUtils`). Must already be loaded — a watchpoint needs a
    /// concrete field id, so it can't be deferred like a line breakpoint.
    pub class_name: String,
    /// Field to watch (e.g. `dsInfra`, `empresaId`). Inherited fields are found by walking
    /// superclasses; the watch is registered on the class that actually declares it.
    pub field_name: String,
    /// Break on writes (`FIELD_MODIFICATION`) — the default, and what answers "who mutates this?".
    #[serde(default = "default_true")]
    pub modify: bool,
    /// Also break on reads (`FIELD_ACCESS`). Noisy on a hot field; off by default.
    #[serde(default)]
    pub access: bool,
}

/// Arguments for `debug.force_return`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ForceReturnArgs {
    /// Return value literal, coerced to the method's declared return type: int, 123L, true/false,
    /// null, or "string". Omit (or pass "void") for a void method.
    #[serde(default)]
    pub value: Option<String>,
    /// Thread id (optional; defaults to last-hit thread). Must be suspended.
    #[serde(default)]
    pub thread_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression guard: every arg struct produces a valid object schema. Catches derive breakage.
    #[test]
    fn all_schemas_generate() {
        let schemas = [
            serde_json::to_value(schemars::schema_for!(AttachArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(SetBreakpointArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(ClearBreakpointArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(StepArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(GetStackArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(EvaluateArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(ListThreadsArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(SetValueArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(SetExceptionBreakpointArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(SetWatchpointArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(ForceReturnArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(GetTracesArgs)).unwrap(),
        ];
        for s in schemas {
            assert_eq!(s.get("type").and_then(|t| t.as_str()), Some("object"));
        }
    }

    // set_value's target accepts both the new `target` key and the legacy `name` key, so existing
    // callers keep working after the locals→fields generalization.
    #[test]
    fn set_value_accepts_target_and_legacy_name() {
        let by_target: SetValueArgs =
            serde_json::from_value(serde_json::json!({"target": "ConfigDefaultUtils.dsInfra", "value": "DEV"})).unwrap();
        assert_eq!(by_target.target, "ConfigDefaultUtils.dsInfra");

        let by_name: SetValueArgs =
            serde_json::from_value(serde_json::json!({"name": "counter", "value": "5"})).unwrap();
        assert_eq!(by_name.target, "counter");
    }
}
