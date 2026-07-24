// Debug session management
//
// Manages JDWP connection state, breakpoints, and thread tracking

use jdwp_client::{JdwpConnection, EventSet};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

pub type SessionId = String;

#[derive(Debug)]
pub struct DebugSession {
    pub connection: JdwpConnection,
    pub breakpoints: HashMap<String, BreakpointInfo>,
    pub last_event: Option<EventSet>,
    pub event_listener_task: Option<JoinHandle<()>>,
    /// Thread of the most recent suspension event — used to default `thread_id`.
    pub last_thread: Option<u64>,
    /// Active single-step request id (must be cleared before the next resume).
    pub pending_step: Option<i32>,
    /// When the VM last suspended on an event; cleared on resume. Drives the watchdog.
    pub suspended_since: Option<std::time::Instant>,
    pub watchdog_task: Option<JoinHandle<()>>,
    /// Breakpoints requested on classes not yet loaded. Each holds a `CLASS_PREPARE` request that
    /// fires when the class loads; the event pump then arms the real breakpoint. See handlers.rs.
    pub pending_breakpoints: Vec<PendingBreakpoint>,
    /// Active exception breakpoints (EXCEPTION event requests), keyed by their `exc_` id.
    pub exception_requests: HashMap<String, ExceptionRequestInfo>,
    /// Ring buffer of trace/logpoint snapshots (see `TraceRecord`). Bounded by `MAX_TRACES`.
    pub traces: VecDeque<TraceRecord>,
    /// Monotonic sequence for trace records (survives ring-buffer eviction).
    pub trace_seq: u64,
}

/// Max trace snapshots retained per session; oldest are evicted (documented cap for TRACE-1).
pub const MAX_TRACES: usize = 500;

/// One captured hit of a trace/logpoint breakpoint: where it fired, on which thread, the in-scope
/// locals/args at that point, and an optional evaluated expression. Recorded without leaving the
/// thread suspended.
#[derive(Debug, Clone)]
pub struct TraceRecord {
    pub seq: u64,
    pub bp_id: String,
    pub thread: u64,
    pub class: String,
    pub method: String,
    pub line: Option<i32>,
    /// (name, rendered value) for each in-scope local/argument at the hit.
    pub args: Vec<(String, String)>,
    /// (expression, rendered result) when the logpoint had a trace expression.
    pub expr: Option<(String, String)>,
}

/// An active exception breakpoint: an EXCEPTION event request that fires when a matching
/// exception is thrown. Tracked so it shows in `list_breakpoints` and is cleared by
/// `clear_breakpoint` / panic, like a normal breakpoint.
#[derive(Debug, Clone)]
pub struct ExceptionRequestInfo {
    /// The `exc_` id reported to the caller.
    pub id: String,
    /// The JDWP EXCEPTION event-request id.
    pub request_id: i32,
    /// Dotted class pattern the caller gave, or "*" for all exceptions.
    pub class_pattern: String,
    pub caught: bool,
    pub uncaught: bool,
}

/// A breakpoint waiting for its class to load. The `CLASS_PREPARE` request suspends the preparing
/// thread (`EventThread` policy) so the real breakpoint can be armed before any of the class's code
/// runs; the pump then resumes that one thread.
#[derive(Debug, Clone)]
pub struct PendingBreakpoint {
    /// The bp_ id reserved for this breakpoint (reported now, armed later).
    pub bp_id: String,
    /// The `CLASS_PREPARE` event-request id (cleared once armed).
    pub class_prepare_request_id: i32,
    /// Dotted class pattern (as the user gave it) — for messages.
    pub class_pattern: String,
    /// JNI signature ("Lpkg/Cls;") to match against the `ClassPrepare` event signature.
    pub signature: String,
    pub line: Option<i32>,
    pub method: Option<String>,
    pub hit_count: Option<i32>,
    pub thread_filter: Option<u64>,
    pub condition: Option<String>,
    /// Arm as a non-suspending trace/logpoint (`EventThread` suspend, snapshot, resume).
    pub trace: bool,
    /// Optional expression to evaluate and record on each trace hit.
    pub trace_expr: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BreakpointInfo {
    pub request_id: i32,
    pub class_pattern: String,
    pub line: u32,
    pub method: Option<String>,
    pub enabled: bool,
    pub hit_count: u32,
    /// Optional server-side condition: on hit, evaluate it and auto-resume if it is not true.
    pub condition: Option<String>,
    /// Non-suspending trace/logpoint: on hit, snapshot into the ring buffer and resume the thread.
    pub trace: bool,
    /// Optional expression evaluated and recorded on each trace hit.
    pub trace_expr: Option<String>,
}

#[derive(Clone)]
pub struct SessionManager {
    sessions: Arc<Mutex<HashMap<SessionId, Arc<Mutex<DebugSession>>>>>,
    current_session: Arc<Mutex<Option<SessionId>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            current_session: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn create_session(&self, connection: JdwpConnection) -> SessionId {
        let session_id = format!("session_{}", uuid::v4());
        let session = DebugSession {
            connection,
            breakpoints: HashMap::new(),
            last_event: None,
            event_listener_task: None,
            last_thread: None,
            pending_step: None,
            suspended_since: None,
            watchdog_task: None,
            pending_breakpoints: Vec::new(),
            exception_requests: HashMap::new(),
            traces: VecDeque::new(),
            trace_seq: 0,
        };

        let mut sessions = self.sessions.lock().await;
        sessions.insert(session_id.clone(), Arc::new(Mutex::new(session)));
        drop(sessions); // release the map lock before taking the current-session lock

        // Set as current session
        let mut current = self.current_session.lock().await;
        *current = Some(session_id.clone());

        session_id
    }

    pub async fn get_current_session(&self) -> Option<Arc<Mutex<DebugSession>>> {
        let current = self.current_session.lock().await;
        if let Some(session_id) = current.as_ref() {
            let sessions = self.sessions.lock().await;
            sessions.get(session_id).cloned()
        } else {
            None
        }
    }

    pub async fn get_session_by_id(&self, session_id: &str) -> Option<Arc<Mutex<DebugSession>>> {
        let sessions = self.sessions.lock().await;
        sessions.get(session_id).cloned()
    }

    pub async fn get_current_session_id(&self) -> Option<SessionId> {
        let current = self.current_session.lock().await;
        current.clone()
    }

    pub async fn remove_session(&self, session_id: &str) {
        let mut sessions = self.sessions.lock().await;

        // Abort the event listener task if it exists
        if let Some(session_arc) = sessions.get(session_id) {
            let mut session = session_arc.lock().await;
            if let Some(task) = session.event_listener_task.take() {
                task.abort();
            }
            if let Some(task) = session.watchdog_task.take() {
                task.abort();
            }
        }

        sessions.remove(session_id);
        drop(sessions); // release the map lock before taking the current-session lock

        // Clear current if it was this session
        let mut current = self.current_session.lock().await;
        if current.as_ref() == Some(&session_id.to_string()) {
            *current = None;
        }
    }
}

// Simple UUID generation for session IDs
mod uuid {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(1);

    pub fn v4() -> String {
        let counter = COUNTER.fetch_add(1, Ordering::SeqCst);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();

        format!("{timestamp:x}{counter:x}")
    }
}
