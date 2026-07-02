// Debug tools schema definitions
//
// MCP tools for JDWP debugging operations

use crate::protocol::Tool;
use serde_json::json;

pub fn get_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "debug.attach".to_string(),
            description: "Connect to a JVM via JDWP protocol".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "host": {
                        "type": "string",
                        "description": "JVM host (e.g., 'localhost')",
                        "default": "localhost"
                    },
                    "port": {
                        "type": "integer",
                        "description": "JDWP port (e.g., 5005)",
                        "default": 5005
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Connection timeout in milliseconds",
                        "default": 5000
                    }
                },
                "required": ["host", "port"]
            }),
        },
        Tool {
            name: "debug.set_breakpoint".to_string(),
            description: "Set a breakpoint at a specific location".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "class_pattern": {
                        "type": "string",
                        "description": "Class name pattern (e.g., 'com.example.MyClass')"
                    },
                    "line": {
                        "type": "integer",
                        "description": "Line number"
                    },
                    "method": {
                        "type": "string",
                        "description": "Method name (optional). If given without 'line', breaks at the method's first line."
                    },
                    "hit_count": {
                        "type": "integer",
                        "description": "Only stop on the Nth hit (optional)"
                    },
                    "thread_id": {
                        "type": "string",
                        "description": "Only stop when this thread (hex id) hits it (optional)"
                    },
                    "condition": {
                        "type": "string",
                        "description": "Only stop when this boolean expression is true (evaluated in the hit frame). Supports `expr OP literal` (==, !=, <, >, <=, >=) and boolean method chains, e.g. `reserva.getReservaPacote().getReservaHotelList().size() > 0` or `getSgMoeda() == \"BRL\"`."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Target debug session (optional; defaults to the current one)"
                    }
                },
                "required": ["class_pattern"]
            }),
        },
        Tool {
            name: "debug.list_breakpoints".to_string(),
            description: "List all active breakpoints".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        Tool {
            name: "debug.clear_breakpoint".to_string(),
            description: "Clear a specific breakpoint".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "breakpoint_id": {
                        "type": "string",
                        "description": "Breakpoint ID from list_breakpoints"
                    }
                },
                "required": ["breakpoint_id"]
            }),
        },
        Tool {
            name: "debug.continue".to_string(),
            description: "Resume execution (all threads or specific thread)".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "thread_id": {
                        "type": "string",
                        "description": "Thread ID to resume (optional, resumes all if omitted)"
                    }
                }
            }),
        },
        Tool {
            name: "debug.step_over".to_string(),
            description: "Step over current line".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "thread_id": {
                        "type": "string",
                        "description": "Thread ID to step"
                    }
                },
                "required": []
            }),
        },
        Tool {
            name: "debug.step_into".to_string(),
            description: "Step into method call".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "thread_id": {
                        "type": "string",
                        "description": "Thread ID to step"
                    }
                },
                "required": []
            }),
        },
        Tool {
            name: "debug.step_out".to_string(),
            description: "Step out of current method".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "thread_id": {
                        "type": "string",
                        "description": "Thread ID to step"
                    }
                },
                "required": []
            }),
        },
        Tool {
            name: "debug.get_stack".to_string(),
            description: "Get stack frames (compact: one line per frame `#i class.method:line`, locals indented beneath)".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "thread_id": {
                        "type": "string",
                        "description": "Thread ID (optional; defaults to the last thread that hit a breakpoint/step)"
                    },
                    "max_frames": {
                        "type": "integer",
                        "description": "Maximum number of frames to return",
                        "default": 20
                    },
                    "include_variables": {
                        "type": "boolean",
                        "description": "Include local variables under each frame (set false for just the call chain)",
                        "default": true
                    },
                    "package_filter": {
                        "type": "string",
                        "description": "Only show frames whose class name contains this substring (case-insensitive), e.g. your app package 'br.com.infotravel'; framework frames are collapsed into '… N frame(s) hidden'. Big token saver on deep JVM stacks."
                    }
                },
                "required": []
            }),
        },
        Tool {
            name: "debug.evaluate".to_string(),
            description: "Evaluate expression in frame context".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "thread_id": {
                        "type": "string",
                        "description": "Thread ID"
                    },
                    "frame_index": {
                        "type": "integer",
                        "description": "Stack frame index (0 = current frame)",
                        "default": 0
                    },
                    "expression": {
                        "type": "string",
                        "description": "Java expression to evaluate"
                    },
                    "max_result_length": {
                        "type": "integer",
                        "description": "Maximum length of the rendered result string (default 2000; raise for long toString()s)",
                        "default": 2000
                    }
                },
                "required": ["expression"]
            }),
        },
        Tool {
            name: "debug.list_threads".to_string(),
            description: "List threads by name (one `0x<id> <name>` line each). A JVM like WildFly has hundreds of threads — filter with name_filter, and note the last thread that hit a breakpoint is already reported by debug.get_last_event.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name_filter": {
                        "type": "string",
                        "description": "Only threads whose name contains this substring (case-insensitive), e.g. 'Avail' or 'task'"
                    },
                    "only_suspended": {
                        "type": "boolean",
                        "description": "Only threads currently suspended (also appends each thread's run status)",
                        "default": false
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max threads to return (default 40); the rest are reported as a hidden count",
                        "default": 40
                    }
                }
            }),
        },
        Tool {
            name: "debug.pause".to_string(),
            description: "Pause execution (all threads or specific thread)".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "thread_id": {
                        "type": "string",
                        "description": "Thread ID to pause (optional, pauses all if omitted)"
                    }
                }
            }),
        },
        Tool {
            name: "debug.disconnect".to_string(),
            description: "Disconnect from JVM debug session".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        Tool {
            name: "debug.get_last_event".to_string(),
            description: "Get the last breakpoint/event received. Includes a machine-readable [event] line with thread id and source location (class.method:line).".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        Tool {
            name: "debug.panic".to_string(),
            description: "Safety: clear ALL breakpoints and resume ALL threads. Use to unfreeze a JVM if a breakpoint left a thread suspended.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        Tool {
            name: "debug.set_value".to_string(),
            description: "Set a local variable in a suspended frame to a literal (int, long like 123L, true/false, null, or \"string\").".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Local variable name" },
                    "value": { "type": "string", "description": "Literal: int, 123L, true/false, null, or \"string\"" },
                    "thread_id": { "type": "string", "description": "Thread id (optional; defaults to last-hit thread)" },
                    "frame_index": { "type": "integer", "description": "Frame index (default 0)", "default": 0 }
                },
                "required": ["name", "value"]
            }),
        },
    ]
}
