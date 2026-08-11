// MCP protocol definitions - minimal and explicit
//
// Based on gamecode-mcp2 pattern: no hidden behavior, all JSON explicit

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

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
    ///
    /// Only meaningful in [`Era::Legacy`]: the stateless era has no handshake to complete, and nothing
    /// there is ever armed.
    armed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Which wire contract the peer speaks, as an [`Era`] discriminant (MCP-1).
    era: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Notifications discarded because the queue was full, reported on the next one that gets through
    /// so a client that fell behind never reads the silence as "nothing happened" (SAFE-8's posture).
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Alerter {
    pub fn new(tx: tokio::sync::mpsc::Sender<String>) -> Self {
        Self {
            tx,
            armed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            era: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(Era::Unknown as u8)),
            dropped: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Allow emission. Called when the client confirms the handshake.
    ///
    /// Confirming a handshake **is** the legacy signal — there is no other reason to send
    /// `notifications/initialized` — so this records the era as well, and the two cannot disagree.
    pub fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::Relaxed);
        self.note_era(Era::Legacy);
    }

    /// Record which era the peer opened in (MCP-1). Last writer wins, deliberately: a client that
    /// probed with `server/discover` and then fell back to `initialize` has just told us, in its second
    /// message, which contract it actually wants.
    ///
    /// Returns whether this changed the answer, so a caller can log the transition once rather than on
    /// every request.
    pub fn note_era(&self, era: Era) -> bool {
        self.era.swap(era as u8, std::sync::atomic::Ordering::Relaxed) != era as u8
    }

    fn era(&self) -> Era {
        Era::from_u8(self.era.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Emit one `notifications/message`, or account for it if that is not possible.
    ///
    /// Returns whether it was queued, which is what the tests assert on — the caller has nothing
    /// useful to do with the answer, since every failure here is already handled.
    // `data` by reference, not by value: the armed/enabled guard below returns before it would be
    // consumed, and a disarmed alerter is the common case for the whole pre-handshake window.
    pub fn alert(&self, level: &str, data: &serde_json::Value) -> bool {
        use std::sync::atomic::Ordering;
        if !alerts_enabled() {
            return false;
        }
        // MCP-1: under 2026-07-28 there is NO legal channel for this line. `notifications/message` is
        // request-scoped and MUST NOT be emitted for a request that did not set
        // `io.modelcontextprotocol/logLevel` — and a JDWP hit belongs to no request at all, since it is
        // the debuggee's clock that produced it, not a caller's. `subscriptions/listen` cannot carry it
        // either: its opt-in types are a closed set about list changes and resource subscriptions.
        //
        // So it goes to stderr, which the stdio binding blesses for exactly this ("the server MAY write
        // UTF-8 strings to stderr for any logging purposes"), and `debug.get_last_event` remains the
        // supported way to ask. Reported rather than dropped, which is the whole of EVT-2's posture —
        // what changed is the channel, not whether the caller can find out.
        if self.era() != Era::Legacy {
            warn!(level, alert = %data, "JDWP alert (stderr: the stateless protocol has no unsolicited channel — read it with debug.get_last_event)");
            return false;
        }
        if !self.armed.load(Ordering::Relaxed) {
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

    /// MCP-1: under a stateless protocol version there is no legal channel for an unsolicited
    /// `notifications/message`, so nothing may reach the queue that writes stdout.
    ///
    /// Asserted on the CHANNEL and not only on the return value, because the return value is already
    /// `false` for three other reasons — unarmed, disabled, full — and a regression that started
    /// pushing would keep returning `true` while putting an illegal line on the wire.
    #[test]
    fn a_stateless_client_gets_nothing_on_stdout() {
        let (n, mut rx) = notifier_with_capacity(4);
        n.note_era(Era::Modern);
        assert!(
            !n.alert("warning", &json!({ "event": "breakpoint" })),
            "must not push to a stateless client"
        );
        assert!(rx.try_recv().is_err(), "a stateless client's alert may not be queued at all");

        // `arm()` IS the legacy handshake, so it carries the era with it — a client that completed a
        // handshake is by definition one that expects these.
        n.arm();
        assert!(n.alert("warning", &json!({ "event": "breakpoint" })), "a legacy client still gets pushed");
        assert!(rx.try_recv().is_ok(), "the legacy path must be unchanged by MCP-1");
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
    /// `CacheableResult` (MCP-1). Required on a list reply since 2026-07-28, and inert to a client on
    /// an earlier revision — the tool set is compiled in, so it is a freshness hint that cannot go
    /// stale while this process lives. See [`CACHE_TTL_MS`].
    #[serde(rename = "ttlMs")]
    pub ttl_ms: u64,
    #[serde(rename = "cacheScope")]
    pub cache_scope: String,
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

/// MCP-defined error codes (MCP-1).
///
/// `-32020`–`-32099` is reserved for the specification, and an implementation **MUST NOT** emit a code
/// from that range the spec does not define. `-32000`–`-32019` is the grandfathered implementation
/// range and is deliberately unused here.
///
/// **Two of the three the spec defines are absent on purpose**, because a code that cannot be emitted
/// is a claim about behaviour that does not exist. `HeaderMismatch` (`-32020`) is a Streamable-HTTP
/// condition and this server is stdio-only (`.out-of-scope/http-transport.md`).
/// `MissingRequiredClientCapability` (`-32021`) answers *"this request needs a capability you did not
/// declare"* — and no tool here needs one: nothing samples, elicits or reads roots, so all 42 are
/// answerable by a client that declares nothing at all. A **missing** `clientCapabilities` field is a
/// different fault, and the spec puts that one at `-32602`, malformed.
pub const UNSUPPORTED_PROTOCOL_VERSION: i32 = -32022;

/// The protocol versions this server serves **statelessly**, newest first (MCP-1).
///
/// "Modern" in the spec's own terms: versions that carry version, identity and capabilities as
/// per-request `_meta` rather than negotiating them in a handshake. Only these are valid in
/// `io.modelcontextprotocol/protocolVersion`, which is why the list does not include
/// [`LEGACY_PROTOCOL_VERSION`] — that one is reachable *only* through `initialize`, and a client
/// naming it in `_meta` would be asking for a combination that does not exist.
pub const MODERN_PROTOCOL_VERSIONS: [&str; 1] = ["2026-07-28"];

/// The revision `initialize` negotiates, for clients that predate the stateless model.
pub const LEGACY_PROTOCOL_VERSION: &str = "2024-11-05";

/// Reserved `_meta` keys this server reads or writes (MCP-1).
pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
pub const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
pub const META_LOG_LEVEL: &str = "io.modelcontextprotocol/logLevel";
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

/// The syslog levels of RFC 5424, which is the closed set `io.modelcontextprotocol/logLevel` may name.
pub const LOG_LEVELS: [&str; 8] =
    ["debug", "info", "notice", "warning", "error", "critical", "alert", "emergency"];

/// How long a client may cache a list, and who may cache it (MCP-1, `CacheableResult`).
///
/// **An hour bounds staleness after an upgrade, not staleness after a change.** The tool set is
/// compiled in — `get_tools()` is a fixed vector — so it cannot change while this process lives, and
/// the honest TTL for "will not change" is unbounded. What an hour actually protects against is a
/// client that outlives the binary it cached from, which on stdio it should not, and the cost of being
/// wrong is one extra `tools/list`.
pub const CACHE_TTL_MS: u64 = 3_600_000;

/// `public`: nothing in a list reply varies by caller and none of it is secret. This server has no
/// authorization surface at all (stdio takes credentials from the environment), so there is no
/// per-caller variation for a shared intermediary to leak.
pub const CACHE_SCOPE: &str = "public";

/// Which protocol era the client on the other end opened in (MCP-1).
///
/// **This is not the session state the stateless model forbids, and the distinction is worth stating
/// because it looks like it.** Nothing about how a request is *answered* is derived from it: every
/// reply is a pure function of that request. It records which of two wire contracts the peer speaks,
/// which the spec's own compatibility matrix instructs a dual-era server to infer from how the client
/// opens — and the only thing that consults it is where an unsolicited alert may legally go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Era {
    /// Nothing has arrived yet that says. Treated as [`Self::Modern`] for delivery, because that is
    /// the choice that cannot put an illegal line on stdout.
    Unknown = 0,
    /// The client sent `initialize`. It expects `notifications/message` and gets them, exactly as
    /// before this revision was supported.
    Legacy = 1,
    /// The client sent per-request `_meta`. An unsolicited `notifications/message` would be a protocol
    /// violation, so alerts go to stderr and `debug.get_last_event` is the supported pull path.
    Modern = 2,
}

impl Era {
    const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Legacy,
            2 => Self::Modern,
            _ => Self::Unknown,
        }
    }
}

/// The server's reply to `server/discover`, which a 2026-07-28 server **MUST** implement (MCP-1).
#[derive(Debug, Serialize, Deserialize)]
pub struct DiscoverResult {
    #[serde(rename = "supportedVersions")]
    pub supported_versions: Vec<String>,
    pub capabilities: ServerCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(rename = "ttlMs")]
    pub ttl_ms: u64,
    #[serde(rename = "cacheScope")]
    pub cache_scope: String,
}
