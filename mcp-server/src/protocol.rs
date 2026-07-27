// MCP protocol definitions - minimal and explicit
//
// Based on gamecode-mcp2 pattern: no hidden behavior, all JSON explicit

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// JSON-RPC 2.0 base types
#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

// MCP protocol types
#[derive(Debug, Serialize, Deserialize)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClientCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolsCapability {}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerCapabilities {
    pub tools: ToolsCapability,
    /// Declared so the server may send `notifications/message` (EVT-2). In MCP, log notifications are
    /// a **server** capability — there is no client-side "I accept these" flag to gate on — so what
    /// actually gates emission is the handshake completing, not anything the client advertised.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logging: Option<LoggingCapability>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoggingCapability {}

/// How many unsent notifications may queue before new ones are dropped (EVT-2).
///
/// Sized by the same reasoning as `MAX_EVENTS`: a *suspending* hit holds a thread, so these arrive at
/// human pace, not at trace speed. A client would have to stop reading stdout entirely to reach this,
/// and the drop counter covers that case honestly rather than letting the queue grow.
pub const ALERT_CAPACITY: usize = 64;

/// The outbound half of the stdio transport (EVT-2).
///
/// Every line this process writes — responses and unsolicited notifications alike — goes through one
/// channel to one writer task, and that single writer **is** the interleaving guarantee: a
/// notification produced by the event pump while a response is being written cannot land inside it,
/// because it is not the thing doing the writing. Nothing else may write to stdout.
///
/// Sending never blocks and never fails loudly. The producers are the JDWP event pump and the
/// watchdog, and neither may be made to wait on how fast an MCP client drains its pipe — a debugger
/// that stalls its own event loop because the client is slow is worse than one that drops a hint the
/// caller can still read with `debug.get_last_event`.
#[derive(Clone, Debug)]
pub struct Alerter {
    tx: tokio::sync::mpsc::Sender<String>,
    /// Set once the client has sent `notifications/initialized`. A hit can arrive while `debug.attach`
    /// is still in flight, and emitting before the handshake completes is protocol-illegal.
    armed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Notifications discarded because the queue was full, reported on the next one that gets through
    /// so a client that fell behind never reads the silence as "nothing happened" (SAFE-8's posture).
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Alerter {
    pub fn new(tx: tokio::sync::mpsc::Sender<String>) -> Self {
        Self {
            tx,
            armed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            dropped: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Allow emission. Called when the client confirms the handshake.
    pub fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Emit one `notifications/message`, or account for it if that is not possible.
    ///
    /// Returns whether it was queued, which is what the tests assert on — the caller has nothing
    /// useful to do with the answer, since every failure here is already handled.
    // `data` by reference, not by value: the armed/enabled guard below returns before it would be
    // consumed, and a disarmed alerter is the common case for the whole pre-handshake window.
    pub fn alert(&self, level: &str, data: &serde_json::Value) -> bool {
        use std::sync::atomic::Ordering;
        if !self.armed.load(Ordering::Relaxed) || !alerts_enabled() {
            return false;
        }

        // Fold the drop count into the message that recovers, rather than keeping a separate channel
        // for bad news that would itself need somewhere to go.
        let missed = self.dropped.swap(0, Ordering::Relaxed);
        let mut params = json!({ "level": level, "logger": "jdwp-mcp", "data": data });
        if missed > 0 {
            if let Some(o) = params.as_object_mut() {
                o.insert("droppedSinceLast".to_string(), json!(missed));
            }
        }

        let note = JsonRpcNotification {
            jsonrpc: "2.0".to_string(),
            method: "notifications/message".to_string(),
            params: Some(params),
        };
        let Ok(line) = serde_json::to_string(&note) else {
            return false;
        };

        if self.tx.try_send(line).is_err() {
            // Full, or the writer is gone. Put back what we just took plus this one, so no drop is
            // lost to the swap above.
            self.dropped.fetch_add(missed + 1, Ordering::Relaxed);
            return false;
        }
        true
    }
}

/// `JDWP_ALERTS=0` turns push notifications off entirely, leaving `debug.get_last_event` as the
/// only way to learn about a hit. Same spelling convention as `JDWP_READONLY` / `JDWP_WATCHDOG_SECS`.
pub fn alerts_enabled() -> bool {
    std::env::var("JDWP_ALERTS")
        .map_or(true, |v| !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notifier_with_capacity(n: usize) -> (Alerter, tokio::sync::mpsc::Receiver<String>) {
        let (tx, rx) = tokio::sync::mpsc::channel(n);
        (Alerter::new(tx), rx)
    }

    // EVT-2: a hit can land while `debug.attach` is still in flight, and pushing before the client has
    // confirmed the handshake is a protocol violation rather than an early warning.
    #[test]
    fn nothing_is_pushed_before_the_handshake_completes() {
        let (n, mut rx) = notifier_with_capacity(4);
        assert!(!n.alert("warning", &json!({"event": "breakpoint"})), "must not push unarmed");
        assert!(rx.try_recv().is_err(), "nothing should have been queued");

        n.arm();
        assert!(n.alert("warning", &json!({"event": "breakpoint"})));
        let line = rx.try_recv().expect("armed alerter should queue");
        assert!(line.contains("notifications/message"), "{line}");
        assert!(line.contains("\"logger\":\"jdwp-mcp\""), "{line}");
        assert!(line.contains("\"event\":\"breakpoint\""), "payload must survive: {line}");
        // A notification carries no id — that is what distinguishes it from a response.
        assert!(!line.contains("\"id\""), "a notification must have no id: {line}");
    }

    // EVT-2 + SAFE-8: a client that stops reading must not be able to grow the queue, and the drops
    // must be reported rather than leaving the silence to be read as "nothing happened".
    #[test]
    fn a_full_queue_drops_and_reports_what_it_dropped() {
        let (n, mut rx) = notifier_with_capacity(2);
        n.arm();
        assert!(n.alert("warning", &json!({"i": 1})));
        assert!(n.alert("warning", &json!({"i": 2})));
        // Full now: these are dropped, not queued, and above all they do not block.
        assert!(!n.alert("warning", &json!({"i": 3})));
        assert!(!n.alert("warning", &json!({"i": 4})));

        // Drain, freeing capacity, and the next one through must own up to the two that were lost.
        let _ = rx.try_recv().expect("first");
        let _ = rx.try_recv().expect("second");
        assert!(n.alert("warning", &json!({"i": 5})));
        let recovered = rx.try_recv().expect("fifth");
        assert!(recovered.contains("\"droppedSinceLast\":2"), "must report the gap: {recovered}");

        // And the count resets, so the next clean notification does not re-report old losses.
        assert!(n.alert("warning", &json!({"i": 6})));
        let clean = rx.try_recv().expect("sixth");
        assert!(!clean.contains("droppedSinceLast"), "drop count must reset: {clean}");
    }

    // The receiver going away is a normal shutdown ordering, not a panic — and it must still be
    // counted, so a later alerter on a live channel does not under-report.
    #[test]
    fn a_dead_writer_is_survivable() {
        let (n, rx) = notifier_with_capacity(2);
        n.arm();
        drop(rx);
        assert!(!n.alert("warning", &json!({"i": 1})), "must report failure, not panic");
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

// Tool schema
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListToolsResult {
    pub tools: Vec<Tool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CallToolResult {
    pub content: Vec<ContentBlock>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
}

// Standard JSON-RPC error codes
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;
