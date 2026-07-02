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
fn default_port() -> u16 { 5005 }
fn default_max_frames() -> usize { 20 }
fn default_true() -> bool { true }
fn default_max_result_length() -> usize { 2000 }
fn default_limit() -> usize { 40 }

/// Parse an optional hex thread id like "0x2" (or "2") into a raw id.
pub fn parse_thread_id(s: &Option<String>) -> Option<u64> {
    s.as_deref()
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
}

/// Deserialize tool arguments into a typed struct, tolerating a missing/`null` arguments value
/// (treated as an empty object so all-optional structs still get their defaults).
pub fn parse<T: serde::de::DeserializeOwned>(args: &serde_json::Value) -> Result<T, String> {
    let v = if args.is_null() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        args.clone()
    };
    serde_json::from_value(v).map_err(|e| format!("Invalid arguments: {}", e))
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

/// Arguments for debug.set_breakpoint.
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
    /// `expr OP literal` (==, !=, <, >, <=, >=) and boolean method chains, e.g.
    /// `reserva.getReservaPacote().getReservaHotelList().size() > 0` or `getSgMoeda() == "BRL"`.
    #[serde(default)]
    pub condition: Option<String>,
    // NOTE: `session_id` is a cross-cutting argument handled uniformly by `resolve_session`
    // (from the raw arguments) for every tool, so it is intentionally not a typed field here.
}

/// Arguments for debug.clear_breakpoint.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClearBreakpointArgs {
    /// Breakpoint ID from debug.list_breakpoints.
    pub breakpoint_id: String,
}

/// Arguments for debug.step_over / step_into / step_out.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct StepArgs {
    /// Thread ID to step (optional; defaults to the last thread that hit a breakpoint).
    #[serde(default)]
    pub thread_id: Option<String>,
}

/// Arguments for debug.get_stack.
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
    /// Java expression to evaluate.
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

/// Arguments for debug.list_threads.
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

/// Arguments for debug.set_value.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetValueArgs {
    /// Local variable name.
    pub name: String,
    /// Literal: int, 123L, true/false, null, or "string".
    pub value: String,
    /// Thread id (optional; defaults to last-hit thread).
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Frame index (default 0).
    #[serde(default)]
    pub frame_index: usize,
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
        ];
        for s in schemas {
            assert_eq!(s.get("type").and_then(|t| t.as_str()), Some("object"));
        }
    }
}
