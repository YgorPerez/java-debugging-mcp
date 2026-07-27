// MCP request handlers
//
// Handles initialize, list tools, and debug tool execution

use crate::protocol::{
    CallToolParams, CallToolResult, ContentBlock, InitializeParams, InitializeResult, JsonRpcError,
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, ListToolsResult, ServerCapabilities, ServerInfo,
    ToolsCapability, INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND,
};
use crate::session::SessionManager;
use crate::tools;
use serde_json::json;
use std::fmt::Write as _;
use tracing::{debug, info, warn};

/// Serialize an internal response struct into a JSON value, mapping the
/// (practically impossible) serialization failure to a JSON-RPC internal error
/// rather than panicking.
fn to_json<T: serde::Serialize>(value: &T) -> Result<serde_json::Value, JsonRpcError> {
    serde_json::to_value(value).map_err(|e| JsonRpcError {
        code: INTERNAL_ERROR,
        message: format!("Failed to serialize response: {e}"),
        data: None,
    })
}

pub struct RequestHandler {
    session_manager: SessionManager,
    /// Outbound push channel (EVT-2). Held here so the handshake can arm it; sessions get their own
    /// clone at creation so the event pump and watchdog can reach it without going through here.
    alerter: crate::protocol::Alerter,
}

impl RequestHandler {
    pub fn new(alerter: crate::protocol::Alerter) -> Self {
        Self { session_manager: SessionManager::new(alerter.clone()), alerter }
    }

    /// Resolve the target session: an explicit `session_id` argument, else the current session.
    /// (Supports multiple concurrent debug sessions to different JVMs.)
    async fn resolve_session(
        &self,
        args: &serde_json::Value,
    ) -> Option<std::sync::Arc<tokio::sync::Mutex<crate::session::DebugSession>>> {
        match args.get("session_id").and_then(|v| v.as_str()) {
            Some(sid) => self.session_manager.get_session_by_id(sid).await,
            None => self.session_manager.get_current_session().await,
        }
    }

    pub async fn handle_request(&self, request: JsonRpcRequest) -> JsonRpcResponse {
        let result = match request.method.as_str() {
            "initialize" => Self::handle_initialize(request.params),
            "tools/list" => Self::handle_list_tools(),
            "tools/call" => self.handle_call_tool(request.params).await,
            _ => Err(JsonRpcError {
                code: METHOD_NOT_FOUND,
                message: format!("Method not found: {}", request.method),
                data: None,
            }),
        };

        match result {
            Ok(value) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: Some(value),
                error: None,
            },
            Err(error) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: None,
                error: Some(error),
            },
        }
    }

    pub fn handle_notification(&self, notification: &JsonRpcNotification) {
        match notification.method.as_str() {
            "notifications/initialized" => {
                info!("Client initialized");
                // Only now may the server push (EVT-2). A stop point can be armed and hit while the
                // handshake is still in flight, and a notification sent before this point is a
                // protocol violation rather than a helpful early warning.
                self.alerter.arm();
            }
            "notifications/cancelled" => {
                debug!("Request cancelled");
            }
            _ => {
                warn!("Unknown notification: {}", notification.method);
            }
        }
    }

    fn handle_initialize(params: Option<serde_json::Value>) -> Result<serde_json::Value, JsonRpcError> {
        let _params: InitializeParams =
            serde_json::from_value(params.unwrap_or_else(|| json!({}))).map_err(|e| JsonRpcError {
                code: INVALID_PARAMS,
                message: format!("Invalid initialize params: {e}"),
                data: None,
            })?;

        let result = InitializeResult {
            protocol_version: "2024-11-05".to_string(),
            capabilities: ServerCapabilities {
                tools: ToolsCapability {},
                // EVT-2. Declared unconditionally: whether anything is actually pushed depends on
                // JDWP_ALERTS, but the capability describes what this server can do, not how
                // it happens to be configured — and a client that sees it may still ignore every
                // notification, which is exactly what best-effort means here.
                logging: Some(crate::protocol::LoggingCapability {}),
            },
            server_info: ServerInfo {
                name: "jdwp-mcp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            instructions: Some(
                "JDWP debugging server for Java applications. \
                Start by using debug.attach to connect to a JVM, \
                then use debug.set_line_stop, debug.get_stack, etc."
                    .to_string(),
            ),
        };

        to_json(&result)
    }

    fn handle_list_tools() -> Result<serde_json::Value, JsonRpcError> {
        let result = ListToolsResult { tools: tools::get_tools() };

        to_json(&result)
    }

    async fn handle_call_tool(
        &self,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, JsonRpcError> {
        let call_params: CallToolParams = serde_json::from_value(params.unwrap_or_else(|| json!({})))
            .map_err(|e| JsonRpcError {
                code: INVALID_PARAMS,
                message: format!("Invalid tool call params: {e}"),
                data: None,
            })?;

        // Route to the appropriate handler, split into two dispatch groups to keep each small.
        let name = call_params.name.as_str();
        let args = call_params.arguments;
        let result = if let Some(r) = self.dispatch_control(name, args.clone()).await {
            r
        } else if let Some(r) = self.dispatch_inspect(name, args).await {
            r
        } else {
            Err(format!("Unknown tool: {name}"))
        };

        match result {
            Ok(content) => {
                let call_result =
                    CallToolResult { content: vec![ContentBlock::Text { text: content }], is_error: None };
                to_json(&call_result)
            }
            Err(error) => {
                let call_result = CallToolResult {
                    content: vec![ContentBlock::Text { text: error }],
                    is_error: Some(true),
                };
                to_json(&call_result)
            }
        }
    }

    /// Session-control and execution tools (attach, breakpoints, stepping, lifecycle).
    /// Returns `None` if `name` isn't one of these, so the caller can try the next group.
    async fn dispatch_control(&self, name: &str, args: serde_json::Value) -> Option<Result<String, String>> {
        Some(match name {
            "debug.attach" => self.handle_attach(args).await,
            "debug.set_line_stop" => self.handle_set_line_stop(args).await,
            "debug.list_stop_points" => self.handle_list_stop_points(args).await,
            "debug.clear_stop_point" => self.handle_clear_stop_point(args).await,
            "debug.toggle_stop_point" => self.handle_toggle_stop_point(args).await,
            "debug.continue" => self.handle_continue(args).await,
            "debug.step_over" => self.handle_step_over(args).await,
            "debug.step_into" => self.handle_step_into(args).await,
            "debug.step_out" => self.handle_step_out(args).await,
            "debug.pause" => self.handle_pause(args).await,
            "debug.list_sessions" => self.handle_list_sessions().await,
            "debug.disconnect" => self.handle_disconnect(args).await,
            "debug.panic" => self.handle_panic(args).await,
            _ => return None,
        })
    }

    /// State-inspection and mutation tools (stack, evaluate, threads, set value, traces).
    /// Returns `None` if `name` isn't one of these.
    async fn dispatch_inspect(&self, name: &str, args: serde_json::Value) -> Option<Result<String, String>> {
        Some(match name {
            "debug.get_stack" => self.handle_get_stack(args).await,
            "debug.evaluate" => self.handle_evaluate(args).await,
            "debug.list_threads" => self.handle_list_threads(args).await,
            "debug.list_classes" => self.handle_list_classes(args).await,
            "debug.list_methods" => self.handle_list_methods(args).await,
            "debug.source" => self.handle_source(args).await,
            "debug.thread_dump" => self.handle_thread_dump(args).await,
            "debug.get_last_event" => self.handle_get_last_event(args).await,
            "debug.set_value" => self.handle_set_value(args).await,
            "debug.force_return" => self.handle_force_return(args).await,
            "debug.set_exception_stop" => self.handle_set_exception_stop(args).await,
            "debug.set_field_stop" => self.handle_set_field_stop(args).await,
            "debug.set_method_exit_stop" => self.handle_set_method_exit_stop(args).await,
            "debug.get_traces" => self.handle_get_traces(args).await,
            _ => return None,
        })
    }

    async fn handle_attach(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::AttachArgs = crate::args::parse(&args)?;
        let host = a.host.as_str();
        let port = a.port;

        let connection = jdwp_client::JdwpConnection::connect(host, port)
            .await
            .map_err(|e| format!("Failed to connect: {e}"))?;

        // Read-only when the caller asks for it OR the env forces it (a deploy-wide guard for a
        // production JVM). Either source alone is enough — the env can't be relaxed per-attach (SAFE-3).
        //
        // Set on the CONNECTION, so it is enforced where invocation and writes actually happen rather
        // than by inspecting expression text up here (SAFE-6). The flag is shared with every clone,
        // including the event pump's — which is what evaluates a condition or `trace_expr` on a hit.
        let read_only = a.read_only || env_readonly();
        if read_only {
            connection.set_read_only(true);
        }
        // Roots given here REPLACE the env default rather than adding to it, which is the opposite of
        // how `read_only` combines above — and deliberately so. `JDWP_READONLY` is a deploy-wide guard
        // that must not be relaxable per-attach; `JDWP_SOURCE_ROOTS` is only a convenience default, so
        // a caller who names roots for this JVM means those and not also whatever the environment held.
        let source_roots = a
            .source_roots
            .as_ref()
            .map_or_else(env_source_roots, |v| v.iter().map(std::path::PathBuf::from).collect());
        let session_id = self
            .session_manager
            .create_session(connection, format!("{host}:{port}"), read_only, source_roots)
            .await;
        // Get the session guard once so the listener/watchdog handles are stored before we return.
        let session_guard = self
            .resolve_session(&args)
            .await
            .ok_or_else(|| "Failed to get session after creation".to_string())?;

        {
            let mut session = session_guard.lock().await;
            let connection_clone = session.connection.clone();
            // Event listener is bound to THIS session id (not "current").
            session.event_listener_task = Some(spawn_event_listener(
                self.session_manager.clone(),
                session_id.clone(),
                connection_clone,
            ));
            // Watchdog: auto-resume if a breakpoint leaves the VM suspended too long, so a
            // forgotten breakpoint can't freeze a request thread on a shared instance.
            session.watchdog_task = Some(spawn_watchdog(self.session_manager.clone(), session_id.clone()));
        }

        let ro = if read_only {
            "\n   🔒 Read-only: method invocation, set_value and force_return are refused; collection expansion falls back to shallow. A guard against accident, not a security boundary."
        } else {
            ""
        };
        Ok(format!("Connected to JVM at {host}:{port} (session: {session_id}){ro}"))
    }

    /// List every live session, so a caller who lost a `session_id` can find it again.
    ///
    /// Read-only on purpose. A dead session is *reported* dead rather than reaped: this is the tool you
    /// reach for when you are already confused about what is attached, and having it silently drop
    /// entries mid-listing would make it a worse instrument. `debug.disconnect {session_id}` removes one.
    async fn handle_list_sessions(&self) -> Result<String, String> {
        let (sessions, current) = self.session_manager.list().await;
        if sessions.is_empty() {
            return Ok("No debug sessions. Use debug.attach to open one.".to_string());
        }

        let mut out = format!("{} session(s):\n", sessions.len());
        for (sid, guard) in &sessions {
            // Scoped so each session's lock is released before the next is taken.
            let line = {
                let s = guard.lock().await;
                render_session_line(sid, &s, current.as_ref())
            };
            out.push_str(&line);
        }
        out.push_str("\nEvery tool takes an optional session_id; without one it uses the current session.");
        Ok(out)
    }

    async fn handle_set_line_stop(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::SetBreakpointArgs = crate::args::parse(&args)?;
        let class_pattern = a.class_pattern.as_str();

        if a.line.is_none() && a.method.is_none() {
            return Err("Provide 'line' and/or 'method'".to_string());
        }

        let signature = if class_pattern.starts_with('L') && class_pattern.ends_with(';') {
            class_pattern.to_string()
        } else {
            format!("L{};", class_pattern.replace('.', "/"))
        };
        let suspend_policy = suspend_policy_for(a.trace);
        let (trace_frames, frames_note) = clamp_trace_frames(a.trace, a.trace_frames);
        let spec = BreakpointSpec {
            class_pattern: class_pattern.to_string(),
            signature,
            line_opt: a.line,
            method_hint: a.method.clone(),
            hit_count: a.hit_count,
            thread_filter: crate::args::parse_thread_id(a.thread_id.as_deref()),
            condition: a.condition.clone(),
            trace: a.trace,
            trace_expr: a.trace_expr.clone(),
            trace_budget: trace_budget_for(a.trace, a.trace_max_hits),
            trace_frames,
            suspend_policy,
        };

        let session_guard = self
            .resolve_session(&args)
            .await
            .ok_or_else(|| "No active debug session. Use debug.attach first.".to_string())?;
        let mut session = session_guard.lock().await;
        check_readonly_exprs(session.read_only, spec.condition.as_deref(), spec.trace_expr.as_deref())?;
        check_thread_filter(&mut session.connection, spec.thread_filter).await?;

        // One id for this breakpoint's whole life, allocated before we know whether it arms now or is
        // deferred — and kept across any later disable/re-arm (BP-3).
        let bp_id = session.next_stop_id("bp_");

        let classes = session
            .connection
            .classes_by_signature(&spec.signature)
            .await
            .map_err(|e| format!("Failed to find class: {e}"))?;
        let Some(first_class) = classes.first() else {
            return register_deferred_breakpoint(&mut session, &spec, bp_id).await;
        };
        let class_type_id = first_class.type_id;

        let (bp_id, line, method_name, request_id) =
            arm_and_insert(&mut session, class_type_id, &spec, bp_id).await?;
        drop(session);

        let mut extra = describe_trace_mode(&spec, frames_note.as_deref());
        if let Some(c) = spec.hit_count {
            let _ = write!(extra, "\n   Stops on hit #{c}");
        }
        if let Some(t) = spec.thread_filter {
            let _ = write!(extra, "\n   Thread filter: 0x{t:x}");
        }
        if let Some(c) = &spec.condition {
            let _ = write!(extra, "\n   Condition: {c}");
        }
        Ok(format!(
            "✅ {} set at {}:{}\n   Method: {}\n   Stop-point ID: {}\n   JDWP Request ID: {}{}",
            if spec.trace { "Trace breakpoint" } else { "Breakpoint" },
            spec.class_pattern,
            line,
            method_name,
            bp_id,
            request_id,
            extra
        ))
    }

    async fn handle_list_stop_points(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        if session.breakpoints.is_empty()
            && session.pending_breakpoints.is_empty()
            && session.exception_requests.is_empty()
            && session.watchpoints.is_empty()
            && session.method_exits.is_empty()
        {
            return Ok(session.last_watchdog_note.as_ref().map_or_else(
                || "No breakpoints set".to_string(),
                |n| format!("No breakpoints set\n⏰ {n}"),
            ));
        }

        // FILT-2: a filter pinned to a dead thread can never fire again, so establish that BEFORE
        // rendering anything as armed. One round trip per distinct filter thread, none without a filter.
        let dead = dead_filter_threads(&mut session).await;

        let mut output = String::new();
        // Surface a watchdog auto-resume up front (SAFE-2): the caller was away, so the fact that a
        // stop point was disarmed and the VM resumed is the most important thing on this listing.
        if let Some(n) = &session.last_watchdog_note {
            let _ = writeln!(output, "⏰ {n}\n");
        }
        let _ = write!(
            output,
            "📍 {} breakpoint(s), {} deferred, {} exception, {} watchpoint(s), {} method-exit:\n\n",
            session.breakpoints.len(),
            session.pending_breakpoints.len(),
            session.exception_requests.len(),
            session.watchpoints.len(),
            session.method_exits.len()
        );

        for (bp_id, bp) in &session.breakpoints {
            render_breakpoint_line(&mut output, bp_id, bp, &dead);
        }

        for pb in &session.pending_breakpoints {
            render_pending_line(&mut output, pb, &dead);
        }

        for er in session.exception_requests.values() {
            render_exception_line(&mut output, er, &dead);
        }

        for (watch_id, wp) in &session.watchpoints {
            render_watchpoint_line(&mut output, watch_id, wp, &dead);
        }

        for me in session.method_exits.values() {
            render_method_exit_line(&mut output, me, &dead);
        }
        if !dead.is_empty() {
            let _ = write!(
                output,
                "\n⚠️  {} stop point(s) above are filtered to a thread that no longer exists. A pool that \
                 retires idle workers (which is what a thread filter is usually for) invalidates the id, \
                 and the stop point then reports nothing at all — silence that reads like \"no hits\". \
                 Re-read debug.list_threads for a live id and re-arm.\n",
                dead.len()
            );
        }
        drop(session);

        Ok(output)
    }

    async fn handle_clear_stop_point(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::ClearBreakpointArgs = crate::args::parse(&args)?;
        let bp_id = a.breakpoint_id.as_str();

        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        // An exception breakpoint lives in exception_requests as an EXCEPTION event request. A disabled
        // one has no live request, so there is only the stored definition to drop.
        if let Some(er) = session.exception_requests.remove(bp_id) {
            if let Some(req) = er.request_id {
                let _ = session.connection.clear_exception_request(req).await;
            }
            return Ok(format!("✅ Exception breakpoint cleared: {} ({})", bp_id, er.class_pattern));
        }

        // A watchpoint lives in watchpoints as a FIELD_ACCESS / FIELD_MODIFICATION request; Clear
        // must name the same event kind the request was created with.
        if let Some(wp) = session.watchpoints.remove(bp_id) {
            if let Some(req) = wp.request_id {
                let _ = session.connection.clear_field_watch(req, wp.kind).await;
            }
            return Ok(format!(
                "✅ Watchpoint cleared: {} ({}.{} {})",
                bp_id,
                wp.class_name,
                wp.field_name,
                wp.kind.label()
            ));
        }

        // A method-exit request lives in method_exits; Clear must name the same event kind it was armed
        // with (41 vs 42), or JDWP looks up a different key and silently leaves the request armed —
        // which for this kind means a possibly-suspending stop point nobody can find.
        if let Some(me) = session.method_exits.remove(bp_id) {
            if let Some(req) = me.request_id {
                let _ = session.connection.clear_method_exit_request(req, me.with_return_value).await;
            }
            return Ok(format!(
                "✅ Method-exit reporting cleared: {} ({}{})",
                bp_id,
                me.class_pattern,
                me.method.map_or_else(|| ".*".to_string(), |m| format!(".{m}"))
            ));
        }

        // A deferred (not-yet-armed) breakpoint lives in pending_breakpoints with only a
        // CLASS_PREPARE watch — clear that watch instead of a real breakpoint request.
        if let Some(pos) = session.pending_breakpoints.iter().position(|p| p.bp_id == bp_id) {
            let pb = session.pending_breakpoints.remove(pos);
            let _ = session.connection.clear_class_prepare(pb.class_prepare_request_id).await;
            return Ok(format!("✅ Deferred breakpoint cleared: {} ({})", bp_id, pb.class_pattern));
        }

        // Find the breakpoint
        let bp_info =
            session.breakpoints.get(bp_id).ok_or_else(|| format!("Breakpoint not found: {bp_id}"))?.clone();

        // Clear the breakpoint in the JVM — a disabled breakpoint has no live request, so there is
        // nothing to clear there, only the stored definition to drop (BP-1).
        if let Some(req) = bp_info.request_id {
            session
                .connection
                .clear_breakpoint(req)
                .await
                .map_err(|e| format!("Failed to clear breakpoint: {e}"))?;
        }

        // Remove from session
        session.breakpoints.remove(bp_id);
        drop(session);

        Ok(format!(
            "✅ Breakpoint cleared: {} at {}:{}\n   JDWP Request ID: {}",
            bp_id,
            bp_info.class_pattern,
            bp_info.line,
            bp_info.request_id.map_or_else(|| "(disabled)".to_string(), |r| r.to_string())
        ))
    }

    /// Silence or re-arm a stop point without losing its definition (BP-1), for any of the three kinds
    /// (BP-2): disabling clears the JDWP request but keeps the entry — location, `condition`,
    /// `trace_expr`, thread filter — and enabling re-arms it from that stored definition.
    ///
    /// The caller-facing id is stable across the round trip (BP-3), so the id you hold keeps working.
    async fn handle_toggle_stop_point(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::ToggleBreakpointArgs = crate::args::parse(&args)?;
        let id = a.breakpoint_id.trim().to_string();

        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;

        // Current state, whichever map owns this id.
        let current = if let Some(b) = session.breakpoints.get(&id) {
            b.enabled
        } else if let Some(e) = session.exception_requests.get(&id) {
            e.enabled
        } else if let Some(w) = session.watchpoints.get(&id) {
            w.enabled
        } else if let Some(m) = session.method_exits.get(&id) {
            m.enabled
        } else if let Some(pb) = session.pending_breakpoints.iter().find(|p| p.bp_id == id) {
            // A deferred breakpoint isn't armed at all yet — it holds only a CLASS_PREPARE watch, so
            // there is no request to silence. Say that, rather than the misleading "not found" this
            // used to return for an id `list_stop_points` was showing (BP-3).
            return Err(format!(
                "{id} is a deferred breakpoint waiting for {} to load — it holds no active breakpoint \
                 request yet, so there is nothing to toggle. Use debug.clear_stop_point to drop it, or \
                 toggle it once the class loads and it arms.",
                pb.class_pattern
            ));
        } else {
            return Err(format!("Stop point not found: {id}"));
        };

        // Omitted `enabled` flips the current state.
        let want = a.enabled.unwrap_or(!current);
        if want == current {
            return Ok(format!("No change: {id} is already {}.", if current { "armed" } else { "disabled" }));
        }

        let what = if want {
            rearm_stop_point(&mut session, &id).await?
        } else {
            disable_stop_point(&mut session, &id).await?
        };
        drop(session);

        Ok(if want {
            format!("✅ Re-armed {id} ({what}) — same id, so anything holding it keeps working.")
        } else {
            format!("🔕 Disabled {id} ({what}) — its definition is kept; toggle it back on to re-arm.")
        })
    }

    async fn handle_continue(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        // Drop any pending single-step request first, or it would re-fire on resume.
        if let Some(req) = session.pending_step.take() {
            let _ = session.connection.clear_step(req).await;
        }
        // "Continue" means the application actually runs again, so clear any counted suspend depth
        // rather than issuing one resume and hoping (SAFE-7).
        let note = resume_and_verify(&mut session).await?;
        session.mark_resumed();
        drop(session);

        Ok(note.map_or_else(|| "▶️  Execution resumed".to_string(), |n| format!("▶️  {n}")))
    }

    async fn handle_step_over(&self, args: serde_json::Value) -> Result<String, String> {
        self.handle_step(args, jdwp_client::extra::StepDepth::Over, "over").await
    }

    async fn handle_step_into(&self, args: serde_json::Value) -> Result<String, String> {
        self.handle_step(args, jdwp_client::extra::StepDepth::Into, "into").await
    }

    async fn handle_step_out(&self, args: serde_json::Value) -> Result<String, String> {
        self.handle_step(args, jdwp_client::extra::StepDepth::Out, "out").await
    }

    async fn handle_step(
        &self,
        args: serde_json::Value,
        depth: jdwp_client::extra::StepDepth,
        label: &str,
    ) -> Result<String, String> {
        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;

        let a: crate::args::StepArgs = crate::args::parse(&args)?;
        let thread_id = crate::args::parse_thread_id(a.thread_id.as_deref())
            .or(session.last_thread)
            .ok_or_else(|| "No thread to step. Pass thread_id, or hit a breakpoint first.".to_string())?;

        // One active step request at a time; clear the previous before setting a new one.
        if let Some(req) = session.pending_step.take() {
            let _ = session.connection.clear_step(req).await;
        }
        let req = session
            .connection
            .set_step(thread_id, depth)
            .await
            .map_err(|e| format!("Failed to set step: {e}"))?;
        session.pending_step = Some(req);
        session.mark_resumed();
        session.connection.resume_all().await.map_err(|e| format!("Failed to resume for step: {e}"))?;
        drop(session);

        Ok(format!(
            "👣 Stepping {label} on thread 0x{thread_id:x}. Call debug.get_last_event to see where it stopped."
        ))
    }

    async fn handle_panic(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;

        if let Some(req) = session.pending_step.take() {
            let _ = session.connection.clear_step(req).await;
        }
        let n = session.breakpoints.len();
        let np = session.pending_breakpoints.len();
        let ne = session.exception_requests.len();
        let nw = session.watchpoints.len();
        let nm = session.method_exits.len();
        let _ = session.connection.clear_all_breakpoints().await;
        session.breakpoints.clear();
        // Also drop deferred breakpoints' CLASS_PREPARE watches.
        let pend: Vec<i32> =
            session.pending_breakpoints.drain(..).map(|p| p.class_prepare_request_id).collect();
        for req in pend {
            let _ = session.connection.clear_class_prepare(req).await;
        }
        // ClearAllBreakpoints only removes BREAKPOINT requests — clear exception requests too. A
        // disabled one holds no live request, so there is nothing to clear in the JVM for it.
        let excs: Vec<i32> = session.exception_requests.drain().filter_map(|(_, e)| e.request_id).collect();
        for req in excs {
            let _ = session.connection.clear_exception_request(req).await;
        }
        // Field watches are likewise untouched by ClearAllBreakpoints, and leaving one armed keeps
        // the debuggee de-optimised — so panic must drop them too.
        let watches: Vec<(i32, jdwp_client::WatchKind)> =
            session.watchpoints.drain().filter_map(|(_, w)| w.request_id.map(|r| (r, w.kind))).collect();
        for (req, kind) in watches {
            let _ = session.connection.clear_field_watch(req, kind).await;
        }
        // Method-exit requests are the most important thing for panic to drop: a suspending one on a hot
        // method re-freezes the VM on the very next return, so resuming without clearing them would be
        // no rescue at all. `ClearAllBreakpoints` does not touch them either.
        let mexits: Vec<(i32, bool)> = session
            .method_exits
            .drain()
            .filter_map(|(_, m)| m.request_id.map(|r| (r, m.with_return_value)))
            .collect();
        for (req, with_value) in mexits {
            let _ = session.connection.clear_method_exit_request(req, with_value).await;
        }
        // The panic button's whole job is to leave the VM running, so it must clear a counted suspend
        // depth and report honestly if it couldn't (SAFE-7).
        let note = resume_and_verify(&mut session).await?;
        session.mark_resumed();
        drop(session);

        Ok(format!(
            "🧯 Panic: cleared {} breakpoint(s){}{}{}{} and resumed all threads.{}",
            n,
            if np > 0 { format!(" + {np} deferred") } else { String::new() },
            if ne > 0 { format!(" + {ne} exception") } else { String::new() },
            if nw > 0 { format!(" + {nw} watchpoint") } else { String::new() },
            if nm > 0 { format!(" + {nm} method-exit") } else { String::new() },
            note.map_or_else(String::new, |t| format!("\n   ⚠️  {t}"))
        ))
    }

    async fn handle_get_stack(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        let a: crate::args::GetStackArgs = crate::args::parse(&args)?;
        let thread_id = crate::args::parse_thread_id(a.thread_id.as_deref());
        let max_frames = a.max_frames;
        let include_variables = a.include_variables;
        // Read-only: object expansion invokes toArray/toString in the debuggee, so it falls back to the
        // shallow `Type (id=…)` rendering rather than being refused outright (SAFE-3).
        let read_only = session.read_only;
        let expand_objects = a.expand_objects && !read_only;

        let last_thread = session.last_thread;
        let target_thread = resolve_target_thread(&mut session.connection, thread_id, last_thread).await?;

        // Get frames (-1 means all frames to avoid INVALID_LENGTH errors)
        let mut frames = session
            .connection
            .get_frames(target_thread, 0, -1)
            .await
            .map_err(|e| format!("Failed to get frames: {e}"))?;

        // Truncate to max_frames
        frames.truncate(max_frames);

        if frames.is_empty() {
            return Ok(format!("Thread {target_thread:x} has no stack frames"));
        }

        // Compact format: one line per frame `#idx class.method:line`, variables indented
        // beneath. Raw JDWP class/method ids are omitted — they're noise to the caller.
        // `package_filter` collapses frames whose class doesn't match (a JVM like WildFly buries a
        // few app frames under dozens of framework ones) into `… N frame(s) hidden` markers, and
        // skips the expensive method/variable round-trips for those hidden frames.
        let package_filter = a.package_filter.as_deref().filter(|s| !s.is_empty()).map(str::to_lowercase);

        let mut output = package_filter.as_ref().map_or_else(
            || format!("Stack (thread 0x{:x}, {} frames):\n", target_thread, frames.len()),
            |f| format!("Stack (thread 0x{:x}, {} frames, filter \"{}\"):\n", target_thread, frames.len(), f),
        );

        // ONE node budget for the whole call — see STACK_NODE_BUDGET. Deep expansion invokes methods
        // in the debuggee, which needs the suspended thread, so `deep` is Some only when asked for;
        // the default path stays cheap and side-effect-free (no toString() per local). The class-name
        // cache rides along because recursion and same-class frames are common.
        let mut state = StackWalkState {
            class_names: std::collections::HashMap::new(),
            hidden: 0,
            deep: expand_objects.then(|| {
                (
                    DeepOpts { depth_limit: a.max_depth, child_limit: a.max_children.max(1), text_len: 200 },
                    DeepState::new(STACK_NODE_BUDGET),
                )
            }),
        };
        if a.expand_objects && read_only {
            let _ = writeln!(output, "🔒 read-only: showing shallow values — expand_objects invokes methods in the debuggee, which is refused here.");
        }

        let walk = StackWalk { target_thread, package_filter: package_filter.as_deref(), include_variables };
        for (idx, frame) in frames.iter().enumerate() {
            let more =
                render_stack_frame(&mut session.connection, &mut output, idx, frame, &walk, &mut state).await;
            if !more {
                break;
            }
        }
        drop(session);
        flush_hidden(&mut output, &mut state.hidden);

        Ok(output)
    }

    async fn handle_evaluate(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::EvaluateArgs = crate::args::parse(&args)?;
        let expression = a.expression.as_str();
        let frame_index = a.frame_index;
        let max_len = a.max_result_length;

        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;
        // Read-only: invocation is refused by the connection itself (SAFE-6), so nothing here needs to
        // guess from the expression text — which used to miss `List.get` subscripts and `toString()`
        // rendering entirely. Deep expansion is still switched off up front so the reply can say why,
        // rather than expanding to a wall of refusals.
        let read_only = session.read_only;
        // A thread/frame is only needed to read locals or invoke methods. A pure static-field read
        // (Class.FIELD) works on a running VM, so a missing/un-suspended thread is not fatal here —
        // resolve_expression falls back to the static path when there's no frame.
        let thread_id = crate::args::parse_thread_id(a.thread_id.as_deref()).or(session.last_thread);
        let conn = &mut session.connection;

        let frame = match thread_id {
            Some(tid) => match conn.get_frames(tid, 0, -1).await {
                Ok(frames) if !frames.is_empty() => frames.get(frame_index).cloned().or_else(|| {
                    // Out-of-range index: fall back to the top frame rather than erroring, so a
                    // static read still works even if the requested frame doesn't exist.
                    frames.first().cloned()
                }),
                _ => None,
            },
            None => None,
        };

        let resolved = resolve_expression_multi(conn, thread_id, frame.as_ref(), expression)
            .await
            .map_err(explain_readonly)?;
        let deep = (a.expand_objects && !read_only).then(|| DeepOpts {
            depth_limit: a.max_depth,
            child_limit: a.max_children.max(1),
            text_len: max_len,
        });

        let rendered = match resolved {
            Resolved::One(value) => render_one(conn, &value, thread_id, max_len, deep).await,
            // A slice/filter result: the header carries how many of how many were selected, which is
            // as important as the values — "0 matched" and "0 scanned" mean very different things.
            Resolved::Many { header, values, keys } => {
                let shown = values.len().min(a.max_children.max(1));
                let mut out = format!("{header} {{");
                for (i, v) in values.iter().take(shown).enumerate() {
                    let r = render_one(conn, v, thread_id, max_len, deep).await;
                    // Map entries keep their keys; everything else is positional.
                    match keys.get(i) {
                        Some(k) => write!(out, "\n  {k} → {r}"),
                        None => write!(out, "\n  [{i}] = {r}"),
                    }
                    .unwrap_or_default();
                }
                if values.len() > shown {
                    let _ = write!(out, "\n  … +{} more (raise max_children)", values.len() - shown);
                }
                out.push_str("\n}");
                out
            }
        };
        drop(session);
        let ro_note = if a.expand_objects && read_only {
            "🔒 read-only: shallow rendering (expand_objects invokes methods)\n"
        } else {
            ""
        };
        Ok(format!("{ro_note}{} = {}", expression.trim(), rendered))
    }

    async fn handle_list_threads(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        let a: crate::args::ListThreadsArgs = crate::args::parse(&args)?;
        let name_filter = a.name_filter.as_deref().filter(|s| !s.is_empty()).map(str::to_lowercase);
        let only_suspended = a.only_suspended;
        let limit = a.limit.max(1);

        // Counted from before the thread list, so the cost line below covers every packet this call
        // spent — including the names it read only in order to choose (DUMP-5, #51).
        let before = session.connection.packets_sent();
        let wire_from = std::time::Instant::now();
        let all =
            session.connection.get_all_threads().await.map_err(|e| format!("Failed to get threads: {e}"))?;
        let total = all.len();

        let ThreadListing { rows, selection } =
            collect_thread_rows(&mut session.connection, &all, limit, name_filter.as_deref(), only_suspended)
                .await;
        let cost = session.connection.packets_sent().saturating_sub(before);
        let wire = wire_from.elapsed();
        drop(session);

        let shown = rows.len();
        let hidden = selection.eligible.saturating_sub(shown);

        let mut note = String::new();
        if let Some(f) = &name_filter {
            let _ = write!(note, " name~\"{f}\"");
        }
        if only_suspended {
            note.push_str(" suspended-only");
        }

        let mut output = format!("{shown}/{total} thread(s){note}:\n");
        output.push_str(&family_order_note(shown, &selection));
        for (tid, name, status) in &rows {
            let _ = match status {
                Some(s) => writeln!(output, "0x{tid:x} {name} [{s}]"),
                None => writeln!(output, "0x{tid:x} {name}"),
            };
        }
        if hidden > 0 {
            let _ = writeln!(
                output,
                "… +{hidden} more (raise limit or use name_filter){}",
                withheld_note(&selection.withheld)
            );
            // Only on a truncated listing, because that is the only shape that paid anything extra: a
            // listing that showed every thread read exactly the names it printed.
            output.push_str(&list_cost_note(cost, wire, shown, name_filter.is_some() || only_suspended));
        }

        Ok(output)
    }

    /// DISC-1: what the debuggee has actually loaded.
    ///
    /// Every stop point here is addressed by a fully-qualified class name, and until this existed the
    /// caller had to already know that name. The cases where they cannot are the ones that matter: a
    /// generated proxy, a shaded or relocated class, an EAR whose deployed build differs from the
    /// checkout in front of you. Only the debuggee knows what it loaded.
    ///
    /// Bounded rather than complete. A real app server loads thousands of types, so the reply reports
    /// matched-against-loaded and shows a page — truncating loudly, per DUMP-1, so a page is never
    /// mistaken for the whole answer.
    async fn handle_list_classes(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        let a: crate::args::ListClassesArgs = crate::args::parse(&args)?;
        let filter = a.filter.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let limit = a.limit.max(1);

        let all =
            session.connection.all_classes().await.map_err(|e| format!("Failed to list classes: {e}"))?;
        drop(session);
        let loaded = all.len();

        // Arrays outnumber the interesting entries on a real heap and are never the answer to "what do
        // I arm a stop point on", so they are excluded unless asked for.
        let names: Vec<(String, bool)> = all
            .into_iter()
            .filter(|c| a.include_arrays || c.ref_type_tag != REF_TAG_ARRAY)
            .map(|c| (decode_signature(&c.signature), c.ref_type_tag == REF_TAG_INTERFACE))
            .collect();
        // Borrowed rather than retained in place, because a miss is explained by re-reading the same
        // list under a looser spelling (SIG-1) and the rejected rows are exactly what that needs.
        let mut rows: Vec<&(String, bool)> = filter.map_or_else(
            || names.iter().collect(),
            |f| names.iter().filter(|(fqn, _)| class_matches(fqn, f)).collect(),
        );
        rows.sort_by(|x, y| x.0.cmp(&y.0));
        let matched = rows.len();
        let shown = matched.min(limit);

        let note = filter.map_or_else(String::new, |f| format!(" matching \"{f}\""));
        let mut output = format!("{shown}/{matched} class(es){note} — {loaded} loaded in the VM:\n");
        for (fqn, is_interface) in rows.iter().take(limit) {
            let _ =
                if *is_interface { writeln!(output, "{fqn} (interface)") } else { writeln!(output, "{fqn}") };
        }
        if matched > shown {
            let _ = writeln!(output, "… +{} more (raise limit, or narrow with filter)", matched - shown);
        }
        if matched == 0 {
            output.push_str(&explain_no_match(&names, filter));
        }

        Ok(output)
    }

    /// DISC-2: the methods of one loaded class, spelled the way Java source spells them.
    ///
    /// The method table was already being read — `debug.evaluate` resolves overloads against it — and
    /// the caller composing that call was the one person who could not see it. Resolution by runtime
    /// type is the most intricate machinery in this server, and composing arguments for it blind means
    /// a refused argument sends you back to guessing.
    async fn handle_list_methods(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        let a: crate::args::ListMethodsArgs = crate::args::parse(&args)?;
        let class_name = a.class_name.trim();
        if class_name.is_empty() {
            return Err("class_name is required (e.g. com.example.OrderService)".to_string());
        }
        let name_filter = a.name_filter.as_deref().filter(|s| !s.is_empty()).map(str::to_lowercase);
        let limit = a.limit.max(1);

        let target_id = resolve_loaded_class(&mut session.connection, class_name).await?;

        let mut rows =
            collect_method_rows(&mut session.connection, target_id, a.inherited, name_filter.as_deref())
                .await?;
        drop(session);

        // Sorted by rendered form so overloads land together, which is the comparison being made.
        rows.sort_by(|x, y| x.1.cmp(&y.1));
        let matched = rows.len();
        let shown = matched.min(limit);

        let mut note = String::new();
        if let Some(f) = &name_filter {
            let _ = write!(note, " name~\"{f}\"");
        }
        if a.inherited {
            note.push_str(" +inherited");
        }

        let mut output = format!("{shown}/{matched} method(s) on {class_name}{note}:\n");
        for (owner, rendered) in rows.iter().take(limit) {
            let _ = if a.inherited && &**owner != class_name {
                writeln!(output, "{rendered}  [from {owner}]")
            } else {
                writeln!(output, "{rendered}")
            };
        }
        if matched > shown {
            let _ = writeln!(output, "… +{} more (raise limit or use name_filter)", matched - shown);
        }
        if matched == 0 && name_filter.is_some() {
            output.push_str("No method name matched. Drop name_filter to see the whole class.\n");
        }

        Ok(output)
    }

    /// DISC-3: what file a loaded class was compiled from, and — when source roots are configured —
    /// the lines around the one a stack frame named.
    ///
    /// Two halves, deliberately independent. **The JVM half needs no local files at all**, and it is
    /// the half that settles whether the checkout in front of you is the code that is running: a class
    /// reporting `Order.java` when your tree renamed that file months ago is the answer, and no amount
    /// of reading local source would have shown it. The disk half is a convenience layered on top, so
    /// every way *it* can fail still reports the JVM half instead of collapsing into one error — the
    /// four local outcomes (no roots, no match, escaped a root, unreadable) each say something
    /// different about what to fix, and none of them makes the JVM's answer less true.
    ///
    /// The two genuinely empty-handed cases are the errors: the class is not loaded, or it is loaded
    /// and carries no `SourceFile` attribute at all.
    async fn handle_source(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        let a: crate::args::SourceArgs = crate::args::parse(&args)?;
        let class_name = a.class_name.trim().to_string();
        if class_name.is_empty() {
            return Err("class_name is required (e.g. com.example.OrderService)".to_string());
        }

        let type_id = resolve_loaded_class(&mut session.connection, &class_name).await?;
        let file_name = match session.connection.get_source_file(type_id).await {
            Ok(f) => f,
            Err(jdwp_client::JdwpError::JdwpErrorCode(code, _))
                if code == jdwp_client::protocol::ERR_ABSENT_INFORMATION =>
            {
                return Err(format!(
                    "{class_name} is loaded, but the JVM reports NO source file for it: the class was \
                     compiled without the SourceFile attribute (javac -g:none), or it is synthetic — a \
                     lambda body, a generated proxy, a bytecode-woven class. Nothing local can be \
                     resolved from a name this build does not carry. Rebuild the deployed artifact with \
                     debug info, or work from debug.list_methods and bytecode-level stop points."
                ));
            }
            Err(e) => return Err(format!("Failed to read the source file of {class_name}: {e}")),
        };
        // One extra packet, asked unconditionally because it is only interesting when it is there and a
        // caller cannot know in advance that it will be. Absent on nearly every class; when present it
        // means the `.java` above is a *translation artefact* and the file worth reading is elsewhere.
        // A hard error is dropped rather than reported: the client already answers `None` for the two
        // codes that mean "there is no SMAP", so anything left is a garnish failing on a reply the rest
        // of this tool does not need — losing the whole answer over it would be the wrong trade.
        let smap = session.connection.get_source_debug_extension(type_id).await.ok().flatten();
        let roots: Vec<std::path::PathBuf> = a.source_roots.as_ref().map_or_else(
            || session.source_roots.clone(),
            |v| v.iter().map(std::path::PathBuf::from).collect(),
        );
        drop(session);

        let mut output = format!("{class_name} — compiled from {file_name} (reported by the JVM)\n");
        if let Some(s) = &smap {
            let _ = writeln!(
                output,
                "Source debug extension (JSR-45 SMAP) present — this class was translated from another \
                 file, and {file_name} is the intermediate:\n{}",
                truncate(s.trim_end(), 800)
            );
        }
        output.push_str(&local_source_section(&class_name, &file_name, &roots, &a));
        Ok(output)
    }

    /// DUMP-1: every thread's stack in one call, plus which monitors each thread holds and which one it
    /// is blocked on — the "it's wedged, who is blocked on what?" question.
    ///
    /// Three things this deliberately does not do:
    /// - **Suspend on its own.** JDWP can only read a suspended thread's frames and locks, so a dump of a
    ///   running VM is mostly unreadable entries. Quietly pausing a shared instance to fix that is the
    ///   SAFE-4 mistake, so it takes an explicit `suspend:true` — and then resumes and *verifies*.
    /// - **Abort on one bad thread.** A thread that died mid-dump, or is running, is reported on its own
    ///   line; the rest of the dump still arrives.
    /// - **Invoke anything.** Frames, statuses and monitors are all plain reads, so this works in a
    ///   read-only session (SAFE-6).
    async fn handle_thread_dump(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::ThreadDumpArgs = crate::args::parse(&args)?;
        // Refused rather than silently corrected: monitors_only with monitors:false asks for neither
        // locks nor frames, so every row would come back empty — and an empty dump is exactly the
        // output that reads as "nothing is contended". Overriding one flag with the other would answer
        // a question the caller did not ask.
        if a.monitors_only && !a.monitors {
            return Err("monitors_only:true with monitors:false asks for neither locks nor stacks — \
                        every thread would come back empty, which reads as 'nothing is contended'. \
                        Drop one of the two."
                .to_string());
        }
        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;

        let before = session.connection.packets_sent();
        // Same start as the packet counter, so `wire / cost` is a per-packet figure over exactly the
        // packets it counts — including the suspend and resume, which are round trips like any other.
        let wire_from = std::time::Instant::now();
        let all =
            session.connection.get_all_threads().await.map_err(|e| format!("Failed to get threads: {e}"))?;
        let total = all.len();
        if all.is_empty() {
            return Ok("No threads — the JVM reported none.".to_string());
        }

        // Only ask about the monitor capabilities when monitors were actually requested, so a dump that
        // doesn't want them doesn't pay for the round trip.
        let caps = if a.monitors { session.connection.capabilities().await.ok() } else { None };

        // Suspension policy, decided ONCE up front so the resume half can't disagree with it.
        //
        // An already-suspended VM is read as it is and left alone: resuming it here would throw away
        // the breakpoint state the caller is standing in, and re-suspending it would build a counted
        // depth that one resume can't undo (SAFE-7).
        let already = session.suspended_cause.is_some();
        let suspend_now = a.suspend && !already;
        if suspend_now {
            session
                .connection
                .suspend_all()
                .await
                .map_err(|e| format!("Failed to suspend for the dump: {e}"))?;
            // Arm the watchdog for the window we hold it: if this call dies before the resume below,
            // something still un-freezes the VM (SAFE-4).
            session.mark_suspended(crate::session::SuspendCause::ManualPause);
        }

        // The held window starts here and ends at the resume below — measured around the reads only, so
        // our own string building can never inflate the number we report (#17).
        let held_from = std::time::Instant::now();
        // The budget bounds the SUSPENSION, so it only applies when we are the ones holding the VM. A
        // non-suspending dump reads whatever it can with no clock on it, and a VM someone else suspended
        // is not ours to hurry.
        let deadline = (suspend_now && a.max_suspend_ms > 0)
            .then(|| held_from + std::time::Duration::from_millis(a.max_suspend_ms));
        let dump = collect_dump_rows(&mut session.connection, &all, &a, caps.as_ref(), deadline).await;
        let rows = dump.rows;
        let held = suspend_now.then(|| held_from.elapsed());

        // Resume before rendering, so the VM is held for the reads and not for our string building.
        let mut resume_note = String::new();
        if suspend_now {
            let probe = rows.first().map_or_else(|| all.first().copied().unwrap_or(0), |r| r.id);
            match session.connection.resume_all_fully(probe, MAX_RESUME_ATTEMPTS).await {
                Ok((issued, 0)) => {
                    session.mark_resumed();
                    let _ = write!(
                        resume_note,
                        "▶️  Suspended for the dump and resumed again ({issued} resume(s)) — verified running."
                    );
                }
                // Honesty over convenience: a resume that "succeeded" while the VM stayed stopped is
                // the failure ADR-0003 exists for, so say so instead of reporting a clean dump.
                Ok((issued, left)) => {
                    let _ = write!(
                        resume_note,
                        "🛑 Suspended for the dump and the VM is STILL suspended after {issued} resume(s) \
                         ({left} suspend(s) left on the probe thread) — something outside this session is \
                         also holding it. Call debug.continue, or debug.panic."
                    );
                }
                Err(e) => {
                    let _ = write!(
                        resume_note,
                        "🛑 Suspended for the dump and the resume FAILED ({e}) — call debug.panic."
                    );
                }
            }
        }
        let cost = session.connection.packets_sent().saturating_sub(before);
        let wire = wire_from.elapsed();
        drop(session);

        let meta = DumpMeta {
            total,
            already_suspended: already,
            resume_note: &resume_note,
            cost,
            wire,
            held,
            unread: dump.unread,
            vanished: dump.vanished,
            selection: &dump.selection,
        };
        Ok(render_thread_dump(&rows, &a, caps.as_ref(), &meta))
    }

    async fn handle_pause(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        // Idempotent: suspending an already-suspended VM builds a counted suspend DEPTH that one resume
        // can't undo, so the watchdog would resume once, believe it had succeeded, clear
        // `suspended_since` and never retry — leaving the JVM frozen permanently while reporting it
        // rescued. Re-suspending would also overwrite a `StopPoint` cause with `ManualPause` and lose
        // the SAFE-2 disarm. So when it is already stopped, say so and change nothing (SAFE-7).
        if let Some(cause) = session.suspended_cause {
            let since = session.suspended_since.map_or(0, |t| t.elapsed().as_secs());
            let how = match cause {
                crate::session::SuspendCause::ManualPause => "by an earlier debug.pause",
                crate::session::SuspendCause::StopPoint(_) => "at a stop point",
            };
            drop(session);
            return Ok(format!(
                "⏸️  Already suspended {how} ({since}s ago) — left as it is.\n   Suspending again would \
                 need an extra debug.continue to undo, so this is a no-op. Use debug.continue to resume."
            ));
        }

        session.connection.suspend_all().await.map_err(|e| format!("Failed to suspend: {e}"))?;
        // Arm the watchdog for a MANUAL pause too. This used to suspend every thread and record
        // nothing, so `suspended_since` stayed None and the watchdog — the one thing that makes
        // attaching to a shared JVM defensible — never fired. A forgotten `debug.pause` froze the VM
        // permanently, the same hazard SAFE-1 fixed for disconnect (SAFE-4).
        session.mark_suspended(crate::session::SuspendCause::ManualPause);
        let secs = watchdog_secs();
        drop(session);

        Ok(format!(
            "⏸️  Execution paused (all threads suspended){}",
            if secs == 0 {
                " — ⚠️ the watchdog is disabled (JDWP_WATCHDOG_SECS=0), so nothing will auto-resume this. Call debug.continue.".to_string()
            } else {
                format!(" — the watchdog will auto-resume it after {secs}s if you don't. Call debug.continue when done.")
            }
        ))
    }

    async fn handle_disconnect(&self, args: serde_json::Value) -> Result<String, String> {
        let target = match args.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => Some(s.to_string()),
            None => self.session_manager.get_current_session_id().await,
        };
        let Some(session_id) = target else {
            return Err("No active debug session to disconnect".to_string());
        };

        // Leave the JVM RUNNING with nothing armed BEFORE dropping the session. A bare disconnect
        // used to abort the watchdog and drop the session without resuming — so disconnecting while
        // suspended at a breakpoint froze every thread forever, with nothing left alive to rescue it,
        // produced by the tool whose name sounds like the safe way out (SAFE-1). VirtualMachine.Dispose
        // is the JVM's own answer: it clears every event request and resumes every thread in one round
        // trip, and can't leave a request behind the way clearing our tracked set one by one might.
        let safety = if let Some(guard) = self.session_manager.get_session_by_id(&session_id).await {
            let mut session = guard.lock().await;
            let was_suspended = session.suspended_since.is_some();
            let stops = session.breakpoints.len()
                + session.pending_breakpoints.len()
                + session.exception_requests.len()
                + session.watchpoints.len();
            if let Some(req) = session.pending_step.take() {
                let _ = session.connection.clear_step(req).await;
            }
            let note = if session.connection.dispose().await.is_ok() {
                format!("cleared {stops} stop point(s) and resumed all threads")
            } else {
                // A half-dead socket is exactly the case this matters for: fall back to clearing what
                // we track and resuming, best effort, so a live-but-unresponsive Dispose still leaves
                // the VM as unfrozen as we can manage.
                let _ = session.connection.clear_all_breakpoints().await;
                let _ = session.connection.resume_all().await;
                format!(
                    "Dispose failed — best-effort cleared breakpoints and resumed ({stops} stop point(s))"
                )
            };
            session.mark_resumed();
            drop(session);
            Some((note, was_suspended))
        } else {
            None
        };

        self.session_manager.remove_session(&session_id).await;

        Ok(match safety {
            Some((note, was_suspended)) => format!(
                "✅ Disconnected from debug session: {session_id}\n   {note}{}",
                if was_suspended {
                    "\n   The VM was suspended at a stop point — it is now running."
                } else {
                    ""
                }
            ),
            None => format!("✅ Disconnected from debug session: {session_id}"),
        })
    }

    async fn handle_get_last_event(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::GetLastEventArgs = crate::args::parse(&args)?;
        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        if session.events.is_empty() {
            return Ok("No events received yet. Set a breakpoint and trigger it.".to_string());
        }

        // Newest last, matching `get_traces` — so a bare call (limit 1) prints exactly the latest
        // event as it always did, and a larger limit reads as a chronological tail.
        let total = session.events.len();
        let take = a.limit.max(1).min(total);
        let shown: Vec<crate::session::EventRecord> =
            session.events.iter().skip(total - take).cloned().collect();
        let (dropped, unshown) = (session.events_dropped, total - take);

        let mut lines: Vec<String> = Vec::new();
        for rec in &shown {
            // Compact, machine-readable summary only — one [event] line per event with the source
            // location resolved. Raw JDWP ids and the human-readable decoration are intentionally
            // omitted; they cost tokens and the caller never uses them.
            for ev in &rec.set.events {
                let mut obj = serde_json::Map::new();
                obj.insert("seq".to_string(), json!(rec.seq));
                obj.insert("event".to_string(), json!(event_type_name(&ev.details)));
                describe_event_into(&mut session.connection, &ev.details, &mut obj).await;
                lines.push(format!("[event] {}", serde_json::Value::Object(obj)));
            }
        }
        // The newest event is the last one printed, so this describes the state you are in now.
        let suspended = shown.last().is_some_and(|r| event_suspends(&r.set));
        if a.drain {
            session.events.clear();
        }
        let watchdog_note = session.last_watchdog_note.clone();
        drop(session);

        lines.push(format!("[suspended] {suspended}"));
        // If the watchdog auto-resumed while the caller was away, they'd otherwise read a stale
        // "suspended" state — tell them the VM was rescued and which stop point was disarmed (SAFE-2).
        if let Some(n) = watchdog_note {
            lines.push(format!("[watchdog] {n}"));
        }
        // Only when there is something to catch up on: silence means "you have seen everything".
        if unshown > 0 {
            lines.push(format!(
                "[pending] {unshown} older event(s) buffered — pass limit to read them, drain:true to discard"
            ));
        }
        if dropped > 0 {
            lines.push(format!(
                "[dropped] {dropped} event(s) evicted (buffer cap {}) — read events sooner, or narrow the breakpoint",
                crate::session::MAX_EVENTS
            ));
        }
        Ok(lines.join("\n"))
    }

    async fn handle_set_value(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::SetValueArgs = crate::args::parse(&args)?;
        let target = a.target.trim().to_string();
        let value_str = a.value.as_str();
        let frame_index = a.frame_index;

        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;
        if session.read_only {
            return Err(readonly_refusal("set_value writes to the JVM"));
        }
        let thread_opt = crate::args::parse_thread_id(a.thread_id.as_deref()).or(session.last_thread);
        let conn = &mut session.connection;

        let segs = parse_expr(&target)?;

        // A slice or filter names several elements, so there is no single place to write. Refused
        // explicitly: this used to parse the subscript and then silently drop it, writing the whole
        // field instead of the elements the caller named.
        if let Some(seg) = segs.iter().find(|s| s.subs.iter().any(|x| !matches!(x, Subscript::Index(_)))) {
            return Err(format!(
                "'{}[…]' selects several elements with a slice or filter, so there is nothing single \
                 to write. Use one index (e.g. [0]) to write one element.",
                seg.name
            ));
        }

        // `xs[0] = v` — an element write. The container is everything before the final `[…]`, which
        // resolve_expression handles including earlier subscripts (`grid[0][1]`).
        let last_seg = segs.last().ok_or_else(|| "Empty target path".to_string())?;
        if let Some(Subscript::Index(key)) = last_seg.subs.last().cloned() {
            let open = trailing_subscript_start(&target)
                .ok_or_else(|| format!("Could not find the final subscript in '{target}'"))?;
            let container_expr = target.get(..open).unwrap_or_default().trim().to_string();
            return set_element(conn, thread_opt, frame_index, &container_expr, &key, value_str).await;
        }

        // Single bare identifier → local variable in a suspended frame (the original behavior).
        if let [seg] = segs.as_slice() {
            return set_local_variable(conn, thread_opt, frame_index, seg, value_str).await;
        }

        // Multi-segment target: the last segment is the field; the prefix names the container.
        let written = set_field_by_path(conn, thread_opt, frame_index, &target, last_seg, value_str).await;
        drop(session);
        written
    }

    async fn handle_force_return(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::ForceReturnArgs = crate::args::parse(&args)?;

        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;
        if session.read_only {
            return Err(readonly_refusal("force_return changes what the JVM does"));
        }
        let thread_id = crate::args::parse_thread_id(a.thread_id.as_deref())
            .or(session.last_thread)
            .ok_or_else(|| "No thread. Pass thread_id, or hit a breakpoint first.".to_string())?;
        let conn = &mut session.connection;

        let frames = conn
            .get_frames(thread_id, 0, -1)
            .await
            .map_err(|e| format!("Failed to get frames (is the thread suspended?): {e}"))?;
        let frame =
            frames.first().cloned().ok_or_else(|| "Thread has no frames (not suspended?)".to_string())?;

        // The forced value must match the top method's declared return type. Pull the return
        // descriptor (the part after ')') so we coerce the literal correctly and handle void.
        let methods = conn
            .get_methods(frame.location.class_id)
            .await
            .map_err(|e| format!("Failed to get methods: {e}"))?;
        let method = methods
            .iter()
            .find(|m| m.method_id == frame.location.method_id)
            .ok_or_else(|| "Could not resolve the current method".to_string())?;
        let ret_sig = method.signature.rsplit(')').next().unwrap_or("V");
        let ret_byte = *ret_sig.as_bytes().first().unwrap_or(&b'V');

        let raw = a.value.as_deref().map_or("", str::trim);
        let value = if ret_byte == b'V' {
            jdwp_client::types::Value { tag: 86, data: jdwp_client::types::ValueData::Void }
        } else if raw.is_empty() {
            return Err(format!(
                "{}() returns {} — a 'value' is required (int, 123L, true/false, null, or \"string\")",
                method.name,
                decode_signature(ret_sig)
            ));
        } else {
            literal_to_value(conn, raw, ret_byte).await?
        };

        conn.force_early_return(thread_id, &value).await.map_err(|e| {
            format!(
                "ForceEarlyReturn failed (JVM may lack canForceEarlyReturn, or the value type is wrong): {e}"
            )
        })?;
        drop(session);

        let shown = if ret_byte == b'V' { "void".to_string() } else { raw.to_string() };
        Ok(format!(
            "✅ Forced {}() to return {} — thread still suspended; call debug.continue to let it proceed.",
            method.name, shown
        ))
    }

    async fn handle_set_exception_stop(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::SetExceptionBreakpointArgs = crate::args::parse(&args)?;
        if !a.caught && !a.uncaught {
            return Err(
                "Set at least one of caught/uncaught to true — otherwise nothing is reported.".to_string()
            );
        }

        let session_guard = self
            .resolve_session(&args)
            .await
            .ok_or_else(|| "No active debug session. Use debug.attach first.".to_string())?;
        let mut session = session_guard.lock().await;
        check_readonly_exprs(session.read_only, None, a.trace_expr.as_deref())?;

        // Resolve the target exception class to a ref type id (None => all exceptions). The class
        // must be loaded; unlike a line breakpoint we don't defer, because an exception request
        // needs a concrete referenceTypeID up front.
        let pattern = a.class_pattern.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let ref_type = match pattern {
            Some(p) => {
                let tid = resolve_class_by_dotted(&mut session.connection, p).await?
                    .ok_or_else(|| format!(
                        "Exception class '{p}' is not loaded yet — trigger it once so the JVM loads it, then retry (exception breakpoints can't be deferred)."
                    ))?;
                Some(tid)
            }
            None => None,
        };

        let thread_filter = crate::args::parse_thread_id(a.thread_id.as_deref());
        check_thread_filter(&mut session.connection, thread_filter).await?;
        let (trace_frames, frames_note) = clamp_trace_frames(a.trace, a.trace_frames);

        // A traced request suspends only the throwing thread, which the pump snapshots and resumes —
        // so a shared JVM keeps serving while you collect throws. An optional ThreadOnly restricts it
        // to one thread (FILT-1); the trace budget lives on our side (see try_record_trace) rather
        // than as a JDWP Count, because Count reports only the *Nth* throw, not the first N.
        let request_id = session
            .connection
            .set_exception_request_ex(
                ref_type,
                a.caught,
                a.uncaught,
                suspend_policy_for(a.trace),
                None,
                thread_filter,
            )
            .await
            .map_err(|e| format!("Failed to set exception breakpoint: {e}"))?;

        let class_pattern = pattern.unwrap_or("*").to_string();
        let exc_id = session.next_stop_id("exc_");
        session.exception_requests.insert(
            exc_id.clone(),
            crate::session::ExceptionRequestInfo {
                id: exc_id.clone(),
                request_id: Some(request_id),
                enabled: true,
                ref_type,
                class_pattern: class_pattern.clone(),
                caught: a.caught,
                uncaught: a.uncaught,
                trace: a.trace,
                trace_expr: a.trace_expr.clone(),
                trace_budget: trace_budget_for(a.trace, a.trace_max_hits),
                trace_frames,
                trace_cost: crate::session::TraceCost::default(),
                thread_filter,
            },
        );
        drop(session);

        Ok(render_exception_stop_reply(
            &a,
            &ExceptionStopReply {
                class_pattern: &class_pattern,
                exc_id: &exc_id,
                matches_all: pattern.is_none(),
                trace_frames,
                frames_note: frames_note.as_deref(),
                thread_filter,
            },
        ))
    }

    async fn handle_set_field_stop(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::SetWatchpointArgs = crate::args::parse(&args)?;
        let kinds = watch_kinds(&a)?;
        let class_name = a.class_name.trim();
        let field_name = a.field_name.trim();

        let session_guard = self
            .resolve_session(&args)
            .await
            .ok_or_else(|| "No active debug session. Use debug.attach first.".to_string())?;
        let mut session = session_guard.lock().await;
        check_readonly_exprs(session.read_only, None, a.trace_expr.as_deref())?;

        // A watchpoint needs a concrete fieldID up front, so — unlike a line breakpoint — it can't
        // be deferred until the class loads.
        let type_id = resolve_class_by_dotted(&mut session.connection, class_name).await?
            .ok_or_else(|| format!(
                "Class '{class_name}' is not loaded yet — exercise it once so the JVM loads it, then retry (watchpoints can't be deferred)."
            ))?;
        let (declaring_type, field) =
            find_field_info(&mut session.connection, type_id, field_name, None).await?.ok_or_else(|| {
                format!("Class '{class_name}' has no field '{field_name}' (nor does any superclass)")
            })?;
        let is_static = (field.mod_bits & ACC_STATIC) != 0;

        let thread_filter = crate::args::parse_thread_id(a.thread_id.as_deref());
        check_thread_filter(&mut session.connection, thread_filter).await?;
        let trace_budget = trace_budget_for(a.trace, a.trace_max_hits);
        let (trace_frames, frames_note) = clamp_trace_frames(a.trace, a.trace_frames);

        let spec = WatchSpec {
            arm: (declaring_type, field.field_id),
            class_name: class_name.to_string(),
            field_name: field_name.to_string(),
            is_static,
            trace: a.trace,
            trace_expr: a.trace_expr.as_deref(),
            trace_budget,
            trace_frames,
            thread_filter,
        };
        let mut ids = Vec::with_capacity(kinds.len());
        for kind in kinds {
            ids.push(arm_one_field_watch(&mut session, kind, &spec).await?);
        }
        drop(session);

        Ok(render_field_stop_reply(&a, &spec, &ids, &field, frames_note.as_deref()))
    }

    /// METH-1: report what a method actually returned, without having to guess which `return` runs.
    ///
    /// Two things make this kind different from every other stop point here, and both are safety:
    /// - **`trace` defaults to true.** A suspending `MethodExit` on a hot method is the fastest way to
    ///   freeze a shared JVM this tool offers, so the safe mode is the default and the dangerous one is
    ///   opt-in — the reverse of the other stop points.
    /// - **A broad suspending request is refused outright**, naming what would make it acceptable. JDWP
    ///   has no method-name modifier, so `ClassMatch` alone fires on *every method of every matching
    ///   class*; suspending on that is not a thing anyone means to ask for.
    async fn handle_set_method_exit_stop(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::SetMethodBreakpointArgs = crate::args::parse(&args)?;
        let class_pattern = a.class_pattern.trim();
        if class_pattern.is_empty() {
            return Err("Provide a class_pattern (e.g. \"br.com.infotravel.IntegraSrv\").".to_string());
        }
        let method = a.method.as_deref().map(str::trim).filter(|m| !m.is_empty()).map(str::to_string);

        // The refusal, before anything is armed.
        refuse_broad_suspending_method_exit(a.trace, class_pattern, method.as_deref())?;

        let session_guard = self
            .resolve_session(&args)
            .await
            .ok_or_else(|| "No active debug session. Use debug.attach first.".to_string())?;
        let mut session = session_guard.lock().await;
        check_readonly_exprs(session.read_only, None, a.trace_expr.as_deref())?;

        // Kind 42 (with the return value) when the JVM speaks JDWP >= 1.6, else plain kind 41. This is a
        // version check, not a capability bit — there is no `canGetMethodReturnValues` flag to read.
        let with_return_value = session.connection.can_get_method_return_values().await.unwrap_or(false);

        let thread_filter = crate::args::parse_thread_id(a.thread_id.as_deref());
        check_thread_filter(&mut session.connection, thread_filter).await?;
        let trace_budget = trace_budget_for(a.trace, a.trace_max_hits);
        let (trace_frames, frames_note) = clamp_trace_frames(a.trace, a.trace_frames);

        let request_id = session
            .connection
            .set_method_exit_request(
                class_pattern,
                with_return_value,
                suspend_policy_for(a.trace),
                None,
                thread_filter,
            )
            .await
            .map_err(|e| format!("Failed to set method-exit request on '{class_pattern}': {e}"))?;

        let mexit_id = session.next_stop_id("mexit_");
        session.method_exits.insert(
            mexit_id.clone(),
            crate::session::MethodExitRequestInfo {
                id: mexit_id.clone(),
                request_id: Some(request_id),
                enabled: true,
                class_pattern: class_pattern.to_string(),
                method: method.clone(),
                with_return_value,
                trace: a.trace,
                trace_expr: a.trace_expr.clone(),
                trace_budget,
                trace_frames,
                trace_cost: crate::session::TraceCost::default(),
                thread_filter,
            },
        );
        drop(session);

        let mut extra = String::new();
        let _ = match &method {
            Some(m) => write!(extra, "\n   Method filter: {m} (all overloads — JDWP compares names only)"),
            None => write!(
                extra,
                "\n   Method filter: none — EVERY method of every matching class reports its return. \
                 Pass `method` to narrow it."
            ),
        };
        if !with_return_value {
            let _ = write!(
                extra,
                "\n   ⚠️  This JVM speaks JDWP < 1.6, so it cannot report return VALUES \
                 (METHOD_EXIT_WITH_RETURN_VALUE). Degraded to a plain MethodExit: you get the return \
                 site — which `return` was taken — but not the value."
            );
        }
        if let Some(t) = thread_filter {
            let _ = write!(extra, "\n   Thread filter: 0x{t:x} (only returns on this thread)");
        }
        extra.push_str(&describe_trace_budget(a.trace, trace_budget));
        extra.push_str(&describe_trace_frames(
            a.trace,
            trace_frames,
            frames_note.as_deref(),
            "returning frame only",
        ));
        if a.trace {
            if let Some(e) = &a.trace_expr {
                let _ = write!(extra, "\n   Trace expr: {e}");
            }
        }
        let mode = if a.trace {
            "\n   Mode: trace (non-suspending) — each return is snapshotted with its value and the thread resumed; read them with debug.get_traces"
        } else {
            "\n   Mode: SUSPENDING — every matching return freezes all threads until you continue. Hits come back via debug.get_last_event.\n   ⚠️  On a shared JVM use trace:true (the default) instead."
        };
        Ok(format!(
            "✅ Method-exit reporting armed on {class_pattern}\n   Stop-point ID: {mexit_id}\n   JDWP Request ID: {request_id}{mode}{extra}"
        ))
    }

    async fn handle_get_traces(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::GetTracesArgs = crate::args::parse(&args)?;
        let session_guard =
            self.resolve_session(&args).await.ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;

        // FILT-2: the reader of an empty (or quiet) trace buffer is exactly who needs telling that a
        // filter is pinned to a dead thread — "no snapshots" and "this can never fire again" look
        // identical from here otherwise.
        let dead = dead_filter_threads(&mut session).await;
        let dead_note = if dead.is_empty() {
            String::new()
        } else {
            format!(
                "\n⚠️  {} stop point(s) are filtered to a thread that no longer exists, so they cannot \
                 record anything — this silence is not \"no hits\". See debug.list_stop_points, and re-arm \
                 with a live thread_id from debug.list_threads.",
                dead.len()
            )
        };

        if session.traces.is_empty() && session.trace_disarms.is_empty() {
            return Ok(format!(
                "No trace snapshots yet. Set a breakpoint with trace:true and trigger it.{dead_note}"
            ));
        }

        // Filter first (TRACE-4), so the "showing X of Y" counts and the `limit` tail both reflect what
        // the caller asked for rather than the whole buffer.
        let total = session.traces.len();
        let class_filter = a.class_filter.as_deref().map(str::to_lowercase);
        let matched: Vec<&crate::session::TraceRecord> = session
            .traces
            .iter()
            .filter(|r| a.bp_id.as_ref().is_none_or(|id| &r.bp_id == id))
            .filter(|r| a.since.is_none_or(|s| r.seq > s))
            .filter(|r| class_filter.as_ref().is_none_or(|c| r.class.to_lowercase().contains(c.as_str())))
            .collect();
        let n_matched = matched.len();
        let take = a.limit.min(n_matched);
        let start = n_matched - take;

        let filtered = a.bp_id.is_some() || a.class_filter.is_some() || a.since.is_some();
        let scope = if filtered { format!("{n_matched} matching of {total}") } else { format!("{total}") };
        let mut lines = Vec::with_capacity(take + 3);
        lines.push(format!(
            "📢 {scope} trace snapshot(s) (showing {take}, buffer cap {}):",
            crate::session::MAX_TRACES
        ));
        for rec in matched.into_iter().skip(start) {
            let callers_s = format_trace_callers(rec);
            let detail_s = format_trace_detail(rec);
            let args_s = format_trace_args(rec);
            let expr_s = format_trace_expr(rec);
            lines.push(format!(
                "#{} [{}] {}.{}:{}{} thread=0x{:x}{}{}{}",
                rec.seq,
                rec.bp_id,
                rec.class,
                rec.method,
                rec.line.unwrap_or(-1),
                callers_s,
                rec.thread,
                detail_s,
                args_s,
                expr_s
            ));
        }
        // A stop point that hit its budget disarmed itself (TRACE-3) — say so, so a caller doesn't
        // read the silence that follows as "no more hits". Kept until the buffer is cleared. Repeats are
        // collapsed into a count (SAFE-8), which is both bounded and easier to read.
        for (note, times) in &session.trace_disarms {
            match times {
                1 => lines.push(format!("⏸  {note}")),
                n => lines.push(format!("⏸  {note} (×{n})")),
            }
        }
        if session.trace_disarms_dropped > 0 {
            lines.push(format!(
                "[dropped] {} further disarm notice(s) (cap {}) — read and clear them sooner",
                session.trace_disarms_dropped,
                crate::session::MAX_TRACE_DISARMS
            ));
        }
        if a.clear {
            session.traces.clear();
            session.trace_disarms.clear();
            session.trace_disarms_dropped = 0;
            drop(session);
            lines.push("(buffer cleared)".to_string());
        }
        Ok(format!("{}{dead_note}", lines.join("\n")))
    }
}

/// Write `Container.field = value` where the target path has more than one segment.
///
/// Tries the instance field first, then the static one, because a suspended frame is the more common
/// case and the container is far more often an object than a dotted class name. Both misses are
/// reported together: "not an object" and "not a loaded class" are different failures, and a caller who
/// mistyped one needs to know which.
///
/// Split out of `handle_set_value`, which dispatches four shapes of target (element, local, instance
/// field, static field) and was over the complexity gate holding all four.
async fn set_field_by_path(
    conn: &mut jdwp_client::JdwpConnection,
    thread_opt: Option<u64>,
    frame_index: usize,
    target: &str,
    field_seg: &Seg,
    value_str: &str,
) -> Result<String, String> {
    if field_seg.args.is_some() {
        return Err("The last segment must be a field, not a method call".to_string());
    }
    let field_name = field_seg.name.clone();
    let raws = split_segments(target)?;
    let container_expr = raws.split_last().map_or_else(String::new, |(_, prefix)| prefix.join("."));

    // Instance-field attempt: resolve the container to an object using a suspended frame.
    let instance_err =
        match set_instance_field(conn, thread_opt, frame_index, &container_expr, &field_name, value_str)
            .await?
        {
            FieldWrite::Done(msg) => return Ok(msg),
            FieldWrite::Fallthrough(e) => e,
        };

    // Static-field attempt: treat the container as a dotted class name.
    if let Some(msg) =
        set_static_field(conn, thread_opt, frame_index, &container_expr, &field_name, value_str).await?
    {
        return Ok(msg);
    }

    Err(instance_err.map_or_else(
        || format!(
            "Could not write '{target}': '{container_expr}' is not a loaded class, and there's no suspended thread to resolve it as an object."
        ),
        |e| format!(
            "Could not write '{target}': '{container_expr}' didn't resolve to an object ({e}) and isn't a loaded class."
        ),
    ))
}

/// Refuse a SUSPENDING method-exit request that would report more than anyone can have meant to freeze
/// on (METH-1): a wildcard class pattern, or no method name at all.
///
/// JDWP has no method-name modifier, so a `ClassMatch` fires for every method of every matching class.
/// Suspending on that stops a shared VM faster than anything else this tool can do — so it is refused
/// rather than warned about, and the message names the narrow form that *is* accepted. Trace mode is
/// never refused: it snapshots and resumes, so breadth costs throughput, not availability.
fn refuse_broad_suspending_method_exit(
    trace: bool,
    class_pattern: &str,
    method: Option<&str>,
) -> Result<(), String> {
    if trace || !(class_pattern.contains('*') || method.is_none()) {
        return Ok(());
    }
    Err(format!(
        "🛑 Refused: a SUSPENDING method-exit request on `{class_pattern}`{} would freeze every thread \
         on every matching return. JDWP has no method-name filter, so a ClassMatch fires for every \
         method of every matching class — on a hot class that stops the VM faster than anything else \
         this tool can do.\n   Either keep trace:true (the default — snapshots and resumes, read them \
         with debug.get_traces), or narrow it to one concrete class AND one method: \
         {{\"class_pattern\": \"pkg.Class\", \"method\": \"save\", \"trace\": false}}.",
        if method.is_none() { " (no method filter)" } else { "" }
    ))
}

/// The caller-depth lines shared by every traced stop point's arm reply (TRACE-5): the depth, and any
/// clamp notice. `zero_hint` is the kind-specific "what you're missing at depth 0" wording.
///
/// One helper for all four kinds so the depth reads the same wherever it is reported — and because
/// inlining these branches into each `handle_set_*` pushed them past the complexity gate.
fn describe_trace_frames(trace: bool, frames: usize, note: Option<&str>, zero_hint: &str) -> String {
    let mut out = String::new();
    if !trace {
        return out;
    }
    let _ = match frames {
        0 => write!(out, "\n   Caller frames: 0 ({zero_hint})"),
        n => write!(out, "\n   Caller frames: {n}"),
    };
    if let Some(n) = note {
        let _ = write!(out, "\n   ⚠️  {n}");
    }
    out
}

/// The trace-mode lines of a `set_line_stop` reply: mode, trace expression, caller depth, and any
/// clamp notice. Empty for a suspending breakpoint, which has none of them.
fn describe_trace_mode(spec: &BreakpointSpec, frames_note: Option<&str>) -> String {
    let mut out = String::new();
    if !spec.trace {
        return out;
    }
    out.push_str("\n   Mode: trace (non-suspending) — read hits with debug.get_traces");
    if let Some(e) = &spec.trace_expr {
        let _ = write!(out, "\n   Trace expr: {e}");
    }
    // A line breakpoint never reported its budget at all, bounded or not — so the one stop point most
    // likely to be armed on a hot path was the one that said least about what it would cost (#22).
    out.push_str(&describe_trace_budget(spec.trace, spec.trace_budget));
    out.push_str(&describe_trace_frames(
        spec.trace,
        spec.trace_frames,
        frames_note,
        "hit frame only — pass trace_frames to see who called it",
    ));
    out
}

/// The suspend policy a stop point should be armed with. Shared by all three kinds (line breakpoint,
/// exception breakpoint, watchpoint) so "traced" means one thing everywhere.
///
/// A traced hit suspends only the hit thread — enough to read its frame — and the event pump resumes
/// it immediately, so nothing is left frozen. Anything else suspends every thread and waits for the
/// caller, which on a shared JVM stalls other people's requests.
const fn suspend_policy_for(trace: bool) -> jdwp_client::SuspendPolicy {
    if trace {
        jdwp_client::SuspendPolicy::EventThread
    } else {
        jdwp_client::SuspendPolicy::All
    }
}

/// Whether `JDWP_READONLY` forces read-only mode for every session (SAFE-3). Truthy = `1`/`true`/`yes`
/// (case-insensitive); anything else, or unset, is off.
fn env_readonly() -> bool {
    std::env::var("JDWP_READONLY")
        .ok()
        .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

/// The refusal a read-only session returns for a mutating tool (SAFE-3). Names what was refused and
/// how to lift the guard, and is explicit that it is a guard against accident, not a security boundary.
fn readonly_refusal(action: &str) -> String {
    format!(
        "🔒 Read-only session: {action}, which is refused. Reattach without read_only (or unset \
         JDWP_READONLY) to allow it. This is a guard against accident, not a security boundary."
    )
}

/// Refuse a `condition` / `trace_expr` that would invoke, at ARM time, in a read-only session.
///
/// The connection guard would refuse it anyway — but on every hit, deep inside the event pump where the
/// caller never sees it, and a condition that fails to evaluate keeps the VM suspended. Failing once,
/// here, is the difference between a clear error and a stop point that quietly doesn't work.
fn check_readonly_exprs(
    read_only: bool,
    condition: Option<&str>,
    trace_expr: Option<&str>,
) -> Result<(), String> {
    if !read_only {
        return Ok(());
    }
    for (what, expr) in [("condition", condition), ("trace_expr", trace_expr)] {
        if let Some(e) = expr.filter(|e| expr_invokes(e)) {
            return Err(format!(
                "🔒 Read-only session: the {what} `{e}` calls a method, which would have to execute code \
                 in the debuggee on every hit — refused. Use a comparison over fields instead (e.g. \
                 `status == \"OPEN\"`), or attach without read_only."
            ));
        }
    }
    Ok(())
}

/// Turn a read-only refusal raised deep in the resolver (by the connection's invocation guard) into an
/// explanation the caller can act on. Anything else passes through unchanged.
///
/// The refusal can come from further away than the expression suggests — a `List` subscript invokes
/// `get`, and rendering an object invokes `toString()` — so the message names what still works.
fn explain_readonly(e: String) -> String {
    if e.contains("read-only connection") {
        format!(
            "🔒 Read-only session: {e}\n   This expression needs to execute code in the debuggee \
             (a method call, a List/Map subscript, or boxing), which read-only refuses.\n   \
             Reads that need no invocation still work: locals, fields, statics, array indexing, \
             get_stack, and watchpoint/exception reporting.\n   Attach without read_only (or unset \
             JDWP_READONLY) if you need to invoke."
        )
    } else {
        e
    }
}

/// Whether an expression calls a method — a `(` at string-quote depth 0. Used to refuse an invoking
/// `condition`/`trace_expr` at ARM time in a read-only session, so it fails once where the caller is
/// looking instead of on every hit (the connection guard is the actual enforcement — SAFE-6).
/// A false positive only over-refuses, which is the safe direction; a `(` inside a string is ignored.
fn expr_invokes(expr: &str) -> bool {
    let mut in_str = false;
    let mut escaped = false;
    for c in expr.chars() {
        if in_str {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
        } else if c == '"' {
            in_str = true;
        } else if c == '(' {
            return true;
        }
    }
    false
}

/// Default number of hits a traced stop point records before disarming itself (TRACE-3). Bounds
/// per-hit work in the debuggee, not just our memory — `MAX_TRACES` caps the buffer, this caps the load.
const DEFAULT_TRACE_BUDGET: u32 = 200;

/// Default ceiling on how long `debug.thread_dump` may hold the VM suspended, in milliseconds (#17).
///
/// A dump freezes the debuggee for every round trip it makes, so the freeze grows with the thread count
/// and frame depth and is latency-bound on a remote JVM. 2s is chosen to bound the pathological case
/// without truncating a reasonable dump: a narrowed dump finishes well inside it, while "every frame of
/// every thread on a pool of hundreds" does not — which is the case that should have to ask.
///
/// **Provisional.** It is picked from loopback measurements, where a round trip is sub-millisecond; the
/// real per-thread cost against the shared instance is unmeasured, and calibrating this is part of #13.
pub const DEFAULT_MAX_SUSPEND_MS: u64 = 2000;

/// Default number of caller frames a traced hit records above itself (TRACE-5).
///
/// Not 0: a snapshot that can't say which path reached it fails the case trace mode exists for — a
/// swallowed exception on a shared JVM, where the question is always "which request got here". Not
/// large either: each frame is location lookups on *every* hit, so this is the smallest depth that
/// distinguishes callers of a shared helper.
pub const DEFAULT_TRACE_FRAMES: usize = 3;

/// Hard ceiling on `trace_frames`, whatever the caller asks for.
///
/// Depth multiplies per-hit JDWP traffic against a possibly-shared JVM, which is the flooding hazard
/// TRACE-3 exists for; 20 already matches `get_stack`'s default `max_frames`, so anything deeper is
/// better served by a suspending breakpoint and a real `get_stack`. A clamp is reported, never silent.
const MAX_TRACE_FRAMES: usize = 20;

/// Clamp a requested caller depth to `MAX_TRACE_FRAMES`, returning `(depth, note)` where `note` is a
/// sentence for the arm reply when the request was cut down — a silently ignored argument would leave a
/// caller believing they had a deeper chain than they do.
fn clamp_trace_frames(trace: bool, requested: usize) -> (usize, Option<String>) {
    if !trace {
        // A suspending stop point hands the caller a live thread; `debug.get_stack` gives the full
        // stack with locals, so there is nothing for a snapshot depth to do.
        return (0, None);
    }
    if requested > MAX_TRACE_FRAMES {
        return (
            MAX_TRACE_FRAMES,
            Some(format!(
                "trace_frames {requested} exceeds the {MAX_TRACE_FRAMES}-frame cap and was clamped to \
                 {MAX_TRACE_FRAMES} — deeper chains cost JDWP round trips on every hit; use a \
                 suspending breakpoint with debug.get_stack if you need the whole stack."
            )),
        );
    }
    (requested, None)
}

/// The trace-hit budget a stop point should arm with: the caller's `trace_max_hits` when tracing
/// (default `DEFAULT_TRACE_BUDGET`), where `0` means unbounded; `None` for a non-trace stop point,
/// which is unbounded because it suspends and so can't flood.
const fn trace_budget_for(trace: bool, trace_max_hits: Option<u32>) -> Option<u32> {
    if !trace {
        return None;
    }
    match trace_max_hits {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some(DEFAULT_TRACE_BUDGET),
    }
}

/// Everything a field watch needs that is the same for both of its kinds.
///
/// "modify + access" arms two independent JDWP requests over one field, and every field here is
/// identical between them — so this is resolved once and borrowed, rather than rebuilt per kind. It also
/// keeps `trace_expr` as a borrow: each `WatchpointInfo` still owns its own copy, but the copy is made
/// once per registration in `arm_one_field_watch` rather than cloned out of the arguments inside a loop.
struct WatchSpec<'a> {
    arm: (u64, u64),
    class_name: String,
    field_name: String,
    is_static: bool,
    trace: bool,
    trace_expr: Option<&'a str>,
    trace_budget: Option<u32>,
    trace_frames: usize,
    thread_filter: Option<u64>,
}

/// Arm one kind of field watch and register it, returning its `watch_<kind>_<n> (<kind>)` id label.
///
/// Extracted per kind for the reason the disarm helpers were: `handle_set_field_stop` was the most
/// complex function in the file, and the whole of its loop body is this. `ThreadOnly` restricts hits to
/// one thread (FILT-1); the trace budget is enforced on our side (`try_record_trace`), since a JDWP
/// `Count` reports only the Nth touch rather than the first N.
async fn arm_one_field_watch(
    session: &mut crate::session::DebugSession,
    kind: jdwp_client::WatchKind,
    spec: &WatchSpec<'_>,
) -> Result<String, String> {
    let (declaring_type, field_id) = spec.arm;
    let request_id = session
        .connection
        .set_field_watch_ex(
            declaring_type,
            field_id,
            kind,
            suspend_policy_for(spec.trace),
            None,
            spec.thread_filter,
        )
        .await
        .map_err(|e| {
            format!(
                "Failed to set {} watchpoint: {e} (error 99 NOT_IMPLEMENTED means this JVM lacks canWatchField{})",
                kind.label(),
                if kind == jdwp_client::WatchKind::Access { "Access" } else { "Modification" },
            )
        })?;
    let watch_id = session.next_stop_id(&format!("watch_{}_", kind.label()));
    let label = format!("{watch_id} ({})", kind.label());
    session.watchpoints.insert(
        watch_id,
        crate::session::WatchpointInfo {
            request_id: Some(request_id),
            enabled: true,
            arm: spec.arm,
            kind,
            class_name: spec.class_name.clone(),
            field_name: spec.field_name.clone(),
            is_static: spec.is_static,
            trace: spec.trace,
            trace_expr: spec.trace_expr.map(str::to_string),
            trace_budget: spec.trace_budget,
            trace_frames: spec.trace_frames,
            trace_cost: crate::session::TraceCost::default(),
            thread_filter: spec.thread_filter,
        },
    );
    Ok(label)
}

/// Which watch kinds a `debug.set_field_stop` call asked for, or the refusal when it asked for neither.
///
/// "modify + access" is two independent JDWP requests over one field, so this returns a list rather than
/// a flag pair.
fn watch_kinds(a: &crate::args::SetWatchpointArgs) -> Result<Vec<jdwp_client::WatchKind>, String> {
    let mut kinds = Vec::with_capacity(2);
    if a.modify {
        kinds.push(jdwp_client::WatchKind::Modify);
    }
    if a.access {
        kinds.push(jdwp_client::WatchKind::Access);
    }
    if kinds.is_empty() {
        return Err("Set at least one of modify/access to true — otherwise nothing is reported.".to_string());
    }
    Ok(kinds)
}

/// What `render_exception_stop_reply` needs beyond the caller's own arguments.
struct ExceptionStopReply<'a> {
    class_pattern: &'a str,
    exc_id: &'a str,
    /// No `class_pattern` was given, so this matches every exception thrown.
    matches_all: bool,
    trace_frames: usize,
    frames_note: Option<&'a str>,
    thread_filter: Option<u64>,
}

/// The `debug.set_exception_stop` reply: which throws it selected, under which id, and what it costs.
///
/// Split from the arming so each half stays under the complexity gate; everything here is wording.
fn render_exception_stop_reply(
    a: &crate::args::SetExceptionBreakpointArgs,
    r: &ExceptionStopReply<'_>,
) -> String {
    // `(false, false)` is rejected before arming, so the remaining case is "caught only".
    let which = match (a.caught, a.uncaught) {
        (true, true) => "caught + uncaught",
        (false, true) => "uncaught only",
        _ => "caught only",
    };
    let noisy = if r.matches_all {
        "\n   ⚠️  Matches ALL exceptions — expect frequent hits; clear it as soon as you're done."
    } else {
        ""
    };
    let mode = if a.trace {
        "\n   Mode: trace (non-suspending) — throws are snapshotted and the thread resumed; read them with debug.get_traces"
    } else {
        "\n   Hits are reported via debug.get_last_event.\n   ⚠️  Suspends ALL threads on each throw — on a shared JVM use trace:true instead."
    };
    let mut extra = String::new();
    if let Some(t) = r.thread_filter {
        let _ = write!(extra, "\n   Thread filter: 0x{t:x} (only throws on this thread)");
    }
    extra.push_str(&describe_trace_budget(a.trace, trace_budget_for(a.trace, a.trace_max_hits)));
    extra.push_str(&describe_trace_frames(
        a.trace,
        r.trace_frames,
        r.frames_note,
        "throwing frame only — pass trace_frames to see which path reached the catch",
    ));
    let (class_pattern, exc_id) = (r.class_pattern, r.exc_id);
    format!("✅ Exception breakpoint set on {class_pattern} ({which})\n   Stop-point ID: {exc_id}{mode}{noisy}{extra}")
}

/// The `debug.set_field_stop` reply: what was armed, under which id(s), and what it will cost.
///
/// Split from the arming for the reason the complexity gate exists — the two halves share only their
/// inputs, and every branch here is about wording rather than about the debuggee.
fn render_field_stop_reply(
    a: &crate::args::SetWatchpointArgs,
    spec: &WatchSpec<'_>,
    ids: &[String],
    field: &jdwp_client::reftype::FieldInfo,
    frames_note: Option<&str>,
) -> String {
    let mut extra = String::new();
    if let Some(t) = spec.thread_filter {
        let _ = write!(extra, "\n   Thread filter: 0x{t:x} (only touches on this thread)");
    }
    extra.push_str(&describe_trace_budget(a.trace, spec.trace_budget));
    extra.push_str(&describe_trace_frames(
        a.trace,
        spec.trace_frames,
        frames_note,
        "mutating frame only — pass trace_frames to see who called it",
    ));
    let kindness = if spec.is_static { "static" } else { "instance" };
    let where_hits = if a.trace {
        "   Mode: trace (non-suspending) — each hit is snapshotted with the mutating location and old → new value, then the thread resumes; read them with debug.get_traces."
    } else {
        "   Hits are reported via debug.get_last_event with the mutating location and old → new value.\n   ⚠️  Suspends ALL threads on each hit — on a shared JVM use trace:true instead."
    };
    format!(
        "✅ Watchpoint set on {}.{} ({kindness} {})\n   Stop-point ID(s): {}{extra}\n{where_hits}\n   ⚠️  A watched field can't be JIT-optimised — expect the debuggee to slow down; clear it when done.",
        spec.class_name,
        spec.field_name,
        decode_signature(&field.signature),
        ids.join(", "),
    )
}

/// The trace-budget line of an arm reply: how many hits it will record before disarming itself, or —
/// when the caller passed `trace_max_hits: 0` — that nothing will.
///
/// Unbounded used to print nothing at all, and that is the wrong silence (#22). Trace mode's safety on a
/// shared instance rests on two independent facts, and the tool descriptions only ever advertised the
/// first: it does not **freeze** the VM, and the default budget keeps even a hot site to a sub-second
/// blip. `trace_max_hits: 0` removes the second one, leaving a capture path that costs ~0.86ms per hit
/// and tops out near 720 hits/s — so a site firing faster than that is throttled for as long as the stop
/// point stays armed. Not freezing is not the same as not slowing. That trade is the caller's to make,
/// but not one to make by accident.
fn describe_trace_budget(trace: bool, budget: Option<u32>) -> String {
    if !trace {
        return String::new();
    }
    budget.map_or_else(
        || {
            "\n   ⚠️  UNBOUNDED (trace_max_hits: 0) — nothing will disarm this. Capture is serialised at \
             roughly 720 hits/s (~1160 with trace_frames: 0), so if this site fires faster than that, \
             every request through it queues behind the debugger for as long as it stays armed. Fine on \
             a quiet site; on a hot one set a budget, or clear it as soon as you have what you need."
                .to_string()
        },
        |b| format!("\n   Auto-disarms after {b} trace hit(s)"),
    )
}

/// Format one session into the `debug.list_sessions` output, as a whole line including its newline.
///
/// Liveness comes from the event pump: it exits when the connection closes, so a finished task means
/// the JVM is gone. That costs nothing to check, unlike a JDWP round trip — which could itself hang on
/// a half-dead socket, exactly the case this is meant to diagnose.
fn render_session_line(
    sid: &str,
    s: &crate::session::DebugSession,
    current: Option<&crate::session::SessionId>,
) -> String {
    let is_current = current.is_some_and(|c| c == sid);
    let dead = s.event_listener_task.as_ref().is_some_and(tokio::task::JoinHandle::is_finished);
    let state = if dead {
        "DEAD (JVM gone — debug.disconnect it)"
    } else if s.suspended_since.is_some() {
        "SUSPENDED"
    } else {
        "running"
    };
    let stops =
        s.breakpoints.len() + s.pending_breakpoints.len() + s.exception_requests.len() + s.watchpoints.len();
    let mut line = format!(
        "  {} [{}] {} — {}{}, {} stop point(s), {} JDWP packet(s)",
        if is_current { "▶" } else { " " },
        sid,
        s.endpoint,
        state,
        if s.read_only { " 🔒 read-only" } else { "" },
        stops,
        s.connection.packets_sent(),
    );
    // Buffer counts only when there is something to read, so a quiet session stays one short line.
    if !s.traces.is_empty() {
        let _ = write!(line, ", {} trace(s)", s.traces.len());
    }
    if !s.events.is_empty() {
        let _ = write!(line, ", {} event(s)", s.events.len());
    }
    if is_current {
        line.push_str(" ← current");
    }
    line.push('\n');
    line
}

/// Format one active breakpoint into the `debug.list_stop_points` output. `bp_id` is its map key.
fn render_breakpoint_line(
    output: &mut String,
    bp_id: &str,
    bp: &crate::session::BreakpointInfo,
    dead: &std::collections::BTreeSet<u64>,
) {
    let _ = writeln!(
        output,
        "  {} [{}] {}:{}{}{}{}{}",
        if bp.enabled { "✓" } else { "✗" },
        bp_id,
        bp.class_pattern,
        bp.line,
        if bp.trace { " (trace)" } else { "" },
        trace_budget_tag(bp.trace, bp.trace_budget),
        trace_frames_tag(bp.trace, bp.trace_frames),
        if bp.enabled { "" } else { " — DISABLED (definition kept; toggle to re-arm)" },
    );
    let tag = dead_filter_tag(bp.arm.thread_filter, dead);
    if !tag.is_empty() {
        let _ = writeln!(output, "   {tag}");
    }
    if let Some(method) = &bp.method {
        let _ = writeln!(output, "     Method: {method}");
    }
    if let Some(t) = bp.arm.thread_filter {
        let _ = writeln!(output, "     Thread filter: 0x{t:x}");
    }
    if let Some(c) = &bp.condition {
        let _ = writeln!(output, "     Condition: {c}");
    }
    if let Some(e) = &bp.trace_expr {
        let _ = writeln!(output, "     Trace expr: {e}");
    }
    if bp.hit_count > 0 {
        let _ = writeln!(output, "     Hits: {}", bp.hit_count);
    }
    render_trace_cost(output, bp.trace, &bp.trace_cost);
}

/// Format one deferred (class-prepare) breakpoint into the `debug.list_stop_points` output.
fn render_pending_line(
    output: &mut String,
    pb: &crate::session::PendingBreakpoint,
    dead: &std::collections::BTreeSet<u64>,
) {
    let where_ = match (pb.line, &pb.method) {
        (Some(l), _) => format!("line {l}"),
        (None, Some(m)) => format!("method {m}"),
        _ => "?".to_string(),
    };
    let _ = writeln!(
        output,
        "  ⏳ [{}] {} ({}) — waiting for class load{}",
        pb.bp_id,
        pb.class_pattern,
        where_,
        dead_filter_tag(pb.thread_filter, dead)
    );
}

/// Format one exception breakpoint into the `debug.list_stop_points` output.
fn render_exception_line(
    output: &mut String,
    er: &crate::session::ExceptionRequestInfo,
    dead: &std::collections::BTreeSet<u64>,
) {
    let which = match (er.caught, er.uncaught) {
        (true, true) => "caught+uncaught",
        (true, false) => "caught",
        (false, true) => "uncaught",
        (false, false) => "none",
    };
    let _ = writeln!(
        output,
        "  {} [{}] exception {} ({which}){}{}{}{}{}",
        if er.enabled { "⚡" } else { "✗" },
        er.id,
        er.class_pattern,
        if er.trace { " (trace)" } else { "" },
        trace_budget_tag(er.trace, er.trace_budget),
        trace_frames_tag(er.trace, er.trace_frames),
        er.thread_filter.map_or_else(String::new, |t| format!(" thread=0x{t:x}")),
        if er.enabled { "" } else { " — DISABLED (definition kept; toggle to re-arm)" },
    );
    let tag = dead_filter_tag(er.thread_filter, dead);
    if !tag.is_empty() {
        let _ = writeln!(output, "   {tag}");
    }
    render_trace_cost(output, er.trace, &er.trace_cost);
}

/// The ` [N hit(s) left]` budget suffix for a traced stop point in `list_stop_points`, kept separate
/// from the `(trace)` marker so the marker stays a stable substring (TRACE-3).
fn trace_budget_tag(trace: bool, budget: Option<u32>) -> String {
    match (trace, budget) {
        (true, Some(n)) => format!(" [{n} hit(s) left]"),
        _ => String::new(),
    }
}

/// What a traced stop point has cost so far, on its own line under the stop point (TRACE-7).
///
/// Three numbers, and each one is something the other two cannot give:
///  - **mean capture** — what one hit costs here: the observed version of #22's documented ~0.86 ms. Invert
///    it for the rate past which hits queue, which is the form #22's ~720 hits/s is quoted in;
///  - **arriving at N/s** — how hot the site actually is. Nothing else on the line reveals it;
///  - **the share of the window spent capturing** — their product, and the answer to "is this hurting the
///    instance?", which neither gives alone: a cheap capture on a hot line and a costly one on a quiet line
///    can cost the same.
///
/// A `sustains ~N/s` figure was reported here too and was **removed**: being exactly 1/mean, it added a
/// number without adding information, and made a reader work out which of two "rates" they were reading.
///
/// A traced stop point with **no** hits says so explicitly. "0.00 ms" would read as free, and unmeasured
/// is not free — the same silence-is-not-a-finding rule the rest of this tool follows.
///
/// Nothing at all for a suspending stop point: it does no capture, so it has no capture cost. Its price
/// is the freeze, which the watchdog and `thread_dump` report.
fn render_trace_cost(output: &mut String, trace: bool, cost: &crate::session::TraceCost) {
    if !trace {
        return;
    }
    let Some(mean) = cost.mean_capture() else {
        let _ = writeln!(
            output,
            "     ⏱  Trace cost: nothing captured yet — no hits recorded, so this is UNMEASURED rather \
             than free. If you expected hits, check the thread filter and that the line is reached."
        );
        return;
    };
    let mut line = format!(
        "     ⏱  Trace cost: {} capture(s), {:.2}ms mean",
        cost.captures,
        mean.as_secs_f64() * 1000.0
    );
    match (cost.observed_rate(), cost.capture_share()) {
        (Some(rate), Some(share)) => {
            let _ = write!(
                line,
                ", arriving at {:.1}/s ({:.1}% of the window spent capturing)",
                rate,
                share * 100.0
            );
        }
        // One capture establishes a cost but no interval, so there is no arrival rate to report yet.
        _ => line.push_str(", one capture so far, so no arrival rate yet"),
    }
    let _ = writeln!(output, "{line}");
}

/// The ` [+N caller frame(s)]` suffix for a traced stop point in `list_stop_points` (TRACE-5).
///
/// Shown because the depth is what makes a traced hit cost more than one round trip: a debuggee that
/// has slowed down should be explainable from the listing alone. Absent at depth 0, which is the
/// one-frame snapshot that costs nothing extra.
fn trace_frames_tag(trace: bool, frames: usize) -> String {
    if trace && frames > 0 {
        format!(" [+{frames} caller frame(s)]")
    } else {
        String::new()
    }
}

/// Describe one event into a `get_last_event` entry: where it happened, plus whatever is specific to
/// the kind. Everything a caller needs about a hit, in one place — so the trace path (TRACE-2) can
/// reuse the kind-specific halves and report exactly what a suspending hit would.
async fn describe_event_into(
    conn: &mut jdwp_client::JdwpConnection,
    details: &EventKind,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    use jdwp_client::events::EventKind as K;
    if let Some((thread, loc)) = event_location(details) {
        let (cls, method, line) = describe_location(conn, &loc).await;
        obj.insert("thread".to_string(), json!(format!("0x{thread:x}")));
        obj.insert("class".to_string(), json!(cls));
        obj.insert("method".to_string(), json!(method));
        obj.insert("line".to_string(), json!(line));
        describe_exception_event(conn, details, obj).await;
        describe_field_event(conn, details, obj).await;
        describe_method_exit_event(conn, details, obj).await;
        return;
    }
    // Events with no location still name their thread, and a class-prepare names its class.
    match details {
        K::VMStart { thread } | K::ThreadStart { thread } | K::ThreadDeath { thread } => {
            obj.insert("thread".to_string(), json!(format!("0x{thread:x}")));
        }
        K::ClassPrepare { thread, signature, .. } => {
            obj.insert("thread".to_string(), json!(format!("0x{thread:x}")));
            obj.insert("class".to_string(), json!(signature));
        }
        _ => {}
    }
}

/// Add an exception hit's details: the thrown type, whether it is caught, and where it is caught.
///
/// `caught` comes from the presence of a catch location, which is how JDWP reports it — an exception
/// with no catch location propagates out of the thread.
async fn describe_exception_event(
    conn: &mut jdwp_client::JdwpConnection,
    details: &EventKind,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    let EventKind::Exception { exception, catch_location, .. } = details else {
        return;
    };
    let exc_type = match conn.get_object_reference_type(*exception).await {
        Ok(t) => decode_signature(&conn.get_signature(t).await.unwrap_or_default()),
        Err(_) => "unknown".to_string(),
    };
    obj.insert("exception".to_string(), json!(exc_type));
    obj.insert("caught".to_string(), json!(catch_location.is_some()));
    if let Some(cl) = catch_location {
        let (cls, method, line) = describe_location(conn, cl).await;
        obj.insert("caught_at".to_string(), json!(format!("{}.{}:{}", cls, method, line.unwrap_or(-1))));
    }
}

/// Add a method-exit hit's returned value to a `get_last_event` / trace entry (METH-1).
///
/// `returned` is the answer this stop point exists for: **which value came back**, without having to
/// pick the right `return` statement first. The hit's own location already says which return was taken,
/// so the pair together answers "which path, with what".
///
/// Rendered with `thread` None, so no `toString()` runs in the debuggee while it sits inside the event —
/// the same discipline as the watchpoint describer. A `void` method reports `(void)`, which is a real
/// answer and not an absence.
///
/// `return_value` is `None` when the request was armed as a plain `METHOD_EXIT` (a JVM below JDWP 1.6),
/// and that is reported explicitly rather than omitted: silence would read as "returned nothing".
async fn describe_method_exit_event(
    conn: &mut jdwp_client::JdwpConnection,
    details: &EventKind,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    let EventKind::MethodExit { return_value, .. } = details else {
        return;
    };
    match return_value {
        Some(v) => {
            obj.insert("returned".to_string(), json!(render_value(conn, v, None, 200).await));
        }
        None => {
            obj.insert("returned".to_string(), json!("<not reported — this JVM speaks JDWP < 1.6>"));
        }
    }
}

/// Add a watchpoint hit's field details to a `get_last_event` entry: the field (as
/// `Declaring.name`), whether it is static, and its value(s).
///
/// For a modification the JVM reports the value the pending store *will* write, and the store has
/// not committed yet — so reading the field right now yields the value being replaced. That is where
/// `old`/`new` come from. It only holds while the hit thread is still suspended; after a
/// `debug.continue` the write lands and `old` would read back as the new value. A no-op write
/// (`x = x`) legitimately reports the same value on both sides.
///
/// The field name is resolved from the event's own declaring type rather than the session's
/// watchpoint list, so a hit still describes itself after the watchpoint has been cleared.
async fn describe_field_event(
    conn: &mut jdwp_client::JdwpConnection,
    details: &EventKind,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    use jdwp_client::events::EventKind as K;
    let (f, new_value) = match details {
        K::FieldAccess { field } => (field, None),
        K::FieldModification { field, new_value } => (field, Some(new_value)),
        _ => return,
    };
    let (ref_type, field_id, instance) = (f.ref_type, f.field_id, f.object);

    let declaring = decode_signature(&conn.get_signature(ref_type).await.unwrap_or_default());
    let info =
        conn.get_fields(ref_type).await.ok().and_then(|fs| fs.into_iter().find(|f| f.field_id == field_id));
    let (name, is_static) = info.map_or_else(
        // No field info means the type's field list didn't include the id the event named; fall back
        // to the raw id and infer staticness from whether an instance was reported.
        || (format!("field@{field_id:x}"), instance == 0),
        |f| (f.name, (f.mod_bits & ACC_STATIC) != 0),
    );
    obj.insert("field".to_string(), json!(format!("{declaring}.{name}")));
    obj.insert("static".to_string(), json!(is_static));
    if instance != 0 {
        obj.insert("instance".to_string(), json!(format!("0x{instance:x}")));
    }

    // Rendered with thread=None on purpose: no toString() invocation while the VM sits suspended
    // inside an event, which keeps reporting a hit side-effect-free.
    let current = if instance == 0 {
        conn.get_reference_values(ref_type, vec![field_id]).await.ok()
    } else {
        conn.get_object_values(instance, vec![field_id]).await.ok()
    }
    .and_then(|vs| vs.into_iter().next());

    match new_value {
        Some(nv) => {
            if let Some(old) = current {
                obj.insert("old".to_string(), json!(render_value(conn, &old, None, 200).await));
            }
            obj.insert("new".to_string(), json!(render_value(conn, nv, None, 200).await));
        }
        // A read doesn't change anything, so there is one value to report, not a pair.
        None => {
            if let Some(v) = current {
                obj.insert("value".to_string(), json!(render_value(conn, &v, None, 200).await));
            }
        }
    }
}

fn render_watchpoint_line(
    output: &mut String,
    watch_id: &str,
    wp: &crate::session::WatchpointInfo,
    dead: &std::collections::BTreeSet<u64>,
) {
    let _ = writeln!(
        output,
        "  {} [{}] watch {}.{} on {} ({}){}{}{}{}",
        if wp.enabled { "👁" } else { "✗" },
        watch_id,
        wp.class_name,
        wp.field_name,
        wp.kind.label(),
        if wp.is_static { "static" } else { "instance" },
        if wp.trace { " (trace)" } else { "" },
        trace_frames_tag(wp.trace, wp.trace_frames),
        wp.thread_filter.map_or_else(String::new, |t| format!(" thread=0x{t:x}")),
        if wp.enabled { "" } else { " — DISABLED (definition kept; toggle to re-arm)" },
    );
    // Budget on its own line to keep the header stable; harmless when absent.
    if let Some(n) = wp.trace_budget {
        let _ = writeln!(output, "     Trace budget: {n} hit(s) left");
    }
    let tag = dead_filter_tag(wp.thread_filter, dead);
    if !tag.is_empty() {
        let _ = writeln!(output, "   {tag}");
    }
    render_trace_cost(output, wp.trace, &wp.trace_cost);
}

/// Format one method-exit request into the `debug.list_stop_points` output (METH-1).
///
/// The `method` filter and the "returns values or not" fact both belong here: an unfiltered request is
/// reporting every method of the class, and a request that can't read return values answers a different
/// question from the one it was armed for. Neither should have to be re-derived from the arm reply.
fn render_method_exit_line(
    output: &mut String,
    me: &crate::session::MethodExitRequestInfo,
    dead: &std::collections::BTreeSet<u64>,
) {
    let _ = writeln!(
        output,
        "  {} [{}] method-exit {}{} ({}){}{}{}{}",
        if me.enabled { "↩" } else { "✗" },
        me.id,
        me.class_pattern,
        me.method.as_ref().map_or_else(|| ".* (every method)".to_string(), |m| format!(".{m}")),
        if me.with_return_value { "with return value" } else { "no return value — JDWP < 1.6" },
        if me.trace { " (trace)" } else { " ⚠️ SUSPENDING" },
        trace_budget_tag(me.trace, me.trace_budget),
        trace_frames_tag(me.trace, me.trace_frames),
        if me.enabled { "" } else { " — DISABLED (definition kept; toggle to re-arm)" },
    );
    if let Some(t) = me.thread_filter {
        let _ = writeln!(output, "     Thread filter: 0x{t:x}");
    }
    if let Some(e) = &me.trace_expr {
        let _ = writeln!(output, "     Trace expr: {e}");
    }
    let tag = dead_filter_tag(me.thread_filter, dead);
    if !tag.is_empty() {
        let _ = writeln!(output, "   {tag}");
    }
    render_trace_cost(output, me.trace, &me.trace_cost);
}

/// Resolve a frame's class name, using and populating a per-call cache (recursion / same-class
/// frames are common). Falls back to `class@<id>` when the signature can't be read.
async fn resolve_class_name(
    conn: &mut jdwp_client::JdwpConnection,
    class_id: u64,
    cache: &mut std::collections::HashMap<u64, String>,
) -> String {
    if let Some(n) = cache.get(&class_id) {
        return n.clone();
    }
    let n = conn
        .get_signature(class_id)
        .await
        .ok()
        .map(|s| decode_signature(&s))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("class@{class_id:x}"));
    cache.insert(class_id, n.clone());
    n
}

/// Resolve a frame's method name, source line, and (when requested) the variable slots in scope at
/// its bytecode index. Shared per-frame lookups for `debug.get_stack`.
async fn frame_method_info(
    conn: &mut jdwp_client::JdwpConnection,
    location: &Location,
    include_variables: bool,
) -> (String, Option<i32>, Vec<(String, jdwp_client::stackframe::VariableSlot)>) {
    let mut method_name = format!("method@{:x}", location.method_id);
    let mut line: Option<i32> = None;
    let mut active: Vec<(String, jdwp_client::stackframe::VariableSlot)> = Vec::new();
    if let Ok(methods) = conn.get_methods(location.class_id).await {
        if let Some(method) = methods.iter().find(|m| m.method_id == location.method_id) {
            method_name = method.name.clone();
            line = source_line(conn, location.class_id, location.method_id, location.index).await;
            if include_variables {
                if let Ok(var_table) = conn.get_variable_table(location.class_id, location.method_id).await {
                    let ci = location.index;
                    for v in var_table
                        .into_iter()
                        .filter(|v| ci >= v.code_index && ci < v.code_index + u64::from(v.length))
                    {
                        let slot = i32::try_from(v.slot).unwrap_or(0);
                        let sig_byte = v.signature.as_bytes().first().copied().unwrap_or(b'?');
                        active.push((v.name, jdwp_client::stackframe::VariableSlot { slot, sig_byte }));
                    }
                }
            }
        }
    }
    (method_name, line, active)
}

/// The thread a stack read should target: the caller's explicit choice, else the last thread that hit
/// a breakpoint or step, else the VM's first thread.
///
/// The last-hit fallback is what lets every other tool be called without a thread id after a
/// breakpoint fires, which is the common case; the first-thread fallback only matters on a VM nothing
/// has stopped yet.
async fn resolve_target_thread(
    conn: &mut jdwp_client::JdwpConnection,
    explicit: Option<u64>,
    last_hit: Option<u64>,
) -> Result<u64, String> {
    if let Some(tid) = explicit.or(last_hit) {
        return Ok(tid);
    }
    let threads = conn.get_all_threads().await.map_err(|e| format!("Failed to get threads: {e}"))?;
    threads.first().copied().ok_or_else(|| "No threads found".to_string())
}

/// Render a frame's in-scope variables beneath its stack line.
///
/// `deep` is `Some` only when the caller asked for expansion, and carries the budget shared by the
/// whole `get_stack` call — so the cap bounds the call, not each local (OBJ-3). Shallow rendering
/// deliberately passes `thread_id: None`, which keeps `get_stack` from invoking `toString()` on every
/// local of every frame — that would make the default path both slow and side-effecting.
///
/// Returns the name of the local the shared budget ran out on, if it did, so the caller can say where
/// it stopped and skip the remaining frames instead of emitting page after page of
/// "budget exhausted".
///
/// `frame` is `(index, id)`, and the index is the load-bearing half when expanding: deep expansion
/// invokes methods in the debuggee (`toArray`, `toString`), and JDWP invalidates a thread's frame ids
/// the moment a method is invoked on it. So any id read before an earlier frame was expanded is stale,
/// and reading locals through one fails — *silently*, printing a frame with no locals as though it had
/// none. Frame indices stay valid, so the id is re-read per frame. One extra round trip, only on the
/// path that already costs many.
async fn render_frame_variables(
    conn: &mut jdwp_client::JdwpConnection,
    output: &mut String,
    target_thread: u64,
    frame: (usize, u64),
    active: &[(String, jdwp_client::stackframe::VariableSlot)],
    mut deep: Option<(DeepOpts, &mut DeepState)>,
) -> Option<String> {
    let (idx, mut frame_id) = frame;
    if deep.is_some() && idx > 0 {
        let fresh = conn.get_frames(target_thread, i32::try_from(idx).unwrap_or(0), 1).await;
        if let Some(f) = fresh.ok().and_then(|fs| fs.into_iter().next()) {
            frame_id = f.frame_id;
        }
    }
    let slots: Vec<jdwp_client::stackframe::VariableSlot> = active.iter().map(|(_, s)| *s).collect();
    let Ok(values) = conn.get_frame_values(target_thread, frame_id, slots).await else {
        return None;
    };
    // The exhausted local is remembered as a borrow and only copied on the way out, so the budget check
    // costs nothing on the ordinary path where the budget is never reached.
    let mut exhausted_at = None;
    for ((name, _), value) in active.iter().zip(values.iter()) {
        let formatted_value = match &mut deep {
            Some((opts, state)) => render_node(conn, value, Some(target_thread), *opts, state, 0).await,
            None => render_value(conn, value, None, 200).await,
        };
        let _ = writeln!(output, "     {name} = {formatted_value}");
        if deep.as_ref().is_some_and(|(_, state)| state.exhausted()) {
            exhausted_at = Some(name);
            break;
        }
    }
    exhausted_at.cloned()
}

/// The `debug.get_stack` settings that are fixed for the whole walk.
struct StackWalk<'a> {
    target_thread: u64,
    /// Lower-cased class-name substring; frames that don't match collapse into a hidden count.
    package_filter: Option<&'a str>,
    include_variables: bool,
}

/// What a `debug.get_stack` walk carries from frame to frame.
struct StackWalkState {
    /// Class-name cache — recursion and same-class frames are common, and each miss is a round trip.
    class_names: std::collections::HashMap<u64, String>,
    /// Frames collapsed by `package_filter` since the last flush.
    hidden: usize,
    /// The deep-expansion options and the ONE node budget shared by every frame (see `STACK_NODE_BUDGET`).
    deep: Option<(DeepOpts, DeepState)>,
}

/// Render one frame of a `debug.get_stack` reply. Returns `false` when the walk should stop.
///
/// It stops for exactly one reason: the shared node budget ran out mid-frame, and continuing would
/// repeat "budget exhausted" under every local of every frame left. That is reported where it happened
/// rather than at the end, so the caller can see which local was expensive.
async fn render_stack_frame(
    conn: &mut jdwp_client::JdwpConnection,
    output: &mut String,
    idx: usize,
    frame: &jdwp_client::thread::Frame,
    walk: &StackWalk<'_>,
    state: &mut StackWalkState,
) -> bool {
    let class_name = resolve_class_name(conn, frame.location.class_id, &mut state.class_names).await;

    // Collapse frames whose class doesn't match the filter (and skip their lookups).
    if walk.package_filter.is_some_and(|f| !class_name.to_lowercase().contains(f)) {
        state.hidden += 1;
        return true;
    }
    flush_hidden(output, &mut state.hidden);

    // Method name + source line, and the variable slots live at this bytecode index.
    let (method_name, line, active) = frame_method_info(conn, &frame.location, walk.include_variables).await;

    let _ = match line {
        Some(l) => writeln!(output, "#{idx} {class_name}.{method_name}:{l}"),
        None => writeln!(output, "#{idx} {class_name}.{method_name}"),
    };

    if !walk.include_variables || active.is_empty() {
        return true;
    }
    let stopped_at = render_frame_variables(
        conn,
        output,
        walk.target_thread,
        (idx, frame.frame_id),
        &active,
        state.deep.as_mut().map(|(opts, st)| (*opts, st)),
    )
    .await;
    let Some(local) = stopped_at else { return true };
    let _ = writeln!(
        output,
        "   … node budget ({STACK_NODE_BUDGET}) exhausted at #{idx} {class_name}.{method_name} local `{local}` — remaining frames not expanded. Narrow with package_filter/max_frames/max_depth, or inspect one value with debug.evaluate."
    );
    false
}

// ===================================================================================
// Expression evaluation
//
// Supports `localVar`/`this` followed by `.field` and `.method(args)` chains, e.g.
//   reserva.getReservaPacote().getReservaHotelList().size()
//   map.get("key").getName()
// Field access uses ObjectReference.GetValues; method calls use ObjectReference.InvokeMethod,
// resolving overloads by arity and walking the superclass chain for inherited members.
// Supported argument literals: int, long (123L), boolean, null, and "string".
// ===================================================================================

use jdwp_client::events::EventKind;
use jdwp_client::extra::{value_bool, value_int, value_long, value_null, value_object};
use jdwp_client::types::Location;

/// A method-call argument (or the right-hand side of a breakpoint condition). Everything but
/// `Expr` is a self-contained literal; `Expr` is an arbitrary sub-expression (`reserva`,
/// `this.status`, `svc.getId()`) that must be resolved against a suspended frame before use.
#[derive(Debug, Clone)]
enum ArgLit {
    Int(i32),
    Long(i64),
    Bool(bool),
    Null,
    Str(String),
    Expr(String),
}

struct Seg {
    name: String,
    /// None = field access; Some = method call with these arguments (possibly empty).
    args: Option<Vec<ArgLit>>,
    /// Trailing `[…]` subscripts, applied left to right after the field/method resolves, so
    /// `grid[0][1]` and `orders[?paid == true]` both work.
    subs: Vec<Subscript>,
}

/// A `[…]` subscript. `Index` narrows to one value and keeps chaining; `Range` and `Filter` produce
/// several values and therefore end the expression (see [`Resolved`]).
#[derive(Debug, Clone)]
enum Subscript {
    /// `[3]` on an array/List, or `["key"]` / `[7]` on a Map.
    Index(ArgLit),
    /// `[2..5]` — half-open, like Rust's ranges, on an array or collection.
    Range(i64, i64),
    /// `[?predicate]` — keep elements the predicate holds for. The left side of the predicate is
    /// resolved *against each element*, so `orders[?status == "OPEN"]` needs no element variable.
    Filter(String),
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Split an expression into `.`-separated segments, ignoring dots inside () or "".
/// Split an expression on `.`, ignoring dots inside quotes, parentheses, or brackets. Brackets matter
/// as much as parens: a filter predicate like `[?customer.name == "Ana"]` is full of dots that belong
/// to the subscript, not to the outer chain.
fn split_segments(e: &str) -> Result<Vec<String>, String> {
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut in_str = false;
    for c in e.chars() {
        match c {
            '"' => {
                in_str = !in_str;
                cur.push(c);
            }
            '(' | '[' if !in_str => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' if !in_str => {
                depth -= 1;
                cur.push(c);
            }
            // A `..` range inside a subscript is at depth > 0, so it can't be mistaken for a chain
            // separator; only a top-level dot splits.
            '.' if !in_str && depth == 0 => {
                segs.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if depth != 0 || in_str {
        return Err("Unbalanced parentheses, brackets or quotes".to_string());
    }
    if !cur.trim().is_empty() {
        segs.push(cur.trim().to_string());
    }
    Ok(segs)
}

/// Split a raw segment into its `name`/`name(args)` head and its trailing `[…]` groups.
fn split_subscripts(raw: &str) -> Result<(String, Vec<String>), String> {
    // The head ends at the first `[` that is outside quotes and outside parentheses — parens can
    // legitimately contain a bracket, as in `foo(bar["k"])`.
    let mut depth = 0i32;
    let mut in_str = false;
    let mut head_end = raw.len();
    for (i, c) in raw.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => depth -= 1,
            '[' if !in_str && depth == 0 => {
                head_end = i;
                break;
            }
            _ => {}
        }
    }
    let head = raw[..head_end].trim().to_string();
    let mut rest = raw[head_end..].trim();
    let mut groups = Vec::new();
    while !rest.is_empty() {
        if !rest.starts_with('[') {
            return Err(format!("Unexpected text after a subscript: '{rest}'"));
        }
        let mut depth = 0i32;
        let mut in_str = false;
        let mut close = None;
        for (i, c) in rest.char_indices() {
            match c {
                '"' => in_str = !in_str,
                '[' if !in_str => depth += 1,
                ']' if !in_str => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else {
            return Err(format!("Unclosed '[' in '{raw}'"));
        };
        groups.push(rest[1..close].trim().to_string());
        rest = rest[close + 1..].trim();
    }
    Ok((head, groups))
}

/// Parse one `[…]` body: `?pred` is a filter, `a..b` a half-open range, anything else an index.
fn parse_subscript(inner: &str) -> Result<Subscript, String> {
    let t = inner.trim();
    if t.is_empty() {
        return Err("Empty subscript '[]' — use [i], [a..b], or [?predicate]".to_string());
    }
    if let Some(pred) = t.strip_prefix('?') {
        if pred.trim().is_empty() {
            return Err("Empty filter '[?]' — give a predicate, e.g. [?status == \"OPEN\"]".to_string());
        }
        return Ok(Subscript::Filter(pred.trim().to_string()));
    }
    if let Some((a, b)) = t.split_once("..") {
        let parse_bound = |x: &str, what: &str| -> Result<i64, String> {
            x.trim()
                .parse::<i64>()
                .map_err(|_| format!("Range {what} must be an integer, got '{}' in '[{t}]'", x.trim()))
        };
        let from = parse_bound(a, "start")?;
        let to = parse_bound(b, "end")?;
        if to < from {
            return Err(format!("Range '[{t}]' ends before it starts"));
        }
        return Ok(Subscript::Range(from, to));
    }
    Ok(Subscript::Index(parse_lit(t)?))
}

fn parse_lit(t: &str) -> Result<ArgLit, String> {
    let t = t.trim();
    if t == "null" {
        return Ok(ArgLit::Null);
    }
    if t == "true" {
        return Ok(ArgLit::Bool(true));
    }
    if t == "false" {
        return Ok(ArgLit::Bool(false));
    }
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        return Ok(ArgLit::Str(t[1..t.len() - 1].to_string()));
    }
    if let Some(num) = t.strip_suffix('L').or_else(|| t.strip_suffix('l')) {
        if let Ok(n) = num.parse::<i64>() {
            return Ok(ArgLit::Long(n));
        }
    }
    if let Ok(n) = t.parse::<i32>() {
        return Ok(ArgLit::Int(n));
    }
    if let Ok(n) = t.parse::<i64>() {
        return Ok(ArgLit::Long(n));
    }
    // Not a literal — accept it as a sub-expression if it parses as one (`reserva`, `this.status`,
    // `cfg.getName()`), so callers can pass an existing object by reference. Rejecting here would
    // otherwise be the only way to spell "unsupported token".
    if parse_expr(t).is_ok() {
        return Ok(ArgLit::Expr(t.to_string()));
    }
    Err(format!(
        "Unsupported argument: '{t}' (a literal — int, long like 123L, true/false, null, \"string\" — \
         or an expression like a local, this.field, or obj.getX())"
    ))
}

/// Split a call's argument list on top-level commas. Commas inside a string literal or nested
/// parentheses (`foo.matches(bar.key(1, 2))`) belong to the inner argument, not this list.
fn parse_args(inside: &str) -> Result<Vec<ArgLit>, String> {
    let s = inside.trim();
    if s.is_empty() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '"' => {
                in_str = !in_str;
                cur.push(c);
            }
            '(' if !in_str => {
                depth += 1;
                cur.push(c);
            }
            ')' if !in_str => {
                depth -= 1;
                cur.push(c);
            }
            ',' if !in_str && depth == 0 => {
                out.push(parse_lit(&cur)?);
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    out.push(parse_lit(&cur)?);
    Ok(out)
}

fn parse_seg(raw: &str) -> Result<Seg, String> {
    let (head, sub_groups) = split_subscripts(raw)?;
    let subs = sub_groups.iter().map(|g| parse_subscript(g)).collect::<Result<Vec<_>, _>>()?;

    if let Some(open) = head.find('(') {
        if !head.ends_with(')') {
            return Err(format!("Malformed method call: '{head}'"));
        }
        let name = head[..open].trim();
        if !is_ident(name) {
            return Err(format!("Bad method name: '{name}'"));
        }
        let args = parse_args(&head[open + 1..head.len() - 1])?;
        Ok(Seg { name: name.to_string(), args: Some(args), subs })
    } else {
        if !is_ident(&head) {
            return Err(format!("Unsupported token: '{head}'"));
        }
        Ok(Seg { name: head, args: None, subs })
    }
}

fn parse_expr(expr: &str) -> Result<Vec<Seg>, String> {
    let e = expr.trim();
    if e.is_empty() {
        return Err("Empty expression".to_string());
    }
    let raws = split_segments(e)?;
    if raws.is_empty() {
        return Err("Empty expression".to_string());
    }
    raws.iter().map(|r| parse_seg(r)).collect()
}

// JDWP reference type tags (`ClassInfo::ref_type_tag`).
const REF_TAG_INTERFACE: u8 = 2;
const REF_TAG_ARRAY: u8 = 3;

/// Match a dotted FQN against a DISC-1 filter: `com.example.*` (prefix), `*.OrderService` (suffix),
/// or a bare substring.
///
/// The suffix form also accepts the bare simple name, so `*.Order` finds a top-level `Order` in the
/// default package rather than silently missing it.
fn class_matches(fqn: &str, filter: &str) -> bool {
    match (filter.strip_suffix('*'), filter.strip_prefix('*')) {
        // Both anchors (`*Order*`) is a substring test with the stars removed.
        (Some(_), Some(_)) => fqn.contains(filter.trim_matches('*')),
        (Some(prefix), None) => fqn.starts_with(prefix),
        (None, Some(suffix)) => fqn.ends_with(suffix) || fqn == suffix.trim_start_matches('.'),
        (None, None) => fqn.contains(filter),
    }
}

/// What `debug.list_classes` says when its filter matched nothing (DISC-1).
///
/// SIG-1 (#46) is why this is a function rather than a string literal. The old note explained every miss
/// with class loading — *"a class the JVM has not loaded yet does not appear here at all"* — and that
/// was flatly wrong for the miss it was most likely to be printing. A caller who copied a lambda's real
/// name out of a stack trace got `0/0` and was sent to look for a code path that had never run, while
/// the class sat in the very list the tool had just searched, spelled differently by the tool itself.
///
/// So the first thing this does is *check*. Rejected rows are re-read with `/` and `.` treated as the
/// same separator, which catches a name in the JVM's internal form (`com/example/Order`), a hidden class
/// under either spelling, and the mangled names this tool handed out before #46. If any of them come
/// back, the answer is a spelling, and saying "not loaded" would be a lie the tool had the evidence to
/// avoid. Only when nothing comes back is the reading genuinely open — and then all three readings are
/// offered rather than one picked, which is `CONTEXT.md`'s standing rule under **Loaded**.
fn explain_no_match(names: &[(String, bool)], filter: Option<&str>) -> String {
    // Only in the miss path, so the per-name allocation buys honesty on a reply nobody is waiting on in
    // a loop. Both sides are normalised, so it does not matter which spelling the caller arrived with.
    let under_another_spelling: Vec<&str> = filter.map_or_else(Vec::new, |f| {
        let loose = f.replace('/', ".");
        names
            .iter()
            .filter(|(fqn, _)| !class_matches(fqn, f) && class_matches(&fqn.replace('/', "."), &loose))
            .map(|(fqn, _)| fqn.as_str())
            .take(10)
            .collect()
    });

    if under_another_spelling.is_empty() {
        return "Nothing matched — and this tool cannot tell you which of three things that means. The \
                class may not be loaded yet: classes load on first use, so an untouched code path \
                contributes none of its classes. There may be no such class. Or the name may simply be \
                spelled differently here — a hidden class (a lambda, a method reference, a generated \
                proxy) is named `Outer$$Lambda/<a suffix the JVM assigned>`, with the `/` part of the \
                name, and a nested class is `Outer$Inner`. Filters are substrings, so a fragment of the \
                name matches where the whole of it may not.\n"
            .to_string();
    }

    let mut note = format!(
        "Nothing matched that spelling — but {} loaded class(es) match it once `/` and `.` are read as \
         the same separator, so this is a spelling difference and NOT a class that is missing or \
         unloaded. The debuggee has them. Search for one of these instead:\n",
        under_another_spelling.len(),
    );
    for fqn in &under_another_spelling {
        let _ = writeln!(note, "{fqn}");
    }
    note
}

/// The reference type id of a loaded class named the Java way — `com.example.Order`, or an inner
/// class as `com.example.Order$Line`.
///
/// One resolver behind every discovery tool on purpose. "Not loaded" has to mean the same thing in
/// DISC-1, DISC-2 and DISC-3, and the honest wording is the part that would drift if each tool spelled
/// it out itself: JDWP knows only what is *loaded*, so it genuinely cannot separate a wrong name from
/// a class the VM has not touched yet. Picking one would be wrong about half the time, so the reply
/// says both are possible and names the tool that can actually tell them apart.
///
/// It asks for each of `descriptor_candidates`' spellings in turn rather than building one descriptor,
/// because a hidden class is spelled differently on JDK 11 and on 15+ (DISC-4, #50) — a normal class
/// still costs exactly one lookup.
async fn resolve_loaded_class(
    conn: &mut jdwp_client::JdwpConnection,
    class_name: &str,
) -> Result<u64, String> {
    for signature in descriptor_candidates(class_name) {
        let found = conn
            .classes_by_signature(&signature)
            .await
            .map_err(|e| format!("Failed to resolve {class_name}: {e}"))?;
        if let Some(class) = found.first() {
            return Ok(class.type_id);
        }
    }
    let simple = class_name.rsplit('.').next().unwrap_or(class_name);
    Err(format!(
        "{class_name} is not loaded in the debuggee. Either the name is wrong, or the JVM has not \
         loaded it yet — classes load on first use, so an untouched code path has none of its classes \
         present. To tell those apart: debug.list_classes with filter \"*.{simple}\"."
    ))
}

/// Every JNI descriptor a class name this tool printed could be spelled as on the wire, best-known
/// first — the inverse of `decode_internal_name`, and the reason it hands back a list rather than one.
///
/// DISC-4 (#50), the step SIG-1 (#46) left reachable. Once a hidden class is rendered under the name the
/// JVM answers to, a caller reads `SyntheticProbe$$Lambda/0x00007cd1e0001220` off a stack and naturally
/// asks the next question about it — and `resolve_loaded_class` built a single ordinary descriptor,
/// `L{name.replace('.', "/")};`, so the tool refused the very name it had just handed out and explained
/// the refusal as "not loaded" about a class it was looking straight at.
///
/// **The two wire shapes `decode_internal_name` documents are the two candidates here**, and which one
/// is right is a property of the JVM on the other end, not of the name:
///
/// * **JDK 11** (VM-anonymous classes): `LSyntheticProbe$$Lambda$3/574182878;` — a **slash**, which the
///   ordinary rewrite already produces. This half was never broken, and it is why the plain descriptor
///   stays first.
/// * **JDK 15+** (hidden classes): `LSyntheticProbe$$Lambda.0x00007cd1e0001220;` — a **dot**, because a
///   `/` there would not be a legal descriptor (JVMS §4.2.2). This is the one that missed.
///
/// **We do not decide which JVM we are talking to; we offer both spellings and let the debuggee answer.**
/// The tempting shortcut is to read the suffix — hex means 15+, decimal means 11 — and that is precisely
/// the JDK-locked reasoning #36's matrix already caught once, in #46's first pinned test. A lookup that
/// misses is a cheap packet on a path that was about to return an error anyway, and the debuggee is the
/// only authority that cannot be wrong about its own class list.
///
/// A normal class produces exactly one candidate and costs nothing new: the second is offered only when
/// the last `/` segment begins with a digit, which is the same boundary rule the forward transform leans
/// on — Java forbids a simple name starting with a digit, so such a segment is a suffix the VM assigned
/// and never package structure.
fn descriptor_candidates(class_name: &str) -> Vec<String> {
    let internal = class_name.replace('.', "/");
    let mut candidates = vec![format!("L{internal};")];
    if let Some((binary_name, assigned_by_the_vm)) = internal.rsplit_once('/') {
        if assigned_by_the_vm.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            candidates.push(format!("L{binary_name}.{assigned_by_the_vm};"));
        }
    }
    candidates
}

/// Collect a class's methods as `(declaring class, rendered signature)` pairs (DISC-2).
///
/// Kept flat rather than grouped by declaring class: an overload set spread across a class and its
/// parent is exactly the comparison the caller is trying to make, so splitting it into sections would
/// hide the thing they came for.
///
/// Split out of the handler because the superclass walk is the only real logic in it — the rest is
/// argument handling and formatting, and the two do not need to be read together.
async fn collect_method_rows(
    conn: &mut jdwp_client::JdwpConnection,
    start: u64,
    inherited: bool,
    name_filter: Option<&str>,
) -> Result<Vec<(std::sync::Arc<str>, String)>, String> {
    let mut rows = Vec::new();
    let mut current = Some(start);
    while let Some(type_id) = current {
        // `Arc<str>`, not `String`: every method of a class repeats its declaring class, so a plain
        // clone per row re-heap-allocates the same name once for each method — a refcount bump instead.
        let owner: std::sync::Arc<str> = std::sync::Arc::from(
            decode_signature(&conn.get_signature(type_id).await.unwrap_or_default()).as_str(),
        );
        let methods = conn
            .get_methods(type_id)
            .await
            .map_err(|e| format!("Failed to read the methods of {owner}: {e}"))?;
        for m in &methods {
            // `<clinit>` is the static initialiser: nothing can call it and nothing can usefully break
            // on it. `<init>` stays — a constructor is a real target for both evaluate and a stop point.
            if m.name == "<clinit>" {
                continue;
            }
            if name_filter.is_some_and(|f| !m.name.to_lowercase().contains(f)) {
                continue;
            }
            rows.push((std::sync::Arc::clone(&owner), render_method(&m.name, &m.signature, m.mod_bits)));
        }
        if !inherited {
            break;
        }
        current = conn
            .get_superclass(type_id)
            .await
            .map_err(|e| format!("Failed to walk the superclass chain: {e}"))?;
    }
    Ok(rows)
}

/// One method as Java source would spell it: `static boolean matches(java.lang.String, int)`.
///
/// Takes the fields rather than the client's struct so this stays a pure formatting function that a
/// unit test can drive with a literal descriptor.
fn render_method(name: &str, signature: &str, mod_bits: i32) -> String {
    // The return descriptor is everything after ')' — the same slice `force_return` takes.
    let ret = signature.rsplit(')').next().unwrap_or("V");
    let params: Vec<String> = sig_param_types(signature).iter().map(|p| decode_signature(p)).collect();

    let mut out = String::new();
    if mod_bits & ACC_STATIC != 0 {
        out.push_str("static ");
    }
    if mod_bits & ACC_ABSTRACT != 0 {
        out.push_str("abstract ");
    }
    if mod_bits & ACC_NATIVE != 0 {
        out.push_str("native ");
    }
    let _ = write!(out, "{} {}({})", decode_signature(ret), name, params.join(", "));
    out
}

/// JNI signature -> readable type name. "Lpkg/Cls;" -> "pkg.Cls"; "[I" -> "int[]".
///
/// The `/` -> `.` rewrite is deliberately not unconditional; see `decode_internal_name`.
fn decode_signature(sig: &str) -> String {
    let bytes = sig.as_bytes();
    let mut i = 0;
    let mut dims = 0;
    while bytes.get(i) == Some(&b'[') {
        dims += 1;
        i += 1;
    }
    let base = match bytes.get(i) {
        Some(b'L') => {
            let end = if sig.ends_with(';') { sig.len() - 1 } else { sig.len() };
            decode_internal_name(sig.get(i + 1..end).unwrap_or_default())
        }
        Some(b'Z') => "boolean".to_string(),
        Some(b'B') => "byte".to_string(),
        Some(b'C') => "char".to_string(),
        Some(b'S') => "short".to_string(),
        Some(b'I') => "int".to_string(),
        Some(b'J') => "long".to_string(),
        Some(b'F') => "float".to_string(),
        Some(b'D') => "double".to_string(),
        // Only ever appears as a method's return descriptor, which is why nothing needed it until
        // DISC-2 rendered whole signatures — `force_return` tests the raw byte instead.
        Some(b'V') => "void".to_string(),
        _ => sig.to_string(),
    };
    format!("{}{}", base, "[]".repeat(dims))
}

/// A JVM internal class name (`java/lang/String`) as Java spells it — including the `/` that a lambda's
/// generated class carries, which is not a package separator.
///
/// SIG-1 (#46). Every `/` used to become a `.`, which is right for package structure and wrong for the
/// name the JVM invents for a lambda. `Class.getName()`, a `jstack` dump, a `-verbose:class` line and a
/// stack trace all spell that name `<binary name>/<a suffix the JVM assigned>`, so
/// `SyntheticProbe$$Lambda/0x0000000092040970` came back as `SyntheticProbe$$Lambda.0x0000000092040970`
/// — which reads as a class `0x…` in a package `SyntheticProbe$$Lambda`, and is a name nothing outside
/// this tool will answer to. Worse one step out: `debug.list_classes` decoded the same way, so a caller
/// who pasted the JVM's own spelling got `0/0 class(es)` and was then told the class might not be
/// loaded, while the tool was looking straight at it.
///
/// **Two different wire spellings arrive here, and the issue only knew about one.** Measured against
/// live JVMs rather than assumed, because #36's matrix had already caught this shape changing between
/// legs:
///
/// * **JDK 15+** (hidden classes): `LSyntheticProbe$$Lambda.0x0000000092040970;` — the JDK writes a
///   **dot**, because a `/` there would not be a legal descriptor. So the separator is not being mangled
///   by us at all on a modern JVM; it arrives already replaced, and has to be put back.
/// * **JDK 11** (VM-anonymous classes, which predate hidden classes):
///   `LSyntheticProbe$$Lambda$3/574182878;` — an ordinal before a **slash**, a plain decimal after it.
///   This one is the rewrite's fault, and is what the issue describes.
///
/// The dot case is exact rather than a guess: JVMS §4.2.2 forbids `.` in an unqualified name, so a `.`
/// inside a descriptor's class name cannot be package structure and can only be this boundary. The slash
/// case cannot be exact — both separators are `/` on the wire — so it leans on the one thing Java
/// guarantees about the other side: **a simple name cannot begin with a digit**. Keying on `0x` instead
/// would have been written against 21 and broken on 11, which is the mistake the matrix already caught
/// once.
fn decode_internal_name(internal: &str) -> String {
    if let Some((binary_name, assigned_by_the_vm)) = internal.split_once('.') {
        return format!("{}/{}", binary_name.replace('/', "."), assigned_by_the_vm);
    }
    let mut out = String::with_capacity(internal.len());
    for (nth, segment) in internal.split('/').enumerate() {
        if nth > 0 {
            let assigned_by_the_vm = segment.as_bytes().first().is_some_and(u8::is_ascii_digit);
            out.push(if assigned_by_the_vm { '/' } else { '.' });
        }
        out.push_str(segment);
    }
    out
}

/// Count the top-level argument types in a method descriptor like "(ILjava/lang/String;)V".
fn sig_arg_count(sig: &str) -> usize {
    let (a, b) = match (sig.find('('), sig.find(')')) {
        (Some(a), Some(b)) if b > a => (a, b),
        _ => return 0,
    };
    let mut count = 0;
    let mut chars = sig.get(a + 1..b).unwrap_or_default().chars();
    while let Some(c) = chars.next() {
        match c {
            '[' => {} // array prefix; the following base type is the arg
            'L' => {
                for n in chars.by_ref() {
                    if n == ';' {
                        break;
                    }
                }
                count += 1;
            }
            _ => count += 1,
        }
    }
    count
}

/// A new session's default source roots (DISC-3): `JDWP_SOURCE_ROOTS`, a path list in this platform's
/// spelling — `:`-separated on Unix, `;` on Windows, which is what `std::env::split_paths` reads and
/// what the JVM's own `-cp` already uses, so an operator sets it the way they set every other path
/// list. Unset means no roots, and `debug.source` then reports only what the JVM knows.
fn env_source_roots() -> Vec<std::path::PathBuf> {
    std::env::var_os("JDWP_SOURCE_ROOTS")
        .map_or_else(Vec::new, |v| std::env::split_paths(&v).filter(|p| !p.as_os_str().is_empty()).collect())
}

/// Where a class's source sits *under* a root: the package as directories, then the file name the JVM
/// reported.
///
/// Built from the PACKAGE plus the JVM's file name, never from the class name, and that is the whole
/// point of asking the debuggee at all. `com.example.Order$Line` has no `Order$Line.java` to find;
/// neither does a package-private `class OrderRow` that lives inside `Order.java`. The package is the
/// only part of a class name that maps to a directory, and the JVM is the only source for the rest.
///
/// `None` when no path could be trusted: any segment that is empty, `.`, `..`, or carries a path
/// separator, a Windows drive marker or an NTFS stream marker. The file name arrives from the
/// DEBUGGEE — a `SourceFile` attribute reading `../../../../etc/passwd` is a perfectly valid class
/// file, so this is untrusted input, not a formality.
fn source_relative_path(class_name: &str, source_file: &str) -> Option<std::path::PathBuf> {
    let package = class_name.rsplit_once('.').map_or("", |(p, _)| p);
    let mut path = std::path::PathBuf::new();
    if !package.is_empty() {
        for segment in package.split('.') {
            if !is_safe_path_segment(segment) {
                return None;
            }
            path.push(segment);
        }
    }
    if !is_safe_path_segment(source_file) {
        return None;
    }
    path.push(source_file);
    Some(path)
}

/// Whether one path component can be joined onto a root without the result being able to leave it.
fn is_safe_path_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        // ':' covers both a Windows drive-relative segment (`C:foo`) and an NTFS alternate data
        // stream (`file.java:hidden`), neither of which joins the way `join` implies it does.
        && !segment.contains(['/', '\\', ':'])
}

/// What looking for one relative path under a list of roots found. Three outcomes rather than an
/// `Option`, because an escape is not a miss: it means a root held something pointing out of the tree,
/// and reporting that as "not found" would hide it.
enum SourceLookup {
    Found(std::path::PathBuf),
    Missing,
    Escaped(std::path::PathBuf),
}

/// Search `roots` in order for `rel`, refusing anything that resolves outside the root it was found
/// under.
///
/// The containment check is NOT redundant with [`source_relative_path`]'s segment rules. Those make
/// the *joined* path lexically safe; a symlink sitting inside a root can still point anywhere on the
/// disk, and only resolving the real path catches it. Canonicalising both sides is also what makes the
/// comparison meaningful on Windows, where one directory has several valid spellings.
fn find_under_roots(roots: &[std::path::PathBuf], rel: &std::path::Path) -> SourceLookup {
    for root in roots {
        let candidate = root.join(rel);
        if !candidate.is_file() {
            continue;
        }
        // `is_file` just succeeded, so a canonicalize failure here is a race or a permission problem
        // on the root itself — treat the root as not holding the file rather than trusting a path we
        // could not resolve.
        let (Ok(real_root), Ok(resolved)) = (root.canonicalize(), candidate.canonicalize()) else {
            continue;
        };
        if resolved.starts_with(&real_root) {
            return SourceLookup::Found(resolved);
        }
        return SourceLookup::Escaped(candidate);
    }
    SourceLookup::Missing
}

/// The 1-based inclusive line range a reply carries: the window around `line`, or the whole file, and
/// in both cases clamped to `max_lines`.
///
/// Pure, and separate from the reading, because the arithmetic is where this can be wrong in a way no
/// probe would catch: a `line` within `context` of either end of the file makes the window run off one
/// side, and a `max_lines` smaller than the window has to keep the requested line in shot rather than
/// just cutting the tail off.
fn line_window(total: usize, line: Option<usize>, context: usize, max_lines: usize) -> (usize, usize) {
    if total == 0 {
        return (1, 0);
    }
    let cap = max_lines.max(1);
    let Some(line) = line else {
        return (1, total.min(cap));
    };
    // A line past the end still returns the end of the file rather than nothing: the caller is chasing
    // a frame, and a file shorter than the line it named IS the finding.
    let centre = line.clamp(1, total);
    // Shrinking the context (rather than the far edge) keeps the requested line centred when the cap
    // is the binding constraint — a window cut only at the end would drop the lines *after* the frame,
    // which are usually the ones being read.
    let ctx = context.min(cap.saturating_sub(1) / 2);
    (centre.saturating_sub(ctx).max(1), centre.saturating_add(ctx).min(total))
}

/// The on-disk half of `debug.source`: resolve the class under `roots` and render the requested lines.
///
/// Returns text to append to the JVM-reported header rather than a `Result`, because none of the ways
/// this can come up empty invalidates that header — see [`RequestHandler::handle_source`].
fn local_source_section(
    class_name: &str,
    file_name: &str,
    roots: &[std::path::PathBuf],
    a: &crate::args::SourceArgs,
) -> String {
    if roots.is_empty() {
        return "No source roots are configured, so no file was read. Set them per session with \
                debug.attach {\"source_roots\":[...]}, or deploy-wide with JDWP_SOURCE_ROOTS (a path \
                list in this platform's spelling). A root is where the PACKAGE TREE starts — for \
                com.example.Order that is the directory containing `com`, not the project root.\n"
            .to_string();
    }
    let Some(rel) = source_relative_path(class_name, file_name) else {
        return format!(
            "⚠ Refusing to build a path from the file name the JVM reported ({file_name:?}): it \
             carries a path separator, a drive/stream marker or a `..` segment. That name comes from \
             the debuggee, so a path built from it could point outside every configured root.\n"
        );
    };
    let path = match find_under_roots(roots, &rel) {
        SourceLookup::Found(p) => p,
        SourceLookup::Missing => {
            let searched: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
            return format!(
                "Not found on disk: no configured root holds {}. Searched {} root(s): {}. Either the \
                 root list is wrong (a root is where the package tree starts) or this class is not in \
                 this checkout — which is itself worth knowing, since the JVM is running it.\n",
                rel.display(),
                roots.len(),
                searched.join(", "),
            );
        }
        SourceLookup::Escaped(p) => {
            return format!(
                "⚠ Refusing to read {}: it is under a configured root but resolves outside it — a \
                 symlink out of the tree. Nothing was read.\n",
                p.display(),
            );
        }
    };

    let lines: Vec<String> = match std::fs::read_to_string(&path) {
        Ok(text) => text.lines().map(str::to_string).collect(),
        Err(e) => {
            return format!(
                "Found {} but could not read it: {e}. The path resolved, so this is a local \
                 permission or encoding problem, not a wrong root.\n",
                path.display(),
            );
        }
    };
    render_source_body(&path, &lines, a)
}

/// Render the selected lines of a resolved file, with the bound stated in the header.
///
/// Split from [`local_source_section`] so the "which lines" decision is not tangled with the four ways
/// getting to a file can fail.
fn render_source_body(path: &std::path::Path, lines: &[String], a: &crate::args::SourceArgs) -> String {
    let total = lines.len();
    if total == 0 {
        return format!("{} resolved, but the file is empty (0 lines).\n", path.display());
    }
    let wanted = usize::try_from(a.line.unwrap_or(0)).ok().filter(|l| *l > 0);
    if !a.whole_file && wanted.is_none() {
        return format!(
            "Resolved to {} ({total} line(s)). No text returned: pass `line` for a window around it, \
             or whole_file:true for all of it. A whole file is never the default — a caller chasing \
             one frame does not want 2000 lines in context.\n",
            path.display(),
        );
    }
    // `whole_file` wins over `line`, per its documented argument: asking for both is asking for the
    // file, and a window silently applied on top would be the smaller answer to the larger question.
    let window = if a.whole_file { None } else { wanted };
    let (start, end) = line_window(total, window, a.context, a.max_lines);

    let mut out = format!("{} — lines {start}-{end} of {total}\n", path.display());
    // Only when a window was actually asked for: in `whole_file` mode the whole file IS the answer, and
    // "showing the end instead" would be a lie about what was returned.
    if let Some(l) = window.filter(|l| *l > total) {
        let _ = writeln!(
            out,
            "⚠ line {l} is past the end of this {total}-line file — this checkout almost certainly \
             does not match the running build. Showing the end of the file instead."
        );
    }
    let width = end.to_string().len();
    for (i, text) in
        lines.iter().enumerate().skip(start.saturating_sub(1)).take((end + 1).saturating_sub(start))
    {
        let _ = writeln!(out, "{:>width$} | {text}", i + 1);
    }
    if start > 1 || end < total {
        let _ = writeln!(
            out,
            "… {} of {total} line(s) shown; raise context/max_lines, or pass whole_file:true",
            (end + 1).saturating_sub(start),
        );
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let t: String = s.chars().take(max).collect();
        format!("{}… ({} chars total)", t, s.chars().count())
    } else {
        s.to_string()
    }
}

/// Find a method by name + argument count, walking the superclass chain.
async fn find_method_arity(
    conn: &mut jdwp_client::JdwpConnection,
    type_id: u64,
    name: &str,
    argc: usize,
) -> Result<Option<(u64, jdwp_client::reftype::MethodInfo)>, String> {
    let mut current = Some(type_id);
    let mut guard = 0;
    while let Some(tid) = current {
        guard += 1;
        if guard > 50 {
            break;
        }
        let methods = conn.get_methods(tid).await.map_err(|e| format!("Failed to get methods: {e}"))?;
        if let Some(m) = methods.into_iter().find(|m| m.name == name && sig_arg_count(&m.signature) == argc) {
            return Ok(Some((tid, m)));
        }
        current = conn.get_superclass(tid).await.unwrap_or(None);
    }
    Ok(None)
}

/// Split a method descriptor's parameter list into raw JNI type descriptors:
/// `"(I[Ljava/lang/String;Z)V"` -> `["I", "[Ljava/lang/String;", "Z"]`.
///
/// Unlike a tag-per-parameter view, this keeps each reference type's *name*, which is what lets
/// overload resolution tell `pick(String)` from `pick(Item)` — both of which are just tag 'L'.
fn sig_param_types(sig: &str) -> Vec<String> {
    let (a, b) = match (sig.find('('), sig.find(')')) {
        (Some(a), Some(b)) if b > a => (a, b),
        _ => return vec![],
    };
    let mut out = Vec::new();
    let mut chars = sig.get(a + 1..b).unwrap_or_default().chars();
    while let Some(first) = chars.next() {
        let mut t = String::from(first);
        // Array descriptors nest: consume every '[' to reach the element type.
        let mut base = first;
        while base == '[' {
            match chars.next() {
                Some(n) => {
                    t.push(n);
                    base = n;
                }
                None => break,
            }
        }
        if base == 'L' {
            for n in chars.by_ref() {
                t.push(n);
                if n == ';' {
                    break;
                }
            }
        }
        out.push(t);
    }
    out
}

/// Map a primitive JNI type char to its JDWP value tag; `None` for a non-primitive char.
const fn primitive_tag(c: char) -> Option<u8> {
    Some(match c {
        'Z' => 90,
        'B' => 66,
        'C' => 67,
        'S' => 83,
        'I' => 73,
        'J' => 74,
        'F' => 70,
        'D' => 68,
        _ => return None,
    })
}

/// Is a provided argument value tag acceptable for a parameter tag?
fn tag_compatible(param: u8, arg: u8) -> bool {
    let is_obj = |t: u8| matches!(t, 76 | 115 | 116 | 103 | 108 | 99 | 91);
    let is_num = |t: u8| matches!(t, 66 | 67 | 68 | 70 | 73 | 74 | 83);
    param == arg || (is_obj(param) && is_obj(arg)) || (is_num(param) && is_num(arg))
}

/// `ACC_STATIC` in a JVM method's access flags (JVMS 4.6).
const ACC_STATIC: i32 = 0x0008;
// The other two flags DISC-2 renders. Both mean "no body": you cannot put a line breakpoint in
// either, which is the thing a caller reading a method list needs to know before trying.
const ACC_NATIVE: i32 = 0x0100;
const ACC_ABSTRACT: i32 = 0x0400;

/// What an argument actually *is* at the moment of the call, which is what overload resolution
/// scores against.
enum ArgType {
    /// A primitive value carrying this JDWP tag.
    Primitive(u8),
    /// A null reference — assignable to any reference parameter.
    Null,
    /// A live object: its runtime type id, plus the JNI signatures of its class and every superclass,
    /// most specific first (always ending in `Ljava/lang/Object;`).
    ///
    /// The chain answers "is this parameter one of my supertypes?" without a round trip. The type id is
    /// what makes the *interface* question askable, since JDWP reports only direct superinterfaces and
    /// the lattice has to be walked (see `JdwpConnection::implements_interface`).
    Object { type_id: u64, chain: Vec<String> },
}

/// Classify one argument value, reading the object's runtime class chain when it is a reference.
///
/// The chain is what makes an object argument resolvable to a specific overload: a parameter is
/// assignable from the argument exactly when its declared type appears in the chain. Interface-typed
/// parameters are not in the chain — they are settled separately by [`assignable`], which asks the JVM.
async fn arg_type(conn: &mut jdwp_client::JdwpConnection, v: &jdwp_client::types::Value) -> ArgType {
    let id = match v.data {
        jdwp_client::types::ValueData::Object(0) => return ArgType::Null,
        jdwp_client::types::ValueData::Object(id) => id,
        _ => return ArgType::Primitive(v.tag),
    };
    let runtime_type = conn.get_object_reference_type(id).await.unwrap_or(0);
    let mut chain = Vec::new();
    let mut current = (runtime_type != 0).then_some(runtime_type);
    let mut guard = 0;
    while let Some(tid) = current {
        guard += 1;
        if guard > 50 {
            break;
        }
        match conn.get_signature(tid).await {
            Ok(s) => chain.push(s),
            Err(_) => break,
        }
        current = conn.get_superclass(tid).await.unwrap_or(None);
    }
    // Array types have no walkable superclass chain, so make the universal supertype explicit.
    if !chain.iter().any(|s| s == "Ljava/lang/Object;") {
        chain.push("Ljava/lang/Object;".to_string());
    }
    ArgType::Object { type_id: runtime_type, chain }
}

/// Score how well `arg` fits the parameter descriptor `param`: `None` = not assignable at all,
/// higher = more specific. Scoring by specificity is what makes `pick(Item)` beat `pick(Object)`
/// for an `Item` argument, and an exact `int` beat a widened `long`.
fn score_param(param: &str, arg: &ArgType) -> Option<u32> {
    let is_ref = param.starts_with('L') || param.starts_with('[');
    match arg {
        ArgType::Null => is_ref.then_some(1),
        ArgType::Primitive(tag) => {
            let ptag = param.chars().next().and_then(primitive_tag)?;
            if !tag_compatible(ptag, *tag) {
                return None;
            }
            Some(if ptag == *tag { 2 } else { 1 })
        }
        ArgType::Object { chain, .. } => {
            if !is_ref {
                return None;
            }
            let idx = chain.iter().position(|s| s == param)?;
            // Distance from the end of the chain: the runtime class itself scores highest.
            Some(u32::try_from(chain.len() - idx).unwrap_or(1) + 1)
        }
    }
}

/// Settle the cases [`score_param`] can't, by asking the JVM. `None` = genuinely not assignable.
///
/// Three things the superclass chain alone cannot answer:
/// - **An interface-typed parameter** (`handle(Runnable)`): JDWP reports only *direct*
///   superinterfaces, so the lattice has to be walked — `implements_interface` does it through the type
///   cache, and the answer is authoritative. An object that does *not* implement it is now **rejected**
///   rather than passed anyway.
/// - **A boxed primitive** (`f(Integer)` given an `int`): assignable via autoboxing, and the value is
///   boxed for real before the invoke — see [`coerce_args`].
/// - **Array covariance** (`f(Object[])` given a `String[]`): element assignability isn't checkable from
///   a signature, so any array is accepted for any array parameter. The JVM type-checks references
///   itself, so the worst case is a rejected invoke, not a crash.
///
/// Everything scores 1 — the lowest rung. These are all *less* specific than a match `score_param`
/// found, so an exact overload always wins.
///
/// A primitive argument for a non-boxing reference parameter stays a hard mismatch. That is not
/// pedantry: JDWP hands the raw int straight to the JVM, which reads it as an object pointer and dies
/// with a SIGSEGV — the debuggee crashes rather than reporting an error.
///
/// Reference mismatches are just as important to catch here, because **the JVM does not catch them**.
/// Measured: with the old blind fallback, `takesRunnable(anItem)` *succeeded* and returned normally —
/// `InvokeMethod` accepted an object that does not implement the parameter's interface. Nothing failed
/// because that method body never used the argument; one that called `r.run()` would have been acting on
/// a value of the wrong type. So being wrong here is silent, not loud, which is why the check is strict.
async fn assignable(conn: &mut jdwp_client::JdwpConnection, param: &str, arg: &ArgType) -> Option<u32> {
    match arg {
        // Handled entirely by `score_param`: null fits any reference, and a primitive either widens
        // into a primitive parameter or boxes into its own wrapper.
        ArgType::Null => None,
        ArgType::Primitive(tag) => boxed_wrapper_of(*tag).filter(|w| w == &param).map(|_| 1),
        ArgType::Object { type_id, chain } => {
            if param.starts_with('[') {
                // Array parameter: accept only an array argument.
                return chain.first().is_some_and(|s| s.starts_with('[')).then_some(1);
            }
            if !param.starts_with('L') || *type_id == 0 {
                return None;
            }
            conn.implements_interface(*type_id, param).await.unwrap_or(false).then_some(1)
        }
    }
}

/// The JNI signature of the wrapper class a primitive tag autoboxes into.
const fn boxed_wrapper_of(tag: u8) -> Option<&'static str> {
    Some(match tag {
        b'I' => "Ljava/lang/Integer;",
        b'J' => "Ljava/lang/Long;",
        b'S' => "Ljava/lang/Short;",
        b'B' => "Ljava/lang/Byte;",
        b'C' => "Ljava/lang/Character;",
        b'Z' => "Ljava/lang/Boolean;",
        b'F' => "Ljava/lang/Float;",
        b'D' => "Ljava/lang/Double;",
        _ => return None,
    })
}

/// Box any primitive argument whose parameter is a reference type, so `f(Integer)` called with `5`
/// receives a real `Integer` — handing the JVM a raw int for a reference parameter is what SIGSEGVs it.
///
/// Called after overload selection, on the chosen method's signature, so it boxes exactly what that
/// method's parameters require. A no-op for the common case, and it costs a `valueOf` invoke per
/// argument it does box.
async fn coerce_args(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: u64,
    signature: &str,
    args: Vec<jdwp_client::types::Value>,
) -> Result<Vec<jdwp_client::types::Value>, String> {
    let params = sig_param_types(signature);
    let mut out = Vec::with_capacity(args.len());
    for (i, v) in args.into_iter().enumerate() {
        let wants_ref = params.get(i).is_some_and(|p| p.starts_with('L') || p.starts_with('['));
        let is_primitive = !matches!(v.data, jdwp_client::types::ValueData::Object(_));
        if wants_ref && is_primitive {
            let boxed = box_primitive(conn, thread_id, &v).await.ok_or_else(|| {
                format!(
                    "argument {} is a primitive but parameter {} is {} — boxing it via valueOf failed",
                    i + 1,
                    i + 1,
                    params.get(i).map_or("a reference type", String::as_str),
                )
            })?;
            out.push(boxed);
        } else {
            out.push(v);
        }
    }
    Ok(out)
}

/// Find the method `name` to invoke for a concrete argument list, walking the superclass chain.
///
/// Two passes, cheap first. Overloads of matching arity are scored by how specifically each parameter
/// accepts its argument ([`score_param`], no round trips) and the best-scoring one wins; ties go to the
/// most derived class, since the walk starts at the runtime type. Only if *nothing* scored are the
/// arity-matching leftovers put to the JVM ([`assignable`]) — which is where an interface-typed
/// parameter, a boxed primitive, or array covariance gets settled, at the cost of some round trips.
///
/// An overload no pass can justify is **not** used. A mere arity match once handed the JVM an `int` for
/// a reference parameter, which it read as an object pointer and died on.
///
/// `want_static` filters on the method's `ACC_STATIC` flag: `Some(true)` for a `Class.m()` call
/// (JDWP's `ClassType.InvokeMethod` only accepts statics), `Some(false)` for an instance call, and
/// `None` to accept either.
async fn find_method_for_args(
    conn: &mut jdwp_client::JdwpConnection,
    type_id: u64,
    name: &str,
    args: &[jdwp_client::types::Value],
    want_static: Option<bool>,
) -> Result<Option<(u64, jdwp_client::reftype::MethodInfo)>, String> {
    let mut argtypes = Vec::with_capacity(args.len());
    for v in args {
        argtypes.push(arg_type(conn, v).await);
    }

    let mut current = Some(type_id);
    let mut guard = 0;
    let mut best: Option<(u32, u64, jdwp_client::reftype::MethodInfo)> = None;
    // Right arity, but plain scoring couldn't justify at least one parameter — an interface, a wrapper,
    // an array. Kept most-derived-first for the second pass, and only paid for if nothing scores.
    let mut unresolved: Vec<(u64, jdwp_client::reftype::MethodInfo)> = Vec::new();
    while let Some(tid) = current {
        guard += 1;
        if guard > 50 {
            break;
        }
        let methods = conn.get_methods(tid).await.map_err(|e| format!("Failed to get methods: {e}"))?;
        for m in methods {
            if m.name != name {
                continue;
            }
            if want_static.is_some_and(|want| want != (m.mod_bits & ACC_STATIC != 0)) {
                continue;
            }
            let params = sig_param_types(&m.signature);
            if params.len() != argtypes.len() {
                continue;
            }
            // `None` anywhere means at least one argument isn't plainly assignable to its parameter.
            let scored =
                params.iter().zip(&argtypes).try_fold(0u32, |acc, (p, a)| score_param(p, a).map(|s| acc + s));
            match scored {
                // Strictly-greater keeps the first (most derived) winner on a tie, so an override
                // in a subclass shadows the inherited method as Java would.
                Some(total) if best.as_ref().is_none_or(|(bs, ..)| total > *bs) => {
                    best = Some((total, tid, m));
                }
                Some(_) => {}
                None => unresolved.push((tid, m)),
            }
        }
        // A match at this level shadows anything inherited; stop before paying for more round-trips.
        if best.is_some() {
            break;
        }
        current = conn.get_superclass(tid).await.unwrap_or(None);
    }
    if let Some((_, t, m)) = best {
        return Ok(Some((t, m)));
    }
    Ok(resolve_unsettled(conn, unresolved, &argtypes).await)
}

/// Second-pass overload selection: for candidates plain scoring couldn't justify, put every unsettled
/// parameter to the JVM ([`assignable`]) and keep the best-scoring candidate that is fully assignable.
///
/// Separate from [`find_method_for_args`] because it is the expensive half — it can cost round trips per
/// parameter — and runs only when the cheap pass found nothing at all.
async fn resolve_unsettled(
    conn: &mut jdwp_client::JdwpConnection,
    candidates: Vec<(u64, jdwp_client::reftype::MethodInfo)>,
    argtypes: &[ArgType],
) -> Option<(u64, jdwp_client::reftype::MethodInfo)> {
    let mut resolved: Option<(u32, u64, jdwp_client::reftype::MethodInfo)> = None;
    for (tid, m) in candidates {
        let mut total = 0;
        let mut all_ok = true;
        for (p, a) in sig_param_types(&m.signature).iter().zip(argtypes) {
            let score = match score_param(p, a) {
                Some(s) => Some(s),
                None => assignable(conn, p, a).await,
            };
            if let Some(s) = score {
                total += s;
            } else {
                all_ok = false;
                break;
            }
        }
        // Strictly-greater keeps the first (most derived) candidate on a tie, as the first pass does.
        if all_ok && resolved.as_ref().is_none_or(|(bs, ..)| total > *bs) {
            resolved = Some((total, tid, m));
        }
    }
    resolved.map(|(_, t, m)| (t, m))
}

/// Spawn the per-session event pump: receive events off the connection (holding no lock while
/// waiting), then under the session lock arm deferred breakpoints, record trace/logpoint hits, or
/// store the latest reportable event. Bound to `sid`, not the "current" session.
fn spawn_event_listener(
    session_manager: SessionManager,
    sid: crate::session::SessionId,
    connection: jdwp_client::JdwpConnection,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            // Receive without holding any lock.
            let Some(event_set) = connection.recv_event().await else {
                break; // Connection closed
            };
            let Some(session_guard) = session_manager.get_session_by_id(&sid).await else {
                break; // Session gone
            };
            let mut session = session_guard.lock().await;
            if try_arm_deferred_breakpoints(&mut session, &event_set).await {
                continue;
            }
            if try_record_trace(&mut session, &event_set).await {
                continue;
            }
            store_reportable_event(&mut session, event_set).await;
        }
        info!("Event listener task stopped");
    })
}

/// A `ClassPrepare` event means a watched class just loaded. Arm any pending breakpoints for it
/// (before its code runs — the preparing thread is suspended by the `EventThread` policy), then
/// resume that one thread. Returns `true` if this was a class-prepare event (internal plumbing that
/// must never surface as `last_event`).
async fn try_arm_deferred_breakpoints(
    session: &mut crate::session::DebugSession,
    event_set: &jdwp_client::EventSet,
) -> bool {
    let Some((cp_thread, cp_ref, cp_sig)) = event_set.events.iter().find_map(|e| match &e.details {
        jdwp_client::events::EventKind::ClassPrepare { thread, ref_type, signature, .. } => {
            Some((*thread, *ref_type, signature.clone()))
        }
        _ => None,
    }) else {
        return false;
    };
    let pending: Vec<crate::session::PendingBreakpoint> =
        session.pending_breakpoints.iter().filter(|p| p.signature == cp_sig).cloned().collect();
    for pend in pending {
        match resolve_bp_location(&mut session.connection, cp_ref, pend.line, pend.method.as_deref()).await {
            Ok((method, index, line)) => {
                let sp = suspend_policy_for(pend.trace);
                match session
                    .connection
                    .set_breakpoint_ex(
                        cp_ref,
                        method.method_id,
                        index,
                        sp,
                        pend.hit_count,
                        pend.thread_filter,
                    )
                    .await
                {
                    Ok(req_id) => {
                        // Do the bookkeeping that only borrows `pend` first, so its owned fields can
                        // be moved (not cloned) into the stored BreakpointInfo below.
                        let _ = session.connection.clear_class_prepare(pend.class_prepare_request_id).await;
                        session.pending_breakpoints.retain(|p| p.bp_id != pend.bp_id);
                        info!(
                            "Armed deferred breakpoint {} on {} (line {})",
                            pend.bp_id, pend.class_pattern, line
                        );
                        session.breakpoints.insert(
                            pend.bp_id,
                            crate::session::BreakpointInfo {
                                request_id: Some(req_id),
                                class_pattern: pend.class_pattern,
                                line: u32::try_from(line).unwrap_or(0),
                                method: Some(method.name),
                                enabled: true,
                                hit_count: 0,
                                condition: pend.condition,
                                trace: pend.trace,
                                trace_expr: pend.trace_expr,
                                trace_budget: pend.trace_budget,
                                trace_frames: pend.trace_frames,
                                trace_cost: crate::session::TraceCost::default(),
                                arm: crate::session::BreakpointArm {
                                    class_id: cp_ref,
                                    method_id: method.method_id,
                                    bytecode_index: index,
                                    suspend_policy: sp,
                                    hit_count: pend.hit_count,
                                    thread_filter: pend.thread_filter,
                                },
                            },
                        );
                    }
                    Err(e) => warn!("Failed to arm deferred breakpoint {}: {}", pend.bp_id, e),
                }
            }
            Err(e) => warn!(
                "Deferred breakpoint {}: class {} loaded but location unresolved: {}",
                pend.bp_id, pend.class_pattern, e
            ),
        }
    }
    let _ = session.connection.resume_thread(cp_thread).await;
    true
}

/// What a traced (non-suspending) stop point needs at hit time, whichever kind registered it.
struct TracedRequest {
    /// The caller-facing id (`bp_`/`exc_`/`watch_`), used as the trace record's label.
    id: String,
    /// Only line breakpoints can carry one; an exception or field request has no condition.
    condition: Option<String>,
    trace_expr: Option<String>,
    /// How many caller frames to record above the hit (TRACE-5).
    trace_frames: usize,
    /// Only a method-exit request has one (METH-1): the method name the caller asked for, which has to
    /// be filtered on OUR side because JDWP's `ClassMatch` fires for every method of the class. A hit on
    /// a different method is dropped without recording it and without charging the budget.
    method_filter: Option<String>,
}

/// Find the traced stop point that a JDWP request id belongs to, across all three kinds.
///
/// One lookup, three maps — deliberately not a fourth map keyed by request id. Each kind already owns
/// its bookkeeping (and its `clear`/`panic` handling), so a parallel index would be a second source of
/// truth that could outlive an entry it points at. The maps are small enough that scanning is free.
fn find_traced_request(session: &crate::session::DebugSession, req_id: i32) -> Option<TracedRequest> {
    if let Some((id, b)) = session.breakpoints.iter().find(|(_, b)| b.request_id == Some(req_id) && b.trace) {
        return Some(TracedRequest {
            id: id.clone(),
            condition: b.condition.clone(),
            trace_expr: b.trace_expr.clone(),
            trace_frames: b.trace_frames,
            method_filter: None,
        });
    }
    if let Some((id, e)) =
        session.exception_requests.iter().find(|(_, e)| e.request_id == Some(req_id) && e.trace)
    {
        return Some(TracedRequest {
            id: id.clone(),
            condition: None,
            trace_expr: e.trace_expr.clone(),
            trace_frames: e.trace_frames,
            method_filter: None,
        });
    }
    if let Some((id, w)) = session.watchpoints.iter().find(|(_, w)| w.request_id == Some(req_id) && w.trace) {
        return Some(TracedRequest {
            id: id.clone(),
            condition: None,
            trace_expr: w.trace_expr.clone(),
            trace_frames: w.trace_frames,
            method_filter: None,
        });
    }
    if let Some((id, m)) = session.method_exits.iter().find(|(_, m)| m.request_id == Some(req_id) && m.trace)
    {
        return Some(TracedRequest {
            id: id.clone(),
            condition: None,
            trace_expr: m.trace_expr.clone(),
            trace_frames: m.trace_frames,
            method_filter: m.method.clone(),
        });
    }
    None
}

/// Refuse a `thread_id` that is already dead or was never valid on this connection (FILT-2).
///
/// Checked at ARM time, where the caller is looking, instead of letting the JVM answer
/// `INVALID_OBJECT (20)` — a bare protocol code that says nothing about the actual cause. And the cause is
/// almost always the same one: **thread ids are per-connection and do not survive a reattach**, so an id
/// copied from earlier notes, or from a previous session, is meaningless here.
async fn check_thread_filter(
    conn: &mut jdwp_client::JdwpConnection,
    thread_filter: Option<u64>,
) -> Result<(), String> {
    let Some(tid) = thread_filter else {
        return Ok(());
    };
    if thread_is_alive(conn, tid).await {
        return Ok(());
    }
    Err(format!(
        "🛑 thread_id 0x{tid:x} is not a live thread on this connection, so a stop point filtered to it \
         could never fire.\n   JDWP thread ids are **per-connection** and are not stable across a \
         reattach — an id from an earlier session, or from notes, will not work. A pooled request thread \
         can also simply have been retired since you read it.\n   Re-read debug.list_threads (or \
         debug.thread_dump) for a current id, then arm."
    ))
}

/// JDWP `threadStatus` for a thread that has finished. Its `Thread` **object** outlives it, so this is what
/// "dead" looks like from the wire — not an error.
const THREAD_STATUS_ZOMBIE: i32 = 0;

/// Whether a thread id still refers to a live thread (FILT-2).
///
/// Two distinct failures, and both matter:
/// - the request **errors** — the id was never valid on this connection (ids are per-connection, so one
///   copied from a previous session lands here);
/// - the request succeeds with `ZOMBIE` — the id was valid and the thread has since finished.
///
/// The second is the one that cost a debugging session: a retired pool worker is gone from `AllThreads`,
/// but the debugger still holds a reference to its `Thread` object, so `Status` answers perfectly happily.
/// A first version of this check tested only `is_ok()` and therefore never fired — caught by
/// `a_filter_pinned_to_a_retired_thread_reports_itself_as_dead`, which is why that test retires a real pool
/// rather than trusting a plausible-looking predicate.
async fn thread_is_alive(conn: &mut jdwp_client::JdwpConnection, tid: u64) -> bool {
    matches!(conn.get_thread_status(tid).await, Ok((status, _)) if status != THREAD_STATUS_ZOMBIE)
}

/// The `ThreadOnly` filter threads that have died, across every kind of stop point (FILT-2).
///
/// Checked once per **distinct** thread rather than once per stop point, since several stop points are
/// commonly filtered to the same request thread. Stop points with no filter cost nothing.
///
/// This exists because a filter pinned to a dead thread can never match again: the stop point reports
/// nothing and, before this, still listed itself as armed. On a pool that reaps idle workers — which is
/// exactly where FILT-1 recommends the filter — that silence read as "the bug didn't reproduce".
async fn dead_filter_threads(session: &mut crate::session::DebugSession) -> std::collections::BTreeSet<u64> {
    let mut filters: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    filters.extend(session.breakpoints.values().filter_map(|b| b.arm.thread_filter));
    filters.extend(session.exception_requests.values().filter_map(|e| e.thread_filter));
    filters.extend(session.watchpoints.values().filter_map(|w| w.thread_filter));
    filters.extend(session.method_exits.values().filter_map(|m| m.thread_filter));
    filters.extend(session.pending_breakpoints.iter().filter_map(|p| p.thread_filter));

    let mut dead = std::collections::BTreeSet::new();
    for tid in filters {
        if !thread_is_alive(&mut session.connection, tid).await {
            dead.insert(tid);
        }
    }
    dead
}

/// The ` ⚠️ FILTER THREAD 0x… IS GONE` marker for a stop point whose `ThreadOnly` thread has died.
///
/// Deliberately loud, and deliberately replaces nothing else on the line: the point is that a caller
/// scanning a listing for "is this working?" cannot miss it.
fn dead_filter_tag(thread_filter: Option<u64>, dead: &std::collections::BTreeSet<u64>) -> String {
    match thread_filter {
        Some(t) if dead.contains(&t) => format!(
            " ⚠️  FILTER THREAD 0x{t:x} IS GONE — this can never fire again; re-arm with a live thread_id"
        ),
        _ => String::new(),
    }
}

/// Whether a hit's location is in the method a request was narrowed to (METH-1).
///
/// `None` filter matches everything. Compared by **name only**, so every overload of `save` matches —
/// JDWP's `ClassMatch` gives us no signature to discriminate on, and a caller asking for `save` almost
/// certainly means all of them.
async fn method_name_matches(
    conn: &mut jdwp_client::JdwpConnection,
    filter: Option<&str>,
    loc: &Location,
) -> bool {
    let Some(want) = filter else {
        return true;
    };
    let (method, _, _) = frame_method_info(conn, loc, false).await;
    method == want
}

/// Disarm the one stop point a JDWP request id belongs to — line breakpoint, exception request, or
/// field watch — clearing its request in the JVM but **keeping its definition** so it can be re-armed
/// with `debug.toggle_stop_point`. Returns a human label for what was disarmed, or `None` if no tracked
/// stop point matched (e.g. a single-step, which the caller clears separately, or an already-disarmed
/// request).
///
/// Used by the watchdog to disarm exactly the stop point that froze the VM (SAFE-2) and by the
/// trace-budget path to auto-disarm (TRACE-3). Both are *automatic*, so deleting the entry would
/// silently destroy a condition or `trace_expr` the user typed by hand — the very setup SAFE-2's design
/// note said not to throw away. Disabling keeps it recoverable in one call (BP-2).
async fn disarm_request(session: &mut crate::session::DebugSession, req_id: i32) -> Option<String> {
    if let Some((id, bp)) = session
        .breakpoints
        .iter()
        .find(|(_, b)| b.request_id == Some(req_id))
        .map(|(k, v)| (k.clone(), v.clone()))
    {
        let _ = session.connection.clear_breakpoint(req_id).await;
        if let Some(b) = session.breakpoints.get_mut(&id) {
            b.request_id = None;
            b.enabled = false;
        }
        return Some(format!("breakpoint {id} at {}:{}", bp.class_pattern, bp.line));
    }
    if let Some((id, er)) = session
        .exception_requests
        .iter()
        .find(|(_, e)| e.request_id == Some(req_id))
        .map(|(k, v)| (k.clone(), v.clone()))
    {
        let _ = session.connection.clear_exception_request(req_id).await;
        if let Some(e) = session.exception_requests.get_mut(&id) {
            e.request_id = None;
            e.enabled = false;
        }
        return Some(format!("exception breakpoint {id} ({})", er.class_pattern));
    }
    if let Some((id, wp)) = session
        .watchpoints
        .iter()
        .find(|(_, w)| w.request_id == Some(req_id))
        .map(|(k, v)| (k.clone(), v.clone()))
    {
        let _ = session.connection.clear_field_watch(req_id, wp.kind).await;
        if let Some(w) = session.watchpoints.get_mut(&id) {
            w.request_id = None;
            w.enabled = false;
        }
        return Some(format!("watchpoint {id} ({}.{})", wp.class_name, wp.field_name));
    }
    if let Some((id, me)) = session
        .method_exits
        .iter()
        .find(|(_, m)| m.request_id == Some(req_id))
        .map(|(k, v)| (k.clone(), v.clone()))
    {
        let _ = session.connection.clear_method_exit_request(req_id, me.with_return_value).await;
        if let Some(m) = session.method_exits.get_mut(&id) {
            m.request_id = None;
            m.enabled = false;
        }
        return Some(format!(
            "method-exit request {id} ({}{})",
            me.class_pattern,
            me.method.map_or_else(|| ".*".to_string(), |m| format!(".{m}"))
        ));
    }
    None
}

/// Disable the stop point with this caller-facing id: clear its JDWP request, keep its definition.
/// Returns a short human description of what was disabled.
///
/// Shares its "keep the definition" behaviour with [`disarm_request`], which is the automatic path
/// (watchdog / trace budget); this is the explicit one, via `debug.toggle_stop_point`.
/// One disable per kind, mirroring how [`rearm_stop_point`] is split: each clears a different JDWP
/// request type, and inlining all four made this branchy enough to trip the complexity gate.
async fn disable_stop_point(session: &mut crate::session::DebugSession, id: &str) -> Result<String, String> {
    if let Some(bp) = session.breakpoints.get(id).cloned() {
        return disable_line_breakpoint(session, id, &bp).await;
    }
    if let Some(er) = session.exception_requests.get(id).cloned() {
        return disable_exception_request(session, id, &er).await;
    }
    if let Some(wp) = session.watchpoints.get(id).cloned() {
        return disable_watchpoint(session, id, &wp).await;
    }
    if let Some(me) = session.method_exits.get(id).cloned() {
        return disable_method_exit(session, id, &me).await;
    }
    Err(format!("Stop point not found: {id}"))
}

async fn disable_line_breakpoint(
    session: &mut crate::session::DebugSession,
    id: &str,
    bp: &crate::session::BreakpointInfo,
) -> Result<String, String> {
    if let Some(req) = bp.request_id {
        session
            .connection
            .clear_breakpoint(req)
            .await
            .map_err(|e| format!("Failed to clear breakpoint request: {e}"))?;
    }
    if let Some(b) = session.breakpoints.get_mut(id) {
        b.request_id = None;
        b.enabled = false;
    }
    Ok(format!("{}:{}", bp.class_pattern, bp.line))
}

async fn disable_exception_request(
    session: &mut crate::session::DebugSession,
    id: &str,
    er: &crate::session::ExceptionRequestInfo,
) -> Result<String, String> {
    if let Some(req) = er.request_id {
        session
            .connection
            .clear_exception_request(req)
            .await
            .map_err(|e| format!("Failed to clear exception request: {e}"))?;
    }
    if let Some(e) = session.exception_requests.get_mut(id) {
        e.request_id = None;
        e.enabled = false;
    }
    Ok(format!("exception {}", er.class_pattern))
}

async fn disable_watchpoint(
    session: &mut crate::session::DebugSession,
    id: &str,
    wp: &crate::session::WatchpointInfo,
) -> Result<String, String> {
    if let Some(req) = wp.request_id {
        session
            .connection
            .clear_field_watch(req, wp.kind)
            .await
            .map_err(|e| format!("Failed to clear field watch: {e}"))?;
    }
    if let Some(w) = session.watchpoints.get_mut(id) {
        w.request_id = None;
        w.enabled = false;
    }
    Ok(format!("watch {}.{}", wp.class_name, wp.field_name))
}

/// Disabling a method-exit request must pass back the same `with_return_value` it was armed with: JDWP
/// keys requests by (eventKind, requestID), so clearing kind 41 when 42 was armed leaves it live.
async fn disable_method_exit(
    session: &mut crate::session::DebugSession,
    id: &str,
    me: &crate::session::MethodExitRequestInfo,
) -> Result<String, String> {
    if let Some(req) = me.request_id {
        session
            .connection
            .clear_method_exit_request(req, me.with_return_value)
            .await
            .map_err(|e| format!("Failed to clear method-exit request: {e}"))?;
    }
    if let Some(m) = session.method_exits.get_mut(id) {
        m.request_id = None;
        m.enabled = false;
    }
    Ok(format!("method-exit {}", me.class_pattern))
}

/// Re-arm the disabled stop point with this caller-facing id from its stored definition, keeping the
/// same id (BP-3). Returns a short human description of what was re-armed.
///
/// The location is **re-resolved by name**, not taken from the ids captured when it was first armed
/// (BP-4). A `referenceTypeID`/`methodID`/`fieldID` is only valid while that type stays loaded, and the
/// realistic sequence here is "disable the breakpoint, redeploy, re-arm it" on a long-lived app server —
/// exactly when a cached id is stale and would fail obscurely or resolve somewhere unintended. A class
/// that is no longer loaded is reported as that, which is a state the caller needs to know about.
///
/// A re-armed stop point gets a fresh trace budget: it was disarmed *because* the old one ran out, so
/// re-arming with zero left would fire once and immediately disable itself again.
async fn rearm_stop_point(session: &mut crate::session::DebugSession, id: &str) -> Result<String, String> {
    // One arm per kind, each in its own function: the resolution steps differ (a location, a class, a
    // field) and inlining all three made this branchy enough to trip the complexity gate.
    if let Some(bp) = session.breakpoints.get(id).cloned() {
        return rearm_line_breakpoint(session, id, &bp).await;
    }
    if let Some(er) = session.exception_requests.get(id).cloned() {
        return rearm_exception_request(session, id, &er).await;
    }
    if let Some(wp) = session.watchpoints.get(id).cloned() {
        return rearm_watchpoint(session, id, &wp).await;
    }
    if let Some(me) = session.method_exits.get(id).cloned() {
        return rearm_method_exit(session, id, &me).await;
    }
    Err(format!("Stop point not found: {id}"))
}

/// Re-arm a method-exit request (METH-1).
///
/// Nothing to re-resolve by name, unlike the other three: a `ClassMatch` modifier is the *pattern
/// string*, matched by the JVM as classes load, not a reference type id captured at arm time. So this is
/// the one kind that is immune to the BP-4 staleness problem — and it re-arms across a redeploy without
/// needing the class to be loaded at all.
async fn rearm_method_exit(
    session: &mut crate::session::DebugSession,
    id: &str,
    me: &crate::session::MethodExitRequestInfo,
) -> Result<String, String> {
    let req = session
        .connection
        .set_method_exit_request(
            &me.class_pattern,
            me.with_return_value,
            suspend_policy_for(me.trace),
            None,
            me.thread_filter,
        )
        .await
        .map_err(|e| format!("Failed to re-arm method-exit request: {e}"))?;
    if let Some(m) = session.method_exits.get_mut(id) {
        m.request_id = Some(req);
        m.enabled = true;
        m.trace_budget = refreshed_budget(m.trace_budget);
        reset_trace_cost(&mut m.trace_cost);
    }
    Ok(format!("method-exit {}", me.class_pattern))
}

/// A re-armed traced stop point starts its cost observation from scratch (TRACE-7).
///
/// The alternative — carrying the old figures over — reports an arrival rate diluted by however long the
/// stop point sat disabled, since that gap falls inside the observation window while producing no hits. A
/// self-disarmed logpoint that is re-armed minutes later would look far quieter than the site it is on.
/// The measurement describes the current arming, the same way the budget does.
fn reset_trace_cost(cost: &mut crate::session::TraceCost) {
    *cost = crate::session::TraceCost::default();
}

/// A re-armed stop point's trace budget, refreshed: it was disarmed *because* the old one ran out, so
/// re-arming with zero left would fire once and immediately disable itself again.
const fn refreshed_budget(current: Option<u32>) -> Option<u32> {
    match current {
        Some(0) => Some(DEFAULT_TRACE_BUDGET),
        other => other,
    }
}

/// Re-arm a line breakpoint, re-resolving its location by name first (BP-4).
async fn rearm_line_breakpoint(
    session: &mut crate::session::DebugSession,
    id: &str,
    bp: &crate::session::BreakpointInfo,
) -> Result<String, String> {
    let arm = rearm_breakpoint_location(session, bp).await?;
    let req = session
        .connection
        .set_breakpoint_ex(
            arm.class_id,
            arm.method_id,
            arm.bytecode_index,
            arm.suspend_policy,
            arm.hit_count,
            arm.thread_filter,
        )
        .await
        .map_err(|e| format!("Failed to re-arm breakpoint: {e}"))?;
    if let Some(b) = session.breakpoints.get_mut(id) {
        b.request_id = Some(req);
        b.enabled = true;
        b.arm = arm;
        b.trace_budget = refreshed_budget(b.trace_budget);
        reset_trace_cost(&mut b.trace_cost);
    }
    Ok(format!("{}:{}", bp.class_pattern, bp.line))
}

/// Re-arm an exception breakpoint, re-resolving its exception class by name first (BP-4).
async fn rearm_exception_request(
    session: &mut crate::session::DebugSession,
    id: &str,
    er: &crate::session::ExceptionRequestInfo,
) -> Result<String, String> {
    // "*" means "every exception", which was registered with no ref type at all — nothing to resolve.
    let ref_type = if er.class_pattern == "*" {
        None
    } else {
        Some(resolve_class_by_dotted(&mut session.connection, &er.class_pattern).await?.ok_or_else(|| {
            format!(
                "Cannot re-arm {id}: exception class '{}' is not loaded any more (was it redeployed? \
                 trigger it once so the JVM loads it, then retry)",
                er.class_pattern
            )
        })?)
    };
    let req = session
        .connection
        .set_exception_request_ex(
            ref_type,
            er.caught,
            er.uncaught,
            suspend_policy_for(er.trace),
            None,
            er.thread_filter,
        )
        .await
        .map_err(|e| format!("Failed to re-arm exception breakpoint: {e}"))?;
    if let Some(e) = session.exception_requests.get_mut(id) {
        e.request_id = Some(req);
        e.enabled = true;
        e.ref_type = ref_type;
        e.trace_budget = refreshed_budget(e.trace_budget);
        reset_trace_cost(&mut e.trace_cost);
    }
    Ok(format!("exception {}", er.class_pattern))
}

/// Re-arm a field watchpoint, re-resolving its declaring type and field by name first (BP-4).
async fn rearm_watchpoint(
    session: &mut crate::session::DebugSession,
    id: &str,
    wp: &crate::session::WatchpointInfo,
) -> Result<String, String> {
    let type_id =
        resolve_class_by_dotted(&mut session.connection, &wp.class_name).await?.ok_or_else(|| {
            format!(
                "Cannot re-arm {id}: class '{}' is not loaded any more (was it redeployed? exercise it \
             once so the JVM loads it, then retry)",
                wp.class_name
            )
        })?;
    let (declaring, field) =
        find_field_info(&mut session.connection, type_id, &wp.field_name, None).await?.ok_or_else(|| {
            format!("Cannot re-arm {id}: class '{}' no longer has a field '{}'", wp.class_name, wp.field_name)
        })?;
    let req = session
        .connection
        .set_field_watch_ex(
            declaring,
            field.field_id,
            wp.kind,
            suspend_policy_for(wp.trace),
            None,
            wp.thread_filter,
        )
        .await
        .map_err(|e| format!("Failed to re-arm watchpoint: {e}"))?;
    if let Some(w) = session.watchpoints.get_mut(id) {
        w.request_id = Some(req);
        w.enabled = true;
        w.arm = (declaring, field.field_id);
        w.trace_budget = refreshed_budget(w.trace_budget);
        reset_trace_cost(&mut w.trace_cost);
    }
    Ok(format!("watch {}.{}", wp.class_name, wp.field_name))
}

/// Re-resolve a breakpoint's location from its class pattern and line/method (BP-4), returning fresh
/// JDWP ids. Falls back to the stored ids only when the class *is* still loaded but the line can't be
/// resolved, which keeps a working breakpoint working if a line table shifted underneath us.
async fn rearm_breakpoint_location(
    session: &mut crate::session::DebugSession,
    bp: &crate::session::BreakpointInfo,
) -> Result<crate::session::BreakpointArm, String> {
    let signature = format!("L{};", bp.class_pattern.replace('.', "/"));
    let classes = session
        .connection
        .classes_by_signature(&signature)
        .await
        .map_err(|e| format!("Failed to look up '{}': {e}", bp.class_pattern))?;
    let Some(class) = classes.first() else {
        return Err(format!(
            "Cannot re-arm: class '{}' is not loaded any more (was it redeployed? trigger it once so \
             the JVM loads it, then retry — or set a fresh breakpoint, which defers until it loads)",
            bp.class_pattern
        ));
    };
    let line_opt = i32::try_from(bp.line).ok();
    match resolve_bp_location(&mut session.connection, class.type_id, line_opt, bp.method.as_deref()).await {
        Ok((method, index, _line)) => Ok(crate::session::BreakpointArm {
            class_id: class.type_id,
            method_id: method.method_id,
            bytecode_index: index,
            ..bp.arm.clone()
        }),
        // The class is loaded but the location didn't resolve; the old ids are the best guess left, and
        // they are valid as long as the type wasn't reloaded.
        Err(_) => Ok(bp.arm.clone()),
    }
}

/// A hit on a stop point marked `trace` — a line breakpoint, an exception breakpoint, or a field
/// watchpoint — suspended only the hit thread (`EventThread` policy). Snapshot it into the ring
/// buffer and resume THAT thread immediately, never surfacing it as an event. Returns `true` if a
/// traced request was matched and handled.
async fn try_record_trace(
    session: &mut crate::session::DebugSession,
    event_set: &jdwp_client::EventSet,
) -> bool {
    let (Some((thread, loc)), Some((req_id, details))) = (
        event_set.events.first().and_then(|e| event_location(&e.details)),
        event_set.events.first().map(|e| (e.request_id, e.details.clone())),
    ) else {
        return false;
    };
    let Some(req) = find_traced_request(session, req_id) else {
        return false;
    };
    // Two reasons to drop a hit without recording it, and neither charges the trace budget — so
    // "exactly N traces, then it stops" still holds:
    //  - a line breakpoint's `condition` isn't true;
    //  - a method-exit request fired for a method other than the one asked for (METH-1), which is the
    //    common case, since JDWP's ClassMatch reports every method of the class.
    let wrong_method =
        !method_name_matches(&mut session.connection, req.method_filter.as_deref(), &loc).await;
    let skip = wrong_method
        || match &req.condition {
            Some(cond) => !evaluate_condition_on_thread(&mut session.connection, thread, cond).await,
            None => false,
        };
    // TRACE-7: time the capture and nothing else. The condition evaluation above, the resume below and
    // the budget arithmetic after it are all ours, and charging them to "what a traced hit costs" would
    // report our own bookkeeping as the debuggee's price — the same reason #17 measured the dump's
    // suspend/resume pair rather than the whole call.
    let started = std::time::Instant::now();
    let record = if skip {
        None
    } else {
        Some(
            capture_trace(
                &mut session.connection,
                &req.id,
                req.trace_expr.as_deref(),
                req.trace_frames,
                thread,
                &loc,
                &details,
            )
            .await,
        )
    };
    let took = started.elapsed();
    let recorded = record.is_some();
    if recorded {
        record_trace_cost(session, req_id, started, took);
    }
    if let Some(mut rec) = record {
        session.trace_seq += 1;
        rec.seq = session.trace_seq;
        if session.traces.len() >= crate::session::MAX_TRACES {
            session.traces.pop_front();
        }
        session.traces.push_back(rec);
    }
    let _ = session.connection.resume_thread(thread).await;
    // TRACE-3: charge the hit against this stop point's budget and disarm it once it runs out, so a
    // hot throw/field can't keep flooding the debuggee. Only a recorded hit is charged, so the
    // "exactly N traces, then it stops" contract holds even when a condition skips some.
    if recorded {
        if let Some(label) = charge_trace_budget(session, req_id).await {
            session.note_trace_disarm(label);
        }
    }
    true
}

/// Charge one hit against a traced stop point's budget (TRACE-3). When the budget reaches zero, disarm
/// the request and return a note for `get_traces`; otherwise decrement in place and return `None`. A
/// stop point with no budget (`None`) is unbounded and is never charged.
async fn charge_trace_budget(session: &mut crate::session::DebugSession, req_id: i32) -> Option<String> {
    let remaining = decrement_trace_budget(session, req_id)?;
    if remaining == 0 {
        let what = disarm_request(session, req_id).await?;
        Some(format!(
            "{what} stopped recording — reached its trace-hit budget and disarmed itself. Re-arm with a higher trace_max_hits if you need more."
        ))
    } else {
        None
    }
}

/// Record one capture's cost against whichever traced stop point owns `req_id` (TRACE-7).
///
/// Four maps scanned in the same order as [`decrement_trace_budget`], and for the same reason: each kind
/// owns its own bookkeeping, and a parallel index keyed by request id would be a second source of truth
/// that could outlive the entry it points at.
fn record_trace_cost(
    session: &mut crate::session::DebugSession,
    req_id: i32,
    started: std::time::Instant,
    took: std::time::Duration,
) {
    if let Some(b) = session.breakpoints.values_mut().find(|b| b.request_id == Some(req_id)) {
        b.trace_cost.record(started, took);
    } else if let Some(e) = session.exception_requests.values_mut().find(|e| e.request_id == Some(req_id)) {
        e.trace_cost.record(started, took);
    } else if let Some(w) = session.watchpoints.values_mut().find(|w| w.request_id == Some(req_id)) {
        w.trace_cost.record(started, took);
    } else if let Some(m) = session.method_exits.values_mut().find(|m| m.request_id == Some(req_id)) {
        m.trace_cost.record(started, took);
    }
}

/// Decrement the matching stop point's trace budget in place, returning the count left afterwards, or
/// `None` when the request has no budget (unbounded) or isn't found.
fn decrement_trace_budget(session: &mut crate::session::DebugSession, req_id: i32) -> Option<u32> {
    if let Some(b) = session.breakpoints.values_mut().find(|b| b.request_id == Some(req_id)) {
        let n = b.trace_budget?.saturating_sub(1);
        b.trace_budget = Some(n);
        return Some(n);
    }
    if let Some(e) = session.exception_requests.values_mut().find(|e| e.request_id == Some(req_id)) {
        let n = e.trace_budget?.saturating_sub(1);
        e.trace_budget = Some(n);
        return Some(n);
    }
    if let Some(w) = session.watchpoints.values_mut().find(|w| w.request_id == Some(req_id)) {
        let n = w.trace_budget?.saturating_sub(1);
        w.trace_budget = Some(n);
        return Some(n);
    }
    if let Some(m) = session.method_exits.values_mut().find(|m| m.request_id == Some(req_id)) {
        let n = m.trace_budget?.saturating_sub(1);
        m.trace_budget = Some(n);
        return Some(n);
    }
    None
}

/// Evaluate a conditional breakpoint on the hit thread and auto-resume (without reporting) when the
/// condition is not true; otherwise record the suspension and store the event for the caller.
async fn store_reportable_event(
    session: &mut crate::session::DebugSession,
    event_set: jdwp_client::EventSet,
) {
    let mut skip = false;
    if let (Some((thread, loc)), Some(req_id)) = (
        event_set.events.first().and_then(|e| event_location(&e.details)),
        event_set.events.first().map(|e| e.request_id),
    ) {
        // A suspending method-exit request narrowed to one method (METH-1) still receives every method of
        // the class, so an exit from a different one must resume and be dropped — otherwise a request for
        // `save` freezes the VM on the first unrelated getter that returns.
        let method_filter = session
            .method_exits
            .values()
            .find(|m| m.request_id == Some(req_id))
            .and_then(|m| m.method.clone());
        if method_filter.is_some()
            && !method_name_matches(&mut session.connection, method_filter.as_deref(), &loc).await
        {
            let _ = session.connection.resume_all().await;
            skip = true;
        }
        let cond = session
            .breakpoints
            .values()
            .find(|b| b.request_id == Some(req_id))
            .and_then(|b| b.condition.clone());
        if !skip {
            if let Some(cond) = cond {
                if !evaluate_condition_on_thread(&mut session.connection, thread, &cond).await {
                    let _ = session.connection.resume_all().await;
                    skip = true;
                }
            }
        }
    }
    if !skip {
        if let Some(tid) = event_thread(&event_set) {
            session.last_thread = Some(tid);
        }
        let suspends = event_suspends(&event_set);
        if suspends {
            // Record WHICH request suspended us, here and now. The watchdog used to re-derive this from
            // the newest buffered event, which `get_last_event {drain:true}` erases (SAFE-5).
            let cause = event_set.events.first().map_or(crate::session::SuspendCause::ManualPause, |e| {
                crate::session::SuspendCause::StopPoint(e.request_id)
            });
            session.mark_suspended(cause);
        }
        let seq = session.push_event(event_set);
        // Buffer first, then push. The buffer is the authoritative record and must be written whether
        // or not anyone is listening; the notification is a hint that one exists (EVT-2).
        if suspends {
            notify_suspension(session, seq).await;
        }
    }
}

/// Push a `notifications/message` for a hit that has just frozen the debuggee (EVT-2).
///
/// **Suspending hits only.** A `trace:true` stop point does not stop the VM and is built to fire at
/// hundreds of hits per second — notifying per hit would flood the transport and defeat the one mode
/// that is safe on the shared 8180. Snapshots stay where they belong, behind `debug.get_traces`.
///
/// The payload is built with the same `describe_event_into` the polled path uses, so a caller acting
/// on the notification alone sees exactly what `debug.get_last_event` would have told them. That
/// equivalence is what makes skipping the round trip safe rather than merely quicker.
///
/// Cost: the VM is already frozen by the time this runs, and the location lookups hit the type and
/// line-table caches, so this adds nothing the debuggee was not paying already.
async fn notify_suspension(session: &mut crate::session::DebugSession, seq: u64) {
    let alerter = session.alerter.clone();
    let Some(rec) = session.events.back().cloned() else { return };
    let Some(ev) = rec.set.events.first() else { return };

    let mut obj = serde_json::Map::new();
    obj.insert("seq".to_string(), json!(seq));
    obj.insert("event".to_string(), json!(event_type_name(&ev.details)));
    describe_event_into(&mut session.connection, &ev.details, &mut obj).await;
    // The fact that separates this from a trace snapshot, and the reason it is worth interrupting the
    // caller for at all: the VM is stopped, other people's requests are stalled behind it, and the
    // watchdog clock is now running.
    obj.insert("suspended".to_string(), json!(true));
    if let Some(id) = stop_point_id(session, ev.request_id) {
        obj.insert("stopPoint".to_string(), json!(id));
    }
    // `warning`, not `info`: on a shared instance a freeze is something to act on, and a client
    // filtering its log level should not have this fall below the line.
    alerter.alert("warning", &serde_json::Value::Object(obj));
}

/// The caller-facing stop-point id behind a JDWP request id, across all four kinds (BP-3's ids).
///
/// Pure in-memory lookup over the session's own maps — no JDWP traffic — which is what makes it safe
/// to call on the hit path while the VM is held.
fn stop_point_id(session: &crate::session::DebugSession, req: i32) -> Option<String> {
    let hit = Some(req);
    session
        .breakpoints
        .iter()
        .find(|(_, b)| b.request_id == hit)
        .map(|(k, _)| k.clone())
        .or_else(|| {
            session.exception_requests.iter().find(|(_, e)| e.request_id == hit).map(|(k, _)| k.clone())
        })
        .or_else(|| session.watchpoints.iter().find(|(_, w)| w.request_id == hit).map(|(k, _)| k.clone()))
        .or_else(|| session.method_exits.iter().find(|(_, m)| m.request_id == hit).map(|(k, _)| k.clone()))
}

/// Upper bound on resume attempts when clearing a suspend depth (SAFE-7). A depth above this means
/// something is suspending in a loop, which is worth reporting rather than spinning on.
const MAX_RESUME_ATTEMPTS: u32 = 8;

/// Resume the VM and **verify it is actually running**, clearing a counted suspend depth (SAFE-7).
///
/// Returns `Ok(None)` when the VM is genuinely going again, or `Ok(Some(note))` describing what is still
/// holding it — so a caller can report the truth instead of assuming one `resume_all` was enough.
///
/// JDWP counts suspends, so `pause`-twice (or `pause` while stopped at a breakpoint) needs two resumes.
/// Verified on a real JVM: two suspends then one resume leaves the debuggee stopped while every command
/// reports OK. A watchdog that trusted that reported a rescue it had not performed.
///
/// Falls back to a single plain `resume_all` when there is no thread to probe — nothing is known to be
/// suspended in that case, so there is no depth to clear.
async fn resume_and_verify(session: &mut crate::session::DebugSession) -> Result<Option<String>, String> {
    let Some(probe) = session.last_thread else {
        session.connection.resume_all().await.map_err(|e| format!("Failed to resume: {e}"))?;
        return Ok(None);
    };
    let (issued, left) = session
        .connection
        .resume_all_fully(probe, MAX_RESUME_ATTEMPTS)
        .await
        .map_err(|e| format!("Failed to resume: {e}"))?;
    if left > 0 {
        return Ok(Some(format!(
            "the VM is STILL suspended after {issued} resume(s) — thread 0x{probe:x} has {left} \
             suspend(s) left. Something is holding it beyond this session; call debug.continue again, or \
             debug.panic"
        )));
    }
    // Worth saying when it took more than one: it means the suspends had stacked up.
    Ok((issued > 1).then(|| format!("cleared a suspend depth of {issued}")))
}

/// How long the VM may sit suspended before the watchdog resumes it: `JDWP_WATCHDOG_SECS`, default 120,
/// `0` to disable. Read in one place so the tools can *report* the value they're promising.
fn watchdog_secs() -> u64 {
    std::env::var("JDWP_WATCHDOG_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(120)
}

/// Spawn the watchdog: auto-resume the VM if anything leaves it suspended past `JDWP_WATCHDOG_SECS`
/// (default 120; `0` disables), so a forgotten breakpoint — or a forgotten `debug.pause` — can't freeze
/// a request thread on a shared instance.
fn spawn_watchdog(
    session_manager: SessionManager,
    sid: crate::session::SessionId,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let secs = watchdog_secs();
        if secs == 0 {
            return;
        }
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let Some(g) = session_manager.get_session_by_id(&sid).await else {
                break;
            };
            let mut s = g.lock().await;
            if let Some(since) = s.suspended_since {
                if since.elapsed().as_secs() >= secs {
                    // A pending single-step must be cleared before the resume, or the next resume
                    // re-fires it.
                    if let Some(req) = s.pending_step.take() {
                        let _ = s.connection.clear_step(req).await;
                    }
                    // Disarm whatever caused the suspension rather than only resuming — otherwise the
                    // cycle is freeze → 120s → resume → freeze again on the very next hit, indefinitely
                    // (SAFE-2). The cause was recorded when the VM suspended, so draining the event
                    // buffer can no longer hide it (SAFE-5), and a manual pause — which has no stop
                    // point to disarm — is reported as itself rather than as a failure (SAFE-4).
                    let disarmed = match s.suspended_cause {
                        Some(crate::session::SuspendCause::ManualPause) =>
                            "suspended by debug.pause (a manual pause — no stop point to disarm)".to_string(),
                        Some(crate::session::SuspendCause::StopPoint(req)) => {
                            disarm_request(&mut s, req).await.map_or_else(
                                || "(its stop point was already cleared, so there was nothing left to disarm)".to_string(),
                                |what| format!(
                                    "and disabled {what} so it can't re-freeze the VM — re-arm it with debug.toggle_stop_point (or use trace:true) when ready"
                                ),
                            )
                        }
                        None => "(cause unrecorded)".to_string(),
                    };

                    // Resume for REAL: a counted suspend depth (e.g. a pause on top of a breakpoint)
                    // needs more than one resume, and reporting a rescue that didn't happen — then
                    // clearing `suspended_since` so we never retry — is the worst thing this task can
                    // do (SAFE-7). On failure, leave `suspended_since` set so the next tick tries again.
                    // EVT-2: every arm below sets `last_watchdog_note`, and every one of them is news
                    // the caller cannot discover by asking — the VM they left suspended is no longer
                    // suspended, and a stop point they armed is now disabled. Pushed as well as
                    // recorded, so a caller who walked away is told rather than finding out later.
                    // Each arm stores the note and yields only its severity. Carrying the text back out
                    // too would mean cloning a String on every watchdog tick for no gain — the note is
                    // already on the session, and that copy is the one to alert from.
                    let level = match resume_and_verify(&mut s).await {
                        Ok(None) => {
                            s.mark_resumed();
                            let note = format!("watchdog auto-resumed the VM after {secs}s {disarmed}");
                            info!("{note}");
                            s.last_watchdog_note = Some(note);
                            "warning"
                        }
                        Ok(Some(detail)) if detail.starts_with("cleared") => {
                            s.mark_resumed();
                            let note =
                                format!("watchdog auto-resumed the VM after {secs}s {disarmed} ({detail})");
                            info!("{note}");
                            s.last_watchdog_note = Some(note);
                            "warning"
                        }
                        Ok(Some(problem)) => {
                            // Deliberately NOT calling mark_resumed: the VM is still stopped, so the
                            // watchdog must keep trying rather than going quiet on a false success.
                            let note = format!(
                                "⚠️ watchdog tried to resume the VM after {secs}s {disarmed}, but {problem}"
                            );
                            warn!("{note}");
                            s.last_watchdog_note = Some(note);
                            // A still-frozen VM is an `error`: nothing the caller does next will work
                            // until it runs, which is a different thing from "we rescued it for you".
                            "error"
                        }
                        Err(e) => {
                            let note = format!("⚠️ watchdog could not resume the VM after {secs}s: {e}");
                            warn!("{note}");
                            s.last_watchdog_note = Some(note);
                            "error"
                        }
                    };
                    if let Some(note) = &s.last_watchdog_note {
                        s.alerter.alert(level, &json!({ "watchdog": note }));
                    }
                    drop(s);
                }
            }
        }
    })
}

/// Everything needed to arm one breakpoint, resolved once from the tool arguments.
struct BreakpointSpec {
    class_pattern: String,
    signature: String,
    line_opt: Option<i32>,
    method_hint: Option<String>,
    hit_count: Option<i32>,
    thread_filter: Option<u64>,
    condition: Option<String>,
    trace: bool,
    trace_expr: Option<String>,
    trace_budget: Option<u32>,
    /// Caller-frame depth for traced hits (TRACE-5), already clamped to `MAX_TRACE_FRAMES`.
    trace_frames: usize,
    suspend_policy: jdwp_client::SuspendPolicy,
}

/// Resolve the location on a loaded class, set the JDWP breakpoint, and record it in the session under
/// the caller-facing `bp_id` (allocated by the caller so it survives a later disable/re-arm — BP-3).
/// Returns `(bp_id, resolved source line, method name, JDWP request id)`.
async fn arm_and_insert(
    session: &mut crate::session::DebugSession,
    class_type_id: u64,
    spec: &BreakpointSpec,
    bp_id: String,
) -> Result<(String, i32, String, i32), String> {
    let (method, index, line) = resolve_bp_location(
        &mut session.connection,
        class_type_id,
        spec.line_opt,
        spec.method_hint.as_deref(),
    )
    .await
    .map_err(|e| format!("{e} in {}", spec.class_pattern))?;
    let request_id = session
        .connection
        .set_breakpoint_ex(
            class_type_id,
            method.method_id,
            index,
            spec.suspend_policy,
            spec.hit_count,
            spec.thread_filter,
        )
        .await
        .map_err(|e| format!("Failed to set breakpoint: {e}"))?;
    session.breakpoints.insert(
        bp_id.clone(),
        crate::session::BreakpointInfo {
            request_id: Some(request_id),
            class_pattern: spec.class_pattern.clone(),
            line: u32::try_from(line).unwrap_or(0),
            method: Some(method.name.clone()),
            enabled: true,
            hit_count: 0,
            condition: spec.condition.clone(),
            trace: spec.trace,
            trace_expr: spec.trace_expr.clone(),
            trace_budget: spec.trace_budget,
            trace_frames: spec.trace_frames,
            trace_cost: crate::session::TraceCost::default(),
            arm: crate::session::BreakpointArm {
                class_id: class_type_id,
                method_id: method.method_id,
                bytecode_index: index,
                suspend_policy: spec.suspend_policy,
                hit_count: spec.hit_count,
                thread_filter: spec.thread_filter,
            },
        },
    );
    Ok((bp_id, line, method.name, request_id))
}

/// The target class isn't loaded yet: register a `CLASS_PREPARE` watch (`EventThread` suspend, so the
/// real breakpoint can be armed before any of the class's code runs) and stash the spec; the event
/// pump arms it when the class loads. Closes the load race by re-checking once the watch is in
/// place, arming immediately if the class appeared in between.
async fn register_deferred_breakpoint(
    session: &mut crate::session::DebugSession,
    spec: &BreakpointSpec,
    bp_id: String,
) -> Result<String, String> {
    let cp_req = session
        .connection
        .set_class_prepare(&spec.class_pattern, jdwp_client::SuspendPolicy::EventThread)
        .await
        .map_err(|e| format!("Failed to register class-prepare watch: {e}"))?;

    let recheck = session.connection.classes_by_signature(&spec.signature).await.unwrap_or_default();
    if let Some(c) = recheck.first() {
        let ctid = c.type_id;
        let _ = session.connection.clear_class_prepare(cp_req).await;
        let (bp_id, line, method_name, _req) = arm_and_insert(session, ctid, spec, bp_id).await?;
        return Ok(format!(
            "✅ {} set at {}:{} (class had just loaded)\n   Method: {}\n   Stop-point ID: {}",
            if spec.trace { "Trace breakpoint" } else { "Breakpoint" },
            spec.class_pattern,
            line,
            method_name,
            bp_id
        ));
    }

    session.pending_breakpoints.push(crate::session::PendingBreakpoint {
        bp_id: bp_id.clone(),
        class_prepare_request_id: cp_req,
        class_pattern: spec.class_pattern.clone(),
        signature: spec.signature.clone(),
        line: spec.line_opt,
        method: spec.method_hint.clone(),
        hit_count: spec.hit_count,
        thread_filter: spec.thread_filter,
        condition: spec.condition.clone(),
        trace: spec.trace,
        trace_expr: spec.trace_expr.clone(),
        trace_budget: spec.trace_budget,
        trace_frames: spec.trace_frames,
    });
    let where_ = match (spec.line_opt, spec.method_hint.as_deref()) {
        (Some(l), _) => format!("line {l}"),
        (None, Some(m)) => format!("method {m}"),
        _ => String::new(),
    };
    Ok(format!(
        "⏳ Deferred breakpoint for {0} ({where_}) — {0} is not loaded yet. It will arm automatically when the class loads (trigger the request that loads it), then hit normally.\n   Stop-point ID: {bp_id}",
        spec.class_pattern
    ))
}

/// Resolve a breakpoint location (method, bytecode index, source line) on an already-loaded class,
/// by explicit line, by method name (first executable line), or a named method containing the line.
/// Shared by the immediate path and the deferred (class-prepare) arming path.
async fn resolve_bp_location(
    conn: &mut jdwp_client::JdwpConnection,
    class_type_id: u64,
    line_opt: Option<i32>,
    method_hint: Option<&str>,
) -> Result<(jdwp_client::reftype::MethodInfo, u64, i32), String> {
    let methods = conn.get_methods(class_type_id).await.map_err(|e| format!("Failed to get methods: {e}"))?;
    // Hold a reference to the winning method and clone it once after the loop, rather than cloning
    // on every candidate.
    let mut chosen: Option<(&jdwp_client::reftype::MethodInfo, u64, i32)> = None;
    for method in &methods {
        if let Some(hint) = method_hint {
            if method.name != hint {
                continue;
            }
        }
        let Ok(line_table) = conn.get_line_table(class_type_id, method.method_id).await else {
            continue;
        };
        if let Some(want) = line_opt {
            if let Some(e) = line_table.lines.iter().find(|e| e.line_number == want) {
                chosen = Some((method, e.line_code_index, want));
                break;
            }
            if method_hint.is_some() {
                if let Some(e) = line_table.lines.iter().min_by_key(|e| e.line_code_index) {
                    chosen = Some((method, e.line_code_index, e.line_number));
                    break;
                }
            }
        } else if let Some(e) = line_table.lines.iter().min_by_key(|e| e.line_code_index) {
            chosen = Some((method, e.line_code_index, e.line_number));
            break;
        }
    }
    match chosen {
        Some((method, code_index, line)) => Ok((method.clone(), code_index, line)),
        None => Err(line_opt.map_or_else(
            || format!("Method '{}' not found", method_hint.unwrap_or("")),
            |l| format!("No method contains line {l}"),
        )),
    }
}

/// Find a field (with its id + JNI signature) by name, walking the superclass chain. `want_static`:
/// `Some(true)` = static only, `Some(false)` = instance only, `None` = either. Returns the full
/// `FieldInfo` so the caller can coerce/validate the value against the field's declared type.
/// Find a field by name, walking the superclass chain. Returns the type that *declares* it together
/// with its info — the declaring type is what JDWP's `FieldOnly` watch modifier requires, and it may
/// be a superclass of `type_id`.
async fn find_field_info(
    conn: &mut jdwp_client::JdwpConnection,
    type_id: u64,
    name: &str,
    want_static: Option<bool>,
) -> Result<Option<(u64, jdwp_client::reftype::FieldInfo)>, String> {
    let mut current = Some(type_id);
    let mut guard = 0;
    while let Some(tid) = current {
        guard += 1;
        if guard > 50 {
            break;
        }
        let fields = conn.get_fields(tid).await.map_err(|e| format!("Failed to get fields: {e}"))?;
        if let Some(f) = fields.into_iter().find(|f| {
            f.name == name
                && match want_static {
                    Some(true) => (f.mod_bits & ACC_STATIC) != 0,
                    Some(false) => (f.mod_bits & ACC_STATIC) == 0,
                    None => true,
                }
        }) {
            return Ok(Some((tid, f)));
        }
        current = conn.get_superclass(tid).await.unwrap_or(None);
    }
    Ok(None)
}

/// Clear error when a literal can't be assigned to a field/variable's declared type.
fn type_mismatch_err(name: &str, field_sig: &str, value: &jdwp_client::types::Value) -> String {
    format!(
        "Type mismatch: '{}' is declared {}, but the value {} is not assignable — pass a compatible literal.",
        name,
        decode_signature(field_sig),
        value.format()
    )
}

/// Outcome of a field-write attempt: `Done` carries the success message; `Fallthrough` means this
/// strategy didn't apply, carrying the optional reason for the caller's final error message.
enum FieldWrite {
    Done(String),
    Fallthrough(Option<String>),
}

/// Assign to a bare local variable in a suspended frame (the single-segment `debug.set_value` path).
async fn set_local_variable(
    conn: &mut jdwp_client::JdwpConnection,
    thread_opt: Option<u64>,
    frame_index: usize,
    seg: &Seg,
    value_str: &str,
) -> Result<String, String> {
    if seg.args.is_some() {
        return Err("Cannot assign to a method call".to_string());
    }
    let name = &seg.name;
    let thread_id =
        thread_opt.ok_or_else(|| "No thread. Pass thread_id, or hit a breakpoint first.".to_string())?;
    let frames = conn.get_frames(thread_id, 0, -1).await.map_err(|e| format!("Failed to get frames: {e}"))?;
    let frame =
        frames.get(frame_index).cloned().ok_or_else(|| format!("frame_index {frame_index} out of range"))?;
    let vars = conn
        .get_variable_table(frame.location.class_id, frame.location.method_id)
        .await
        .map_err(|e| format!("Failed to read variable table: {e}"))?;
    let idx = frame.location.index;
    let var = vars
        .iter()
        .find(|v| &v.name == name && idx >= v.code_index && idx < v.code_index + u64::from(v.length))
        .or_else(|| vars.iter().find(|v| &v.name == name))
        .ok_or_else(|| {
            format!(
                "Unknown local variable '{name}' (for a static/instance field use Class.field or obj.field)"
            )
        })?;
    let sig_byte = *var.signature.as_bytes().first().ok_or_else(|| "Bad signature".to_string())?;
    let value = value_to_write(conn, Some(thread_id), frame_index, value_str, &var.signature).await?;
    if !tag_compatible(sig_byte, value.tag) {
        return Err(type_mismatch_err(name, &var.signature, &value));
    }
    conn.set_frame_value(thread_id, frame.frame_id, i32::try_from(var.slot).unwrap_or(0), &value)
        .await
        .map_err(|e| format!("Failed to set value: {e}"))?;
    Ok(format!("✅ Set local {name} = {value_str}"))
}

/// How an index/key literal reads back in a confirmation message.
fn render_arglit(a: &ArgLit) -> String {
    match a {
        ArgLit::Int(n) => n.to_string(),
        ArgLit::Long(n) => format!("{n}L"),
        ArgLit::Bool(b) => b.to_string(),
        ArgLit::Null => "null".to_string(),
        ArgLit::Str(s) => format!("\"{s}\""),
        ArgLit::Expr(e) => e.clone(),
    }
}

/// Byte offset of the `[` that opens the *final* top-level subscript of `target`, if it ends in one.
///
/// Scanned from the end at bracket depth 0, so a nested subscript inside a predicate
/// (`orders[?tags[0] == "x"]`) can't be mistaken for the outer one. `parse_expr` has already validated
/// that the brackets balance.
fn trailing_subscript_start(target: &str) -> Option<usize> {
    let t = target.trim_end();
    if !t.ends_with(']') {
        return None;
    }
    let mut depth = 0i32;
    for (i, c) in t.char_indices().rev() {
        match c {
            ']' => depth += 1,
            '[' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Write one element of an array, a `List`, or a `Map` — the `xs[0] = v` case of `set_value`.
///
/// Two mechanisms behind one syntax, split by whether invoking anything is needed: an array is written
/// with `ArrayReference.SetValues` and has no side effects, while a collection is written by calling a
/// method on it (see [`set_collection_element`]).
async fn set_element(
    conn: &mut jdwp_client::JdwpConnection,
    thread_opt: Option<u64>,
    frame_index: usize,
    container_expr: &str,
    key: &ArgLit,
    raw_value: &str,
) -> Result<String, String> {
    let tid = thread_opt.ok_or_else(|| {
        format!(
            "Writing '{container_expr}[…]' needs a suspended thread — pause one or hit a breakpoint first"
        )
    })?;
    let frames = conn
        .get_frames(tid, 0, -1)
        .await
        .map_err(|e| format!("Failed to get frames (is the thread suspended?): {e}"))?;
    let frame = frames.get(frame_index).or_else(|| frames.first()).cloned();
    let container = resolve_expression(conn, Some(tid), frame.as_ref(), container_expr).await?;
    let id = as_object_id(&container)
        .ok_or_else(|| format!("'{container_expr}' is null or a primitive, so it has no elements"))?;

    if container.tag == 91 {
        return set_array_element(conn, tid, frame_index, id, container_expr, key, raw_value).await;
    }
    set_collection_element(conn, tid, frame_index, frame.as_ref(), id, container_expr, key, raw_value).await
}

/// Write one element of a `List` (via `set(index, value)`) or a `Map` (via `put(key, value)`).
///
/// Both are found by *arity*, and looking for `set` before `put` is unambiguous because a `List` has no
/// `put` and a `Map` has no `set` — the same trick `apply_index` uses to find `get`. Both calls return
/// the element they displaced, so the confirmation reports old → new without a separate read.
#[allow(clippy::too_many_arguments)] // an element write needs all of it: where, what, and with what
async fn set_collection_element(
    conn: &mut jdwp_client::JdwpConnection,
    tid: u64,
    frame_index: usize,
    frame: Option<&jdwp_client::thread::Frame>,
    id: u64,
    container_expr: &str,
    key: &ArgLit,
    raw_value: &str,
) -> Result<String, String> {
    let type_id = conn
        .get_object_reference_type(id)
        .await
        .map_err(|e| format!("Failed to resolve type of '{container_expr}': {e}"))?;
    let writer = match find_method_arity(conn, type_id, "set", 2).await? {
        Some((d, m)) => Some((d, m, false)),
        None => find_method_arity(conn, type_id, "put", 2).await?.map(|(d, m)| (d, m, true)),
    };
    let Some((decl, m, is_map)) = writer else {
        let name = decode_signature(&conn.get_signature(type_id).await.unwrap_or_default());
        return Err(format!(
            "'{container_expr}' is a {name}, which has neither set(index, value) nor \
             put(key, value) — element writes work on arrays, List and Map"
        ));
    };

    // The index/key: a List index is an int; a Map key is whatever the caller wrote (boxed below).
    let key_value = if is_map {
        arglit_to_value(conn, Some(tid), frame, key).await?
    } else {
        let ArgLit::Int(i) = key else {
            return Err(format!("A List index must be an int, got {key:?} on '{container_expr}'"));
        };
        value_int(*i)
    };
    // The value parameter's declared type drives the literal's coercion; for `set(int, E)` and
    // `put(K, V)` that is a reference, so `coerce_args` boxes a primitive into its wrapper.
    let params = sig_param_types(&m.signature);
    let value_sig = params.get(1).map_or("Ljava/lang/Object;", String::as_str).to_string();
    let new_value = value_to_write(conn, Some(tid), frame_index, raw_value, &value_sig).await?;
    let args = coerce_args(conn, tid, &m.signature, vec![key_value, new_value]).await?;

    let (ret, exc) = conn
        .invoke_method(id, tid, decl, m.method_id, args)
        .await
        .map_err(|e| format!("{}() on '{container_expr}' failed: {e}", m.name))?;
    let displaced = invoke_result(conn, &m.name, ret, exc).await?;
    let old = render_value(conn, &displaced, Some(tid), 200).await;
    Ok(format!("✅ Set {container_expr}[{}] = {raw_value} (was {old}) via {}()", render_arglit(key), m.name))
}

/// Write one array element via `ArrayReference.SetValues`, coercing the literal to the array's
/// component type. No invocation, so — unlike the collection path — it has no side effects.
async fn set_array_element(
    conn: &mut jdwp_client::JdwpConnection,
    thread_opt: u64,
    frame_index: usize,
    id: u64,
    container_expr: &str,
    key: &ArgLit,
    raw_value: &str,
) -> Result<String, String> {
    let ArgLit::Int(i) = key else {
        return Err(format!("An array index must be an int, got {key:?} on '{container_expr}'"));
    };
    let len = conn
        .get_array_length(id)
        .await
        .map_err(|e| format!("Failed to read length of '{container_expr}': {e}"))?;
    if *i < 0 || *i >= len {
        return Err(format!("Index {i} is out of bounds for '{container_expr}' (length {len})"));
    }
    // "[I" -> 'I', "[Ljava/lang/String;" -> 'L'. The component type is what the value must match:
    // ArrayReference.SetValues writes untagged, so a wrong width would corrupt the element silently.
    let type_id = conn
        .get_object_reference_type(id)
        .await
        .map_err(|e| format!("Failed to resolve type of '{container_expr}': {e}"))?;
    let sig = conn.get_signature(type_id).await.unwrap_or_default();
    let component = sig.strip_prefix('[').unwrap_or(&sig).to_string();
    let sig_byte = *component.as_bytes().first().unwrap_or(&b'L');

    let old = conn.get_array_values(id, *i, 1).await.ok().and_then(|v| v.into_iter().next());
    let value = value_to_write(conn, Some(thread_opt), frame_index, raw_value, &component).await?;
    if !tag_compatible(sig_byte, value.tag) {
        return Err(format!(
            "'{container_expr}[{i}]' is {} — a {} literal can't be written to it",
            decode_signature(&component),
            decode_signature(&String::from_utf8_lossy(&[value.tag])),
        ));
    }
    conn.set_array_values(id, *i, std::slice::from_ref(&value))
        .await
        .map_err(|e| format!("Failed to write '{container_expr}[{i}]': {e}"))?;

    let was = match old {
        Some(v) => format!(" (was {})", render_value(conn, &v, None, 200).await),
        None => String::new(),
    };
    Ok(format!("✅ Set {container_expr}[{i}] = {raw_value}{was}"))
}

/// Instance-field attempt: resolve `container_expr` to an object via a suspended frame and write
/// `field_name`. Returns `Done` on success, `Fallthrough` (with the reason) when the container isn't
/// a usable object or there is no thread; errors only on a hard failure (null container, JVM error).
async fn set_instance_field(
    conn: &mut jdwp_client::JdwpConnection,
    thread_opt: Option<u64>,
    frame_index: usize,
    container_expr: &str,
    field_name: &str,
    value_str: &str,
) -> Result<FieldWrite, String> {
    let Some(thread_id) = thread_opt else {
        return Ok(FieldWrite::Fallthrough(None));
    };
    let frame = conn.get_frames(thread_id, 0, -1).await.ok().and_then(|f| f.get(frame_index).cloned());
    let v = match resolve_expression(conn, Some(thread_id), frame.as_ref(), container_expr).await {
        Ok(v) => v,
        Err(e) => return Ok(FieldWrite::Fallthrough(Some(e))),
    };
    let obj_id = match v.data {
        jdwp_client::types::ValueData::Object(0) => {
            return Err(format!("Cannot set '.{field_name}' — '{container_expr}' is null"))
        }
        jdwp_client::types::ValueData::Object(obj_id) => obj_id,
        _ => {
            return Ok(FieldWrite::Fallthrough(Some(format!(
                "'{container_expr}' is a primitive, not an object"
            ))))
        }
    };
    let type_id = conn
        .get_object_reference_type(obj_id)
        .await
        .map_err(|e| format!("Failed to resolve object type: {e}"))?;
    let (_, f) = find_field_info(conn, type_id, field_name, Some(false))
        .await?
        .ok_or_else(|| format!("No instance field '{field_name}' on the resolved object"))?;
    let sig_byte = *f.signature.as_bytes().first().ok_or_else(|| "Bad field signature".to_string())?;
    let value = value_to_write(conn, Some(thread_id), frame_index, value_str, &f.signature).await?;
    if !tag_compatible(sig_byte, value.tag) {
        return Err(type_mismatch_err(field_name, &f.signature, &value));
    }
    conn.set_object_values(obj_id, vec![(f.field_id, value)])
        .await
        .map_err(|e| format!("Failed to set instance field: {e}"))?;
    Ok(FieldWrite::Done(format!("✅ Set instance field {container_expr}.{field_name} = {value_str}")))
}

/// Static-field attempt: treat `container_expr` as a dotted class name and write its static field.
/// `Ok(None)` means the container isn't a loaded class (caller falls through to its final error).
async fn set_static_field(
    conn: &mut jdwp_client::JdwpConnection,
    thread_opt: Option<u64>,
    frame_index: usize,
    container_expr: &str,
    field_name: &str,
    value_str: &str,
) -> Result<Option<String>, String> {
    let Some(class_id) = resolve_class_by_dotted(conn, container_expr).await? else {
        return Ok(None);
    };
    let (_, f) = find_field_info(conn, class_id, field_name, Some(true))
        .await?
        .ok_or_else(|| format!("class '{container_expr}' has no static field '{field_name}'"))?;
    let sig_byte = *f.signature.as_bytes().first().ok_or_else(|| "Bad field signature".to_string())?;
    let value = value_to_write(conn, thread_opt, frame_index, value_str, &f.signature).await?;
    if !tag_compatible(sig_byte, value.tag) {
        return Err(type_mismatch_err(field_name, &f.signature, &value));
    }
    conn.set_reference_values(class_id, vec![(f.field_id, value)])
        .await
        .map_err(|e| format!("Failed to set static field: {e}"))?;
    Ok(Some(format!("✅ Set static field {container_expr}.{field_name} = {value_str}")))
}

async fn find_field(
    conn: &mut jdwp_client::JdwpConnection,
    type_id: u64,
    name: &str,
) -> Result<Option<u64>, String> {
    let mut current = Some(type_id);
    let mut guard = 0;
    while let Some(tid) = current {
        guard += 1;
        if guard > 50 {
            break;
        }
        let fields = conn.get_fields(tid).await.map_err(|e| format!("Failed to get fields: {e}"))?;
        if let Some(f) = fields.into_iter().find(|f| f.name == name) {
            return Ok(Some(f.field_id));
        }
        current = conn.get_superclass(tid).await.unwrap_or(None);
    }
    Ok(None)
}

/// Turn one parsed argument into a JDWP value. Literals are built directly; an `Expr` argument is
/// resolved in the caller's evaluation context, so an existing object (a local, `this`, or a field/
/// method chain) is passed **by reference** — the same object the debuggee already holds.
async fn arglit_to_value(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&jdwp_client::thread::Frame>,
    a: &ArgLit,
) -> Result<jdwp_client::types::Value, String> {
    Ok(match a {
        ArgLit::Int(n) => value_int(*n),
        ArgLit::Long(n) => value_long(*n),
        ArgLit::Bool(b) => value_bool(*b),
        ArgLit::Null => value_null(),
        ArgLit::Str(s) => {
            let id = conn.create_string(s).await.map_err(|e| format!("Failed to create string arg: {e}"))?;
            value_object(id)
        }
        ArgLit::Expr(e) => resolve_expression_boxed(conn, thread_id, frame, e)
            .await
            .map_err(|err| format!("argument '{e}': {err}"))?,
    })
}

async fn resolve_head(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: u64,
    frame: &jdwp_client::thread::Frame,
    seg: &Seg,
) -> Result<jdwp_client::types::Value, String> {
    use jdwp_client::types::{Value, ValueData};
    if seg.args.is_some() {
        return Err("Expression must start with a local variable or 'this'".to_string());
    }
    if seg.name == "this" {
        let obj = conn
            .get_this_object(thread_id, frame.frame_id)
            .await
            .map_err(|e| format!("Failed to get 'this': {e}"))?;
        if obj == 0 {
            return Err("No 'this' in this frame (static method)".to_string());
        }
        return Ok(Value { tag: 76, data: ValueData::Object(obj) });
    }
    let vars = conn
        .get_variable_table(frame.location.class_id, frame.location.method_id)
        .await
        .map_err(|e| format!("Failed to read local variable table (compiled without -g?): {e}"))?;
    let idx = frame.location.index;
    let var = vars
        .iter()
        .find(|v| v.name == seg.name && idx >= v.code_index && idx < v.code_index + u64::from(v.length))
        .or_else(|| vars.iter().find(|v| v.name == seg.name))
        .ok_or_else(|| format!("Unknown local variable '{}' in this frame", seg.name))?;
    let sig_byte = *var.signature.as_bytes().first().ok_or_else(|| "Bad variable signature".to_string())?;
    let slot = jdwp_client::stackframe::VariableSlot { slot: i32::try_from(var.slot).unwrap_or(0), sig_byte };
    let frame_values = conn
        .get_frame_values(thread_id, frame.frame_id, vec![slot])
        .await
        .map_err(|e| format!("Failed to read variable value: {e}"))?;
    frame_values.into_iter().next().ok_or_else(|| "No value returned for variable".to_string())
}

// ----- collection subscripts: OBJ-2 -----

/// How many elements a slice or filter will read from a collection before giving up.
///
/// A filter has to look at every element to be meaningful, but "every element" of a production
/// collection can be millions — each one a JDWP round trip. So the scan is capped and the result says
/// how much of the collection it actually covered, rather than quietly reporting a partial answer as
/// if it were complete.
const SUBSCRIPT_SCAN_CAP: i32 = 1000;

/// Apply a segment's `[…]` subscripts left to right.
///
/// An `Index` narrows to one value, so it can be followed by more subscripts or more chain. A `Range`
/// or `Filter` produces several, which ends the expression — the caller enforces that.
async fn apply_subscripts(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&jdwp_client::thread::Frame>,
    base: jdwp_client::types::Value,
    subs: &[Subscript],
    label: &str,
) -> Result<Resolved, String> {
    let mut current = base;
    for (i, sub) in subs.iter().enumerate() {
        match sub {
            Subscript::Index(key) => {
                current = apply_index(conn, thread_id, frame, &current, key, label).await?;
            }
            Subscript::Range(from, to) => {
                if i + 1 < subs.len() {
                    return Err(multi_then_chain_error(label));
                }
                return apply_range(conn, thread_id, &current, *from, *to, label).await;
            }
            Subscript::Filter(pred) => {
                if i + 1 < subs.len() {
                    return Err(multi_then_chain_error(label));
                }
                // Boxed: a predicate re-enters expression resolution, which can reach a nested
                // subscript, and every such cycle runs through here.
                return apply_filter_boxed(conn, thread_id, frame, &current, pred, label).await;
            }
        }
    }
    Ok(Resolved::One(current))
}

/// `expr[i]` on an array or `List`, or `expr[key]` on a `Map`.
///
/// A `Map` is tried first when the object has `get(Object)`, because `counts["a"]` should mean the
/// mapping, not an ordinal position.
async fn apply_index(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&jdwp_client::thread::Frame>,
    base: &jdwp_client::types::Value,
    key: &ArgLit,
    label: &str,
) -> Result<jdwp_client::types::Value, String> {
    let id =
        as_object_id(base).ok_or_else(|| format!("Cannot index '{label}' — it is null or a primitive"))?;

    // Arrays are indexable without invoking anything, so handle them before touching the debuggee.
    if base.tag == 91 {
        let ArgLit::Int(i) = key else {
            return Err(format!("An array index must be an int, got {key:?} on '{label}'"));
        };
        let len = conn
            .get_array_length(id)
            .await
            .map_err(|e| format!("Failed to read length of '{label}': {e}"))?;
        if *i < 0 || *i >= len {
            return Err(format!("Index {i} is out of bounds for '{label}' (length {len})"));
        }
        return conn
            .get_array_values(id, *i, 1)
            .await
            .map_err(|e| format!("Failed to read '{label}[{i}]': {e}"))?
            .into_iter()
            .next()
            .ok_or_else(|| format!("No value returned for '{label}[{i}]'"));
    }

    let tid = thread_id.ok_or_else(|| {
        format!("Indexing '{label}' needs a suspended thread — it calls get() in the debuggee")
    })?;
    let type_id = conn
        .get_object_reference_type(id)
        .await
        .map_err(|e| format!("Failed to resolve type of '{label}': {e}"))?;

    // Look `get` up by *arity*, not by argument type, and read its parameter to decide how to call
    // it. Two reasons: a type-aware lookup can't match `Map.get(Object)` against an int key at all
    // (that needs boxing first), and "no 1-arg get()" is the only honest test for "not indexable" —
    // matching on the key type instead would report a String index into a List as "not indexable".
    let Some((decl, m)) = find_method_arity(conn, type_id, "get", 1).await? else {
        return Err(format!(
            "'{label}' is not indexable — no 1-argument get() found (arrays, List and Map are supported)"
        ));
    };
    let params = sig_param_types(&m.signature);
    let takes_reference = params.first().is_some_and(|p| p.starts_with('L') || p.starts_with('['));

    let arg = if takes_reference {
        // Map.get(Object) cannot take a raw primitive: hand it a wrapper, or the JVM reads the int as
        // an object pointer and dies.
        let key_value = arglit_to_value(conn, thread_id, frame, key).await?;
        if key_value.data.format_primitive().is_some() {
            box_primitive(conn, tid, &key_value)
                .await
                .ok_or_else(|| format!("Could not box the key for '{label}[…]' — try a String key"))?
        } else {
            key_value
        }
    } else {
        // A List: get(int) needs a genuine int index.
        let ArgLit::Int(i) = key else {
            return Err(format!(
                "A list index must be an int — '{label}' takes {}, got {key:?}",
                params.first().map_or("?", String::as_str)
            ));
        };
        value_int(*i)
    };

    let (ret, exc) = conn
        .invoke_method(id, tid, decl, m.method_id, vec![arg])
        .await
        .map_err(|e| format!("'{label}[…]' get() failed: {e}"))?;
    invoke_result(conn, "get", ret, exc).await
}

/// Wrap a primitive value in its `java.lang.*` box via `Wrapper.valueOf(x)`.
async fn box_primitive(
    conn: &mut jdwp_client::JdwpConnection,
    tid: u64,
    v: &jdwp_client::types::Value,
) -> Option<jdwp_client::types::Value> {
    use jdwp_client::types::ValueData;
    let class = match v.data {
        ValueData::Int(_) => "java.lang.Integer",
        ValueData::Long(_) => "java.lang.Long",
        ValueData::Short(_) => "java.lang.Short",
        ValueData::Byte(_) => "java.lang.Byte",
        ValueData::Char(_) => "java.lang.Character",
        ValueData::Boolean(_) => "java.lang.Boolean",
        ValueData::Float(_) => "java.lang.Float",
        ValueData::Double(_) => "java.lang.Double",
        ValueData::Object(_) | ValueData::Void => return None,
    };
    let type_id = resolve_class_by_dotted(conn, class).await.ok()??;
    let (decl, m) =
        find_method_for_args(conn, type_id, "valueOf", std::slice::from_ref(v), Some(true)).await.ok()??;
    let (ret, exc) = conn.invoke_static_method(decl, tid, m.method_id, vec![v.clone()]).await.ok()?;
    (exc == 0).then_some(ret)
}

/// What one scan of a container yielded.
struct Scan {
    /// The elements read — for a `Map`, its *values*.
    values: Vec<jdwp_client::types::Value>,
    /// Rendered keys, parallel to `values`, when the container was a `Map`. Empty otherwise.
    keys: Vec<String>,
    /// The container's full length, which may exceed what was read (the scan cap).
    len: i32,
    /// The container's type name, for the result header.
    name: String,
}

/// Whether a scan may descend into a `Map`'s entries.
///
/// A filter can — it renders survivors as `key → value`. A slice can't: a map has no positional order
/// to take a range of.
#[derive(PartialEq, Eq)]
enum MapScan {
    Refuse,
    Entries,
}

/// Read a bounded prefix of an array's, collection's, or map's elements.
///
/// A `Collection` needs a suspended thread (it calls `toArray()`); arrays don't. A `Map` needs one too,
/// and costs the most: `entrySet()`, `toArray()`, then `getKey()`/`getValue()` per entry — which is why
/// the scan cap matters more here than anywhere else.
async fn scan_elements(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    base: &jdwp_client::types::Value,
    label: &str,
    maps: MapScan,
) -> Result<Scan, String> {
    let id = as_object_id(base)
        .ok_or_else(|| format!("Cannot slice or filter '{label}' — it is null or a primitive"))?;
    let name = type_name_of(conn, id).await;

    let arr = if base.tag == 91 {
        id
    } else {
        let tid = thread_id.ok_or_else(|| {
            format!("Slicing or filtering '{label}' needs a suspended thread — it calls toArray() in the debuggee")
        })?;
        let type_id = conn
            .get_object_reference_type(id)
            .await
            .map_err(|e| format!("Failed to resolve type of '{label}': {e}"))?;
        match classify_container(conn, type_id, &name).await {
            Some(ContainerKind::Collection) => invoke_no_arg(conn, id, type_id, tid, "toArray")
                .await
                .as_ref()
                .and_then(as_object_id)
                .ok_or_else(|| format!("toArray() on '{label}' returned nothing usable"))?,
            Some(ContainerKind::Map) if maps == MapScan::Entries => {
                return scan_map_entries(conn, id, type_id, tid, label, name).await
            }
            // A slice needs positional order, which a Map has none of.
            Some(ContainerKind::Map) => {
                return Err(format!(
                    "'{label}' is a Map, so there is no order to slice. Use {label}[\"key\"] for one \
                     entry, or a filter ({label}[?…]) which keeps the keys."
                ))
            }
            _ => {
                return Err(format!(
                    "'{label}' is not sliceable — expected an array or a Collection, got {name}"
                ))
            }
        }
    };

    let len =
        conn.get_array_length(arr).await.map_err(|e| format!("Failed to read length of '{label}': {e}"))?;
    let take = len.min(SUBSCRIPT_SCAN_CAP);
    let values = if take == 0 {
        Vec::new()
    } else {
        conn.get_array_values(arr, 0, take)
            .await
            .map_err(|e| format!("Failed to read elements of '{label}': {e}"))?
    };
    Ok(Scan { values, keys: Vec::new(), len, name })
}

/// Read a `Map`'s entries as (rendered key, value) pairs, so a filter over the values can still say
/// which key each survivor was under.
///
/// Keys ARE rendered with `toString()`. Normally this code avoids that (see `describe_field_event`), but
/// a key exists to identify its entry, and a real key is often an object: measured against Micrometer,
/// `meterMap` is keyed by `Meter.Id`, which without `toString()` renders as
/// `Meter$Id (id=0xaf)` — true, and useless. The filter is already invoking a predicate against every
/// value, so one more call per surviving entry changes nothing about the side effects.
async fn scan_map_entries(
    conn: &mut jdwp_client::JdwpConnection,
    id: u64,
    type_id: u64,
    tid: u64,
    label: &str,
    name: String,
) -> Result<Scan, String> {
    let set = invoke_no_arg(conn, id, type_id, tid, "entrySet")
        .await
        .as_ref()
        .and_then(as_object_id)
        .ok_or_else(|| format!("entrySet() on '{label}' returned nothing usable"))?;
    let set_type = conn
        .get_object_reference_type(set)
        .await
        .map_err(|e| format!("Failed to resolve the entry set of '{label}': {e}"))?;
    let arr = invoke_no_arg(conn, set, set_type, tid, "toArray")
        .await
        .as_ref()
        .and_then(as_object_id)
        .ok_or_else(|| format!("toArray() on the entry set of '{label}' returned nothing usable"))?;
    let len = conn
        .get_array_length(arr)
        .await
        .map_err(|e| format!("Failed to read the entry count of '{label}': {e}"))?;
    let take = len.min(SUBSCRIPT_SCAN_CAP);
    let entries = if take == 0 {
        Vec::new()
    } else {
        conn.get_array_values(arr, 0, take)
            .await
            .map_err(|e| format!("Failed to read entries of '{label}': {e}"))?
    };

    let mut values = Vec::with_capacity(entries.len());
    let mut keys = Vec::with_capacity(entries.len());
    for e in &entries {
        // An unreadable entry is skipped rather than failing the whole scan, matching how the deep
        // renderer treats one.
        if let Some((k, v)) = entry_pair(conn, e, tid).await {
            keys.push(render_value(conn, &k, Some(tid), 120).await);
            values.push(v);
        }
    }
    Ok(Scan { values, keys, len, name })
}

/// `expr[a..b]` — a half-open slice.
async fn apply_range(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    base: &jdwp_client::types::Value,
    from: i64,
    to: i64,
    label: &str,
) -> Result<Resolved, String> {
    let Scan { values, len, name, .. } = scan_elements(conn, thread_id, base, label, MapScan::Refuse).await?;
    if from < 0 {
        return Err(format!("Range start must not be negative in '{label}[{from}..{to}]'"));
    }
    // Clamp rather than error: `list[0..100]` on a 20-element list is a normal way to ask for
    // "up to 100", and erroring would just make the caller guess the length first.
    let start = usize::try_from(from).unwrap_or(0).min(values.len());
    let end = usize::try_from(to).unwrap_or(0).min(values.len());
    let slice = values.get(start..end).unwrap_or_default().to_vec();
    let scanned = i32::try_from(values.len()).unwrap_or(i32::MAX);
    let note = if scanned < len {
        format!(" (only the first {scanned} of {len} were read — scan cap)")
    } else {
        String::new()
    };
    Ok(Resolved::Many {
        header: format!("{name}[{from}..{to}] → {} of {len}{note}", slice.len()),
        values: slice,
        keys: Vec::new(),
    })
}

/// Boxed, type-erased entry to [`apply_filter`] — breaks the async recursion cycle
/// (subscript → filter → predicate → expression → subscript).
fn apply_filter_boxed<'a>(
    conn: &'a mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&'a jdwp_client::thread::Frame>,
    base: &'a jdwp_client::types::Value,
    predicate: &'a str,
    label: &'a str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Resolved, String>> + Send + 'a>> {
    Box::pin(apply_filter(conn, thread_id, frame, base, predicate, label))
}

/// `expr[?predicate]` — keep the elements the predicate holds for.
async fn apply_filter(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&jdwp_client::thread::Frame>,
    base: &jdwp_client::types::Value,
    predicate: &str,
    label: &str,
) -> Result<Resolved, String> {
    // Prepare the predicate BEFORE reading any elements. Two reasons, one of them a correctness bug
    // found the hard way: `scan_elements` invokes `toArray()` in the debuggee, and JDWP invalidates a
    // thread's frame ids as soon as a method is invoked on it — so a right-hand side like
    // `order.threshold`, which reads a local, must be resolved while the frame is still valid. It also
    // means an element-independent right side is evaluated once instead of once per element.
    let pred = prepare_predicate(conn, thread_id, frame, predicate).await?;

    // A Map filters by its VALUES — `meters[?id.name == "x"]` reads naturally that way — and the
    // matching keys come along so the result can say which entry each survivor was.
    let Scan { values, keys, len, name } =
        scan_elements(conn, thread_id, base, label, MapScan::Entries).await?;
    let scanned = i32::try_from(values.len()).unwrap_or(i32::MAX);
    let is_map = !keys.is_empty();

    let mut kept = Vec::new();
    let mut kept_keys = Vec::new();
    let mut errors = 0usize;
    let mut first_error = None;
    // Each value carries its own key by value, so a survivor takes ownership instead of cloning out of a
    // shared vector on every match. Padded with `None` rather than zipped: a non-map scan has no keys at
    // all, and a plain zip would silently filter every value away.
    let mut keyed = keys.into_iter().map(Some).chain(std::iter::repeat_with(|| None));
    for v in values {
        let key = keyed.next().flatten();
        match eval_predicate_on(conn, thread_id, &v, &pred).await {
            Ok(true) => {
                if let Some(k) = key {
                    kept_keys.push(k);
                }
                kept.push(v);
            }
            Ok(false) => {}
            Err(e) => {
                errors += 1;
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }
    // A predicate that fails on every element is a broken predicate, not an empty result — say so
    // instead of reporting "0 matched" and letting the caller believe the collection was checked.
    if errors > 0 && kept.is_empty() && errors == usize::try_from(scanned).unwrap_or(usize::MAX) {
        return Err(format!(
            "Predicate '{predicate}' failed on every element of '{label}': {}",
            first_error.unwrap_or_default()
        ));
    }
    let note = match (scanned < len, errors) {
        (true, 0) => format!(" (scanned the first {scanned} of {len} — scan cap)"),
        (true, n) => format!(" (scanned the first {scanned} of {len} — scan cap; {n} element(s) errored)"),
        (false, 0) => String::new(),
        (false, n) => format!(" ({n} element(s) errored)"),
    };
    let unit = if is_map { "entr(ies)" } else { "matched" };
    Ok(Resolved::Many {
        header: format!("{name}[?{predicate}] → {} of {scanned} {unit}{note}", kept.len()),
        values: kept,
        keys: kept_keys,
    })
}

/// A prepared filter predicate (EVAL-4): a boolean tree whose comparison leaves have their
/// element-independent right side already resolved, so scanning re-resolves only the per-element half.
enum Predicate {
    Or(Vec<Self>),
    And(Vec<Self>),
    /// `lhs OP rhs`: `lhs` is re-resolved against each element, `rhs` was resolved once.
    Compare {
        lhs: String,
        op: String,
        rhs: PredRhs,
    },
    /// A boolean chain evaluated against each element.
    Bool(String),
}

/// The right-hand side of a comparison: a literal, or a value already read from the frame.
enum PredRhs {
    Lit(ArgLit),
    Value(jdwp_client::types::Value),
}

/// Parse a predicate and resolve every comparison leaf's element-independent right side **once**,
/// before any element is read (EVAL-4 keeps the OBJ-2 optimisation, per leaf).
///
/// Each leaf's left side is deliberately kept as text: it is resolved *against each element*, which is
/// what lets `orders[?status == "OPEN" && qty > 3]` work without an element variable.
async fn prepare_predicate(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&jdwp_client::thread::Frame>,
    predicate: &str,
) -> Result<Predicate, String> {
    prepare_pred_tree(conn, thread_id, frame, &parse_bool_tree(predicate)).await
}

/// Recursively prepare a predicate from a parsed boolean tree. Boxed because the tree is recursive.
fn prepare_pred_tree<'a>(
    conn: &'a mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&'a jdwp_client::thread::Frame>,
    tree: &'a BoolTree,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Predicate, String>> + Send + 'a>> {
    Box::pin(async move {
        match tree {
            BoolTree::Or(branches) => {
                let mut out = Vec::with_capacity(branches.len());
                for b in branches {
                    out.push(prepare_pred_tree(conn, thread_id, frame, b).await?);
                }
                Ok(Predicate::Or(out))
            }
            BoolTree::And(branches) => {
                let mut out = Vec::with_capacity(branches.len());
                for b in branches {
                    out.push(prepare_pred_tree(conn, thread_id, frame, b).await?);
                }
                Ok(Predicate::And(out))
            }
            BoolTree::Leaf(leaf) => {
                let Some((lhs, op, rhs)) = split_comparison(leaf) else {
                    return Ok(Predicate::Bool(leaf.clone()));
                };
                let rhs = match parse_lit(rhs.trim())? {
                    ArgLit::Expr(e) => PredRhs::Value(resolve_expression(conn, thread_id, frame, &e).await?),
                    lit => PredRhs::Lit(lit),
                };
                Ok(Predicate::Compare { lhs, op, rhs })
            }
        }
    })
}

/// Evaluate a prepared predicate against one element (short-circuit).
///
/// Takes no frame: by this point every frame-dependent part is already a value, and the element's own
/// fields and methods are reached through its object id, which invocation does not invalidate. Boxed
/// because the predicate tree is recursive.
fn eval_predicate_on<'a>(
    conn: &'a mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    element: &'a jdwp_client::types::Value,
    pred: &'a Predicate,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + 'a>> {
    Box::pin(async move {
        match pred {
            Predicate::Or(branches) => {
                for p in branches {
                    if eval_predicate_on(conn, thread_id, element, p).await? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Predicate::And(branches) => {
                for p in branches {
                    if !eval_predicate_on(conn, thread_id, element, p).await? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Predicate::Compare { lhs, op, rhs } => {
                let lv = resolve_relative(conn, thread_id, None, element, lhs).await?;
                match rhs {
                    PredRhs::Value(rv) => compare_resolved(conn, &lv, op, rv).await,
                    PredRhs::Lit(lit) => compare_values(conn, &lv, op, lit).await,
                }
            }
            Predicate::Bool(expr) => {
                let v = resolve_relative(conn, thread_id, None, element, expr).await?;
                match v.data {
                    jdwp_client::types::ValueData::Boolean(b) => Ok(b),
                    _ => Err(format!("Predicate '{expr}' did not evaluate to a boolean")),
                }
            }
        }
    })
}

/// Resolve a chain (`status`, `customer.name`, `getTotal()`) starting from `base` rather than from a
/// local or a class — the element-relative resolution filters need.
async fn resolve_relative(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&jdwp_client::thread::Frame>,
    base: &jdwp_client::types::Value,
    expr: &str,
) -> Result<jdwp_client::types::Value, String> {
    let segs = parse_expr(expr)?;
    let mut current = base.clone();
    for seg in &segs {
        let member = resolve_member(conn, thread_id, frame, &current, seg).await?;
        current = apply_subscripts(conn, thread_id, frame, member, &seg.subs, &seg.name)
            .await?
            .single("A filter predicate")?;
    }
    Ok(current)
}

async fn resolve_member(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&jdwp_client::thread::Frame>,
    current: &jdwp_client::types::Value,
    seg: &Seg,
) -> Result<jdwp_client::types::Value, String> {
    use jdwp_client::types::ValueData;
    let obj_id = match &current.data {
        ValueData::Object(0) => return Err(format!("Cannot access '.{}' on null", seg.name)),
        ValueData::Object(id) => *id,
        _ => return Err(format!("Cannot access '.{}' on a primitive value", seg.name)),
    };
    let type_id = conn
        .get_object_reference_type(obj_id)
        .await
        .map_err(|e| format!("Failed to resolve object type: {e}"))?;

    if let Some(arglits) = &seg.args {
        invoke_segment_method(conn, thread_id, frame, obj_id, type_id, seg, arglits).await
    } else {
        read_segment_field(conn, obj_id, type_id, seg).await
    }
}

/// Invoke `seg` as a method call on `obj_id` (of `type_id`), returning its result value.
async fn invoke_segment_method(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&jdwp_client::thread::Frame>,
    obj_id: u64,
    type_id: u64,
    seg: &Seg,
    arglits: &[ArgLit],
) -> Result<jdwp_client::types::Value, String> {
    let tid = thread_id.ok_or_else(|| {
        format!("Calling '.{}()' needs a suspended thread — pause one or hit a breakpoint first", seg.name)
    })?;
    let argvals = eval_args(conn, thread_id, frame, arglits).await?;
    let (decl, m) =
        find_method_for_args(conn, type_id, &seg.name, &argvals, None).await?.ok_or_else(|| {
            format!(
                "No method '{}' on the object accepts {} argument(s) of these types",
                seg.name,
                argvals.len()
            )
        })?;
    // Box any primitive the chosen overload declares as a reference (`f(Integer)` given `5`).
    let argvals = coerce_args(conn, tid, &m.signature, argvals).await?;
    let (ret, exc) = conn
        .invoke_method(obj_id, tid, decl, m.method_id, argvals)
        .await
        .map_err(|e| format!("invoke {}() failed: {}", seg.name, e))?;
    invoke_result(conn, &seg.name, ret, exc).await
}

/// Resolve every parsed argument of a call to a JDWP value, in source order.
async fn eval_args(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&jdwp_client::thread::Frame>,
    arglits: &[ArgLit],
) -> Result<Vec<jdwp_client::types::Value>, String> {
    let mut argvals = Vec::with_capacity(arglits.len());
    for a in arglits {
        argvals.push(arglit_to_value(conn, thread_id, frame, a).await?);
    }
    Ok(argvals)
}

/// Unwrap an `InvokeMethod` outcome: a non-zero exception id means the invoked method threw, which
/// is reported as an error naming the exception type rather than a value.
async fn invoke_result(
    conn: &mut jdwp_client::JdwpConnection,
    name: &str,
    ret: jdwp_client::types::Value,
    exc: u64,
) -> Result<jdwp_client::types::Value, String> {
    if exc != 0 {
        let tn = match conn.get_object_reference_type(exc).await {
            Ok(t) => decode_signature(&conn.get_signature(t).await.unwrap_or_default()),
            Err(_) => "an exception".to_string(),
        };
        return Err(format!("{name}() threw {tn}"));
    }
    Ok(ret)
}

/// Read `seg` as a field access on `obj_id` (of `type_id`), returning the field's value.
async fn read_segment_field(
    conn: &mut jdwp_client::JdwpConnection,
    obj_id: u64,
    type_id: u64,
    seg: &Seg,
) -> Result<jdwp_client::types::Value, String> {
    let fid = find_field(conn, type_id, &seg.name)
        .await?
        .ok_or_else(|| format!("No field '{}' found on the object", seg.name))?;
    let vals = conn
        .get_object_values(obj_id, vec![fid])
        .await
        .map_err(|e| format!("Failed to read field '{}': {}", seg.name, e))?;
    vals.into_iter().next().ok_or_else(|| "No value returned for field".to_string())
}

/// `resolve_expression` with its future boxed and type-erased. An `Expr` argument inside a method
/// call re-enters expression resolution, and an `async fn` cannot name its own future type — this
/// erases it. Recursion terminates because every sub-expression is strictly shorter than its parent.
fn resolve_expression_boxed<'a>(
    conn: &'a mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&'a jdwp_client::thread::Frame>,
    expr: &'a str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<jdwp_client::types::Value, String>> + Send + 'a>>
{
    Box::pin(resolve_expression(conn, thread_id, frame, expr))
}

/// The outcome of resolving an expression.
///
/// A slice or filter yields several values, and JDWP has no "several values" value — materialising a
/// new array in the debuggee would mean allocating in the program under inspection. So those end the
/// expression: `orders[0].name` chains fine (an index narrows to one value), while
/// `orders[?paid == true].name` is rejected with an explanation rather than silently picking one.
enum Resolved {
    One(jdwp_client::types::Value),
    Many {
        /// How the selection went, e.g. "3 of 20 matched" — worth reporting even when empty.
        header: String,
        values: Vec<jdwp_client::types::Value>,
        /// Rendered keys, parallel to `values`, when the selection came from a `Map`. Empty otherwise.
        ///
        /// Filtering a map by its values is the useful operation (`meters[?id.name == "x"]`), but a
        /// bare list of survivors throws away the thing you were looking for — which key each one was
        /// under. Carrying the keys alongside lets the result render as `key → value`.
        keys: Vec<String>,
    },
}

impl Resolved {
    /// Require a single value, explaining the restriction if the expression produced several.
    fn single(self, what: &str) -> Result<jdwp_client::types::Value, String> {
        match self {
            Self::One(v) => Ok(v),
            Self::Many { .. } => Err(format!(
                "{what} needs a single value, but this expression ends in a slice or filter which \
                 selects several. Narrow it with an index (e.g. [0]) or drop the subscript."
            )),
        }
    }
}

/// Resolve an expression to exactly one value. The common path: every existing caller (conditions,
/// `set_value`, call arguments, trace expressions) needs one value and gets a clear error otherwise.
async fn resolve_expression(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&jdwp_client::thread::Frame>,
    expr: &str,
) -> Result<jdwp_client::types::Value, String> {
    resolve_expression_multi(conn, thread_id, frame, expr).await?.single("This")
}

async fn resolve_expression_multi(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&jdwp_client::thread::Frame>,
    expr: &str,
) -> Result<Resolved, String> {
    let segs = parse_expr(expr)?;
    let Some(head_seg) = segs.first() else {
        return Err("Empty expression".to_string());
    };

    // Head resolution has two paths. With a suspended frame we first try the head segment as a
    // local variable or `this` (the common case at a breakpoint). If there is no frame, or the
    // head isn't a local, we fall back to reading a static field off a class named by the leading
    // dotted prefix (e.g. `br.com.infotravel.util.ConfigDefaultUtils.dsUrlMotor`). Static reads
    // don't need a suspended thread at all.
    let head_result = match (thread_id, frame) {
        (Some(tid), Some(fr)) => Some(resolve_head(conn, tid, fr, head_seg).await),
        _ => None,
    };

    let (mut current, start) = if let Some(Ok(v)) = head_result {
        (v, 1usize)
    } else {
        let (v, consumed) = resolve_static_head(conn, thread_id, frame, &segs).await.map_err(
            |static_err| match &head_result {
                Some(Err(head_err)) => {
                    format!("{head_err} (also not a resolvable static member: {static_err})")
                }
                _ => format!(
                    "No suspended frame to read locals from, and not a resolvable static member: {static_err}"
                ),
            },
        )?;
        (v, consumed)
    };

    // The head's own subscripts still have to be applied — `orders[0]` is a single segment. For a
    // static head, `start` counts the class-name prefix too, so the member is the last consumed one.
    let head_owner = segs
        .get(start.saturating_sub(1))
        .ok_or_else(|| "Internal error: head resolution consumed no segments".to_string())?;
    match apply_subscripts(conn, thread_id, frame, current, &head_owner.subs, &head_owner.name).await? {
        Resolved::One(v) => current = v,
        // A multi-value subscript must be the last thing in the expression.
        many @ Resolved::Many { .. } => {
            return if start < segs.len() { Err(multi_then_chain_error(&head_owner.name)) } else { Ok(many) }
        }
    }

    let last = segs.len().saturating_sub(1);
    for (i, seg) in segs.iter().enumerate().skip(start) {
        let member = resolve_member(conn, thread_id, frame, &current, seg).await?;
        match apply_subscripts(conn, thread_id, frame, member, &seg.subs, &seg.name).await? {
            Resolved::One(v) => current = v,
            many @ Resolved::Many { .. } => {
                return if i < last { Err(multi_then_chain_error(&seg.name)) } else { Ok(many) };
            }
        }
    }
    Ok(Resolved::One(current))
}

fn multi_then_chain_error(name: &str) -> String {
    format!(
        "'{name}[…]' selects several values, so nothing can be chained after it. \
         Use an index (e.g. [0]) to pick one, or make the slice/filter the end of the expression."
    )
}

/// Fallback head resolution: treat a leading dotted prefix as a class name, then read the next
/// segment as a static **field** or invoke it as a static **method**.
///
/// Given segments like `[br, com, infotravel, util, ConfigDefaultUtils, dsUrlMotor]`, try the
/// longest class prefix first (so package names and nested classes win), resolve it to a loaded
/// reference type, then resolve the next segment against it. Returns the value plus the number of
/// segments consumed (class prefix + the member), so the caller can continue chaining
/// (`.getFoo()`, `.bar`) on the result.
///
/// A static field read needs no suspended thread; a static method call does (JDWP runs the
/// invocation on a thread), and says so if none is available.
async fn resolve_static_head(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&jdwp_client::thread::Frame>,
    segs: &[Seg],
) -> Result<(jdwp_client::types::Value, usize), String> {
    let n = segs.len();
    if n < 2 {
        return Err("a static access needs at least Class.field or Class.method()".to_string());
    }
    // k = number of segments forming the class name; the member is the next segment. Longest
    // prefix first. `split_at(k)` keeps every access in-bounds (1 <= k < n).
    for k in (1..n).rev() {
        let (class_segs, rest) = segs.split_at(k);
        let Some(member) = rest.first() else { continue };
        // Only the member segment may carry arguments — a class name never does.
        if class_segs.iter().any(|s| s.args.is_some()) {
            continue;
        }
        let dotted = class_segs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(".");
        let Some(type_id) = resolve_class_by_dotted(conn, &dotted).await? else { continue };
        let v = match &member.args {
            Some(arglits) => {
                invoke_static_member(conn, thread_id, frame, type_id, &dotted, member, arglits).await?
            }
            None => read_static_field(conn, type_id, &dotted, &member.name).await?,
        };
        return Ok((v, k + 1));
    }
    Err("no loaded class matches the leading segment(s)".to_string())
}

/// Read `Class.field` off a resolved reference type. Needs no suspended thread.
async fn read_static_field(
    conn: &mut jdwp_client::JdwpConnection,
    type_id: u64,
    dotted: &str,
    name: &str,
) -> Result<jdwp_client::types::Value, String> {
    let fid = find_static_field(conn, type_id, name)
        .await?
        .ok_or_else(|| format!("class '{dotted}' has no static field '{name}'"))?;
    let vals = conn
        .get_reference_values(type_id, vec![fid])
        .await
        .map_err(|e| format!("Failed to read static field '{name}': {e}"))?;
    vals.into_iter().next().ok_or_else(|| "No value returned for static field".to_string())
}

/// Invoke `Class.method(args)` via `ClassType.InvokeMethod`.
///
/// Overload selection is restricted to *static* methods, so an instance method of the same name and
/// arity can't be picked (JDWP would reject the invoke). The declaring class from the lookup — not
/// the class the user named — is what gets invoked, which is what JDWP requires for an inherited
/// static.
async fn invoke_static_member(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&jdwp_client::thread::Frame>,
    type_id: u64,
    dotted: &str,
    member: &Seg,
    arglits: &[ArgLit],
) -> Result<jdwp_client::types::Value, String> {
    let tid = thread_id.ok_or_else(|| {
        format!(
            "Calling static '{}.{}()' needs a suspended thread — pause one or hit a breakpoint first",
            dotted, member.name
        )
    })?;
    let argvals = eval_args(conn, thread_id, frame, arglits).await?;
    let (decl, m) =
        find_method_for_args(conn, type_id, &member.name, &argvals, Some(true)).await?.ok_or_else(|| {
            format!(
                "class '{}' has no static method '{}' accepting {} argument(s) of these types",
                dotted,
                member.name,
                argvals.len()
            )
        })?;
    // Box any primitive the chosen overload declares as a reference (`f(Integer)` given `5`).
    let argvals = coerce_args(conn, tid, &m.signature, argvals).await?;
    let (ret, exc) = conn
        .invoke_static_method(decl, tid, m.method_id, argvals)
        .await
        .map_err(|e| format!("invoke static {}.{}() failed: {}", dotted, member.name, e))?;
    invoke_result(conn, &member.name, ret, exc).await
}

/// Resolve a dotted class name to a loaded reference type id.
///
/// First tries the name as fully-qualified via `classes_by_signature`. If that misses and the name
/// is a bare simple name (no dot), scans `all_classes` for a class whose signature ends in
/// `/Name;` — so `ConfigDefaultUtils.dsUrlMotor` works without spelling out the package. Prefers a
/// class (tag 1) over an interface when several match.
///
/// Shares `descriptor_candidates` with `resolve_loaded_class` rather than building its own descriptor:
/// a hidden class is spelled two ways across the supported JDKs (DISC-4, #50), and the second resolver
/// in this file is exactly where that would have been fixed in one place and not the other.
async fn resolve_class_by_dotted(
    conn: &mut jdwp_client::JdwpConnection,
    dotted: &str,
) -> Result<Option<u64>, String> {
    for sig in descriptor_candidates(dotted) {
        let classes =
            conn.classes_by_signature(&sig).await.map_err(|e| format!("classes_by_signature failed: {e}"))?;
        if let Some(c) = classes.iter().find(|c| c.ref_type_tag == 1).or_else(|| classes.first()) {
            return Ok(Some(c.type_id));
        }
    }

    if !dotted.contains('.') {
        let suffix = format!("/{dotted};");
        let bare = format!("L{dotted};"); // default-package class
        let all = conn.all_classes().await.map_err(|e| format!("all_classes failed: {e}"))?;
        let matches = |s: &str| s.ends_with(&suffix) || s == bare;
        if let Some(c) = all.iter().find(|c| c.ref_type_tag == 1 && matches(&c.signature)) {
            return Ok(Some(c.type_id));
        }
        if let Some(c) = all.iter().find(|c| matches(&c.signature)) {
            return Ok(Some(c.type_id));
        }
    }
    Ok(None)
}

/// Find a static field by name, walking the superclass chain. Skips instance fields so the id we
/// hand to ReferenceType.GetValues is always a valid static.
async fn find_static_field(
    conn: &mut jdwp_client::JdwpConnection,
    type_id: u64,
    name: &str,
) -> Result<Option<u64>, String> {
    const ACC_STATIC: i32 = 0x0008;
    let mut current = Some(type_id);
    let mut guard = 0;
    while let Some(tid) = current {
        guard += 1;
        if guard > 50 {
            break;
        }
        let fields = conn.get_fields(tid).await.map_err(|e| format!("Failed to get fields: {e}"))?;
        if let Some(f) = fields.into_iter().find(|f| f.name == name && (f.mod_bits & ACC_STATIC) != 0) {
            return Ok(Some(f.field_id));
        }
        current = conn.get_superclass(tid).await.unwrap_or(None);
    }
    Ok(None)
}

/// Shallow render of an array element (no recursion / method invocation).
async fn render_element(conn: &mut jdwp_client::JdwpConnection, value: &jdwp_client::types::Value) -> String {
    use jdwp_client::types::ValueData;
    match &value.data {
        ValueData::Object(0) => "null".to_string(),
        ValueData::Object(id) => {
            if value.tag == 115 {
                if let Ok(s) = conn.get_string_value(*id).await {
                    return format!("\"{}\"", truncate(&s, 60));
                }
            }
            match conn.get_object_reference_type(*id).await {
                Ok(t) => format!(
                    "{} (id=0x{:x})",
                    decode_signature(&conn.get_signature(t).await.unwrap_or_default()),
                    id
                ),
                Err(_) => format!("(object) @{id:x}"),
            }
        }
        _ => value.format(),
    }
}

/// Render one value either shallowly or deeply, depending on whether expansion was requested.
async fn render_one(
    conn: &mut jdwp_client::JdwpConnection,
    value: &jdwp_client::types::Value,
    thread_id: Option<u64>,
    max_len: usize,
    deep: Option<DeepOpts>,
) -> String {
    match deep {
        Some(opts) => render_value_deep(conn, value, thread_id, opts).await,
        None => render_value(conn, value, thread_id, max_len).await,
    }
}

/// Render a value for display. Strings show contents; arrays show their elements; objects
/// show their type name (and, when `thread_id` is Some, a best-effort `toString()`).
async fn render_value(
    conn: &mut jdwp_client::JdwpConnection,
    value: &jdwp_client::types::Value,
    thread_id: Option<u64>,
    max_len: usize,
) -> String {
    use jdwp_client::types::ValueData;
    match &value.data {
        ValueData::Object(0) => "null".to_string(),
        ValueData::Object(id) => render_object(conn, *id, value.tag, thread_id, max_len).await,
        // `ValueData::format_primitive` declines only a reference, and both reference shapes are matched
        // above — so the fallback is unreachable rather than a rendering anyone should see.
        other => other.format_primitive().unwrap_or_else(|| "(?)".to_string()),
    }
}

// ----- deep (recursive) object rendering: OBJ-1 -----

/// Bounds for a deep render, all caller-visible.
///
/// Every one of these exists because the alternative is unbounded work against a live JVM:
/// `max_depth` stops a linked structure from being walked forever, `max_children` stops one wide
/// object from flooding the output, and the node budget in [`DeepState`] caps the *total* cost so a
/// shallow-but-bushy graph can't blow up either.
#[derive(Clone, Copy)]
struct DeepOpts {
    /// How many levels of fields/elements to expand. 0 renders nothing deeply (the shallow form).
    depth_limit: usize,
    /// Fields per object, or elements per array/collection, before "… +N more".
    child_limit: usize,
    /// Max length of a rendered string value.
    text_len: usize,
}

/// Default total nodes one deep render may visit. Reached only by genuinely large graphs; the point
/// is that a pathological object can't hang the tool, and the output says when it was hit.
const DEEP_NODE_BUDGET: usize = 400;

/// Total nodes ONE `get_stack {expand_objects:true}` call may visit, across every frame and local.
///
/// `get_stack` expands many values, not one, so it gets a larger allowance than a single
/// `debug.evaluate` — but it must be *one* allowance for the whole call. Per-value budgets multiply:
/// 20 locals × 20 frames × 400 is ~160k nodes of round trips against a possibly-shared JVM, which is
/// not a cap in any useful sense.
///
/// 1000 rather than something larger because two costs bind, not one: JDWP round trips *and* the size
/// of the reply. A node is roughly a line of output, so a thousand of them is already a reply no
/// caller wants in full — narrowing with `package_filter` / `max_frames` / `max_depth` is the answer,
/// and the exhaustion notice says so.
const STACK_NODE_BUDGET: usize = 1000;

/// Mutable state threaded through one deep render.
struct DeepState {
    /// Nodes left to visit across the whole render.
    budget: usize,
    /// Object ids on the current path, for cycle detection. Path-based (not globally-seen) on
    /// purpose: a value reachable twice by different routes is worth printing twice in a debugger,
    /// but a true cycle (`parent.child.parent`) must not recurse.
    path: Vec<u64>,
}

impl DeepState {
    const fn new(budget: usize) -> Self {
        Self { budget, path: Vec::new() }
    }

    /// Whether the budget ran out during the render(s) so far.
    const fn exhausted(&self) -> bool {
        self.budget == 0
    }
}

/// Deep-render a value: walk instance fields, array elements, and collection contents to a bounded
/// depth, with cycle detection.
///
/// Needs `thread_id` for collections and `toString()` — both require invoking methods in the
/// debuggee, which JDWP only does on a suspended thread. Without one, collections fall back to their
/// type name and only plain fields/arrays expand.
async fn render_value_deep(
    conn: &mut jdwp_client::JdwpConnection,
    value: &jdwp_client::types::Value,
    thread_id: Option<u64>,
    opts: DeepOpts,
) -> String {
    let mut state = DeepState::new(DEEP_NODE_BUDGET);
    let body = render_node(conn, value, thread_id, opts, &mut state, 0).await;
    if state.exhausted() {
        format!("{body}\n… node budget ({DEEP_NODE_BUDGET}) exhausted — raise max_depth only if needed")
    } else {
        body
    }
}

/// Boxed, type-erased recursion entry — an `async fn` cannot name its own future type.
fn render_node_boxed<'a>(
    conn: &'a mut jdwp_client::JdwpConnection,
    value: &'a jdwp_client::types::Value,
    thread_id: Option<u64>,
    opts: DeepOpts,
    state: &'a mut DeepState,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
    Box::pin(render_node(conn, value, thread_id, opts, state, depth))
}

async fn render_node(
    conn: &mut jdwp_client::JdwpConnection,
    value: &jdwp_client::types::Value,
    thread_id: Option<u64>,
    opts: DeepOpts,
    state: &mut DeepState,
    depth: usize,
) -> String {
    if let Some(p) = value.data.format_primitive() {
        return p;
    }
    let jdwp_client::types::ValueData::Object(id) = value.data else {
        return "(?)".to_string();
    };
    if id == 0 {
        return "null".to_string();
    }
    if state.budget == 0 {
        return format!("… (budget exhausted) @0x{id:x}");
    }
    state.budget -= 1;

    // A cycle: this exact object is already an ancestor of itself.
    if state.path.contains(&id) {
        let name = type_name_of(conn, id).await;
        return format!("↩ {name} (id=0x{id:x}, cycle)");
    }

    // A boxed primitive is a leaf, whatever the depth: expanding it would turn a `List<Integer>`
    // into twenty `java.lang.Integer { value = (int) n }` blocks instead of twenty numbers.
    if let Some(unboxed) = render_boxed_primitive(conn, id).await {
        return unboxed;
    }

    // At the depth limit, stop expanding but still say as much as one line can — toString() is the
    // most informative summary available, so this is where it earns its keep.
    if depth >= opts.depth_limit {
        return render_object(conn, id, value.tag, thread_id, opts.text_len).await;
    }

    // Strings and arrays already have good shallow renderings; strings are terminal, arrays recurse.
    if value.tag == 115 {
        if let Ok(s) = conn.get_string_value(id).await {
            return format!("\"{}\"", truncate(&s, opts.text_len));
        }
    }
    let Ok(type_id) = conn.get_object_reference_type(id).await else {
        return format!("(object) @{id:x}");
    };
    let name = decode_signature(&conn.get_signature(type_id).await.unwrap_or_default());
    if name == "java.lang.String" {
        if let Ok(s) = conn.get_string_value(id).await {
            return format!("\"{}\"", truncate(&s, opts.text_len));
        }
    }

    state.path.push(id);
    let rendered = expand_object(conn, id, type_id, &name, value.tag, thread_id, opts, state, depth).await;
    state.path.pop();
    rendered
}

/// Expand one non-string reference: an array, a recognised collection, or a plain object's fields.
#[allow(clippy::too_many_arguments)] // one render step genuinely needs all of this context
async fn expand_object(
    conn: &mut jdwp_client::JdwpConnection,
    id: u64,
    type_id: u64,
    name: &str,
    tag: u8,
    thread_id: Option<u64>,
    opts: DeepOpts,
    state: &mut DeepState,
    depth: usize,
) -> String {
    if tag == 91 {
        return render_array_deep(conn, id, name, thread_id, opts, state, depth).await;
    }
    // Collections need method invocation, so only attempt them with a suspended thread.
    if let Some(tid) = thread_id {
        if let Some(rendered) = render_collection_deep(conn, id, type_id, name, tid, opts, state, depth).await
        {
            return rendered;
        }
    }
    render_fields_deep(conn, id, type_id, name, thread_id, opts, state, depth).await
}

/// Expand a plain object's instance fields (its own and inherited).
#[allow(clippy::too_many_arguments)]
async fn render_fields_deep(
    conn: &mut jdwp_client::JdwpConnection,
    id: u64,
    type_id: u64,
    name: &str,
    thread_id: Option<u64>,
    opts: DeepOpts,
    state: &mut DeepState,
    depth: usize,
) -> String {
    let fields = collect_instance_fields(conn, type_id).await;
    if fields.is_empty() {
        // Nothing to expand — a one-liner beats an empty brace block.
        return render_object(conn, id, 76, thread_id, opts.text_len).await;
    }
    let shown = fields.len().min(opts.child_limit);
    let ids: Vec<u64> = fields.iter().take(shown).map(|f| f.field_id).collect();
    let Ok(values) = conn.get_object_values(id, ids).await else {
        return format!("{name} (id=0x{id:x}, fields unreadable)");
    };

    let pad = indent(depth + 1);
    let mut out = format!("{name} (id=0x{id:x}) {{");
    for (f, v) in fields.iter().take(shown).zip(&values) {
        let rendered = render_node_boxed(conn, v, thread_id, opts, state, depth + 1).await;
        let _ = write!(out, "\n{pad}{} = {rendered}", f.name);
    }
    if fields.len() > shown {
        let _ = write!(out, "\n{pad}… +{} more field(s)", fields.len() - shown);
    }
    let _ = write!(out, "\n{}}}", indent(depth));
    out
}

/// Expand array elements, recursing into each.
async fn render_array_deep(
    conn: &mut jdwp_client::JdwpConnection,
    id: u64,
    name: &str,
    thread_id: Option<u64>,
    opts: DeepOpts,
    state: &mut DeepState,
    depth: usize,
) -> String {
    let Ok(len) = conn.get_array_length(id).await else {
        return format!("{name} (id=0x{id:x}, length unreadable)");
    };
    let base = name.strip_suffix("[]").unwrap_or(name);
    render_indexed_block(conn, &format!("{base}[{len}]"), id, len, thread_id, opts, state, depth)
        .await
        .unwrap_or_else(|| format!("{name} (id=0x{id:x}, elements unreadable)"))
}

/// Indentation for a node at `depth`. Children are drawn at `indent(depth + 1)` and the closing
/// brace at `indent(depth)`, so it lines up under the text that opened the block.
fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

/// Every non-static field of a type, its own first then inherited, in declaration order.
async fn collect_instance_fields(
    conn: &mut jdwp_client::JdwpConnection,
    type_id: u64,
) -> Vec<jdwp_client::reftype::FieldInfo> {
    let mut out = Vec::new();
    let mut current = Some(type_id);
    let mut guard = 0;
    while let Some(tid) = current {
        guard += 1;
        if guard > 50 {
            break;
        }
        match conn.get_fields(tid).await {
            Ok(fields) => out.extend(fields.into_iter().filter(|f| (f.mod_bits & ACC_STATIC) == 0)),
            Err(_) => break,
        }
        current = conn.get_superclass(tid).await.unwrap_or(None);
    }
    out
}

/// What kind of container an object turned out to be, for element-level rendering.
///
/// Deliberately NOT memoised per type, though the verdict is a pure function of one: measured, it is
/// free. See "Caching the container classification" in `docs/VARIABLE_INSPECTION_PLAN.md`.
enum ContainerKind {
    /// Anything with `toArray()` + `size()` — `List`, `Set`, `Queue`, …
    Collection,
    /// Anything with `entrySet()` + `size()`.
    Map,
    /// `java.util.Optional` exactly.
    Optional,
}

/// Classify an object as a container by looking for distinctive methods rather than by checking
/// interfaces.
///
/// This is duck typing, and deliberately so: deciding "is this a `java.util.Map`?" properly means
/// walking the *transitive* interface hierarchy (`ReferenceType.Interfaces` returns only direct
/// superinterfaces), which is many round trips per object rendered. The concrete classes that matter
/// — `ArrayList`, `HashMap`, `HashSet`, … — declare these methods themselves or inherit them from an
/// abstract base the superclass walk already covers.
///
/// The cost of a false positive is bounded: a non-collection class that happens to have `toArray()`
/// and `size()` gets rendered element-wise, which is odd but not wrong, and never unsafe.
async fn classify_container(
    conn: &mut jdwp_client::JdwpConnection,
    type_id: u64,
    name: &str,
) -> Option<ContainerKind> {
    if name == "java.util.Optional" {
        return Some(ContainerKind::Optional);
    }
    // `size()I` is the cheap discriminator: bail before the more expensive lookups without it.
    let has_size = find_method_arity(conn, type_id, "size", 0)
        .await
        .ok()
        .flatten()
        .is_some_and(|(_, m)| m.signature == "()I");
    if !has_size {
        return None;
    }
    if find_method_arity(conn, type_id, "entrySet", 0).await.ok().flatten().is_some() {
        return Some(ContainerKind::Map);
    }
    if find_method_arity(conn, type_id, "toArray", 0)
        .await
        .ok()
        .flatten()
        .is_some_and(|(_, m)| m.signature == "()[Ljava/lang/Object;")
    {
        return Some(ContainerKind::Collection);
    }
    None
}

/// Element-level rendering for a collection, map, or `Optional`. `None` when the object isn't one (or
/// its contents can't be read), so the caller falls back to field expansion.
#[allow(clippy::too_many_arguments)]
async fn render_collection_deep(
    conn: &mut jdwp_client::JdwpConnection,
    id: u64,
    type_id: u64,
    name: &str,
    tid: u64,
    opts: DeepOpts,
    state: &mut DeepState,
    depth: usize,
) -> Option<String> {
    match classify_container(conn, type_id, name).await? {
        ContainerKind::Optional => render_optional_deep(conn, id, type_id, tid, opts, state, depth).await,
        ContainerKind::Collection => {
            render_elements_deep(conn, id, type_id, name, tid, opts, state, depth).await
        }
        ContainerKind::Map => render_map_deep(conn, id, type_id, name, tid, opts, state, depth).await,
    }
}

/// `Optional[value]` or `Optional.empty`.
async fn render_optional_deep(
    conn: &mut jdwp_client::JdwpConnection,
    id: u64,
    type_id: u64,
    tid: u64,
    opts: DeepOpts,
    state: &mut DeepState,
    depth: usize,
) -> Option<String> {
    // isPresent() first: get() on an empty Optional throws, and a thrown call is indistinguishable
    // here from a broken one.
    let present = matches!(
        invoke_no_arg(conn, id, type_id, tid, "isPresent").await,
        Some(jdwp_client::types::Value { data: jdwp_client::types::ValueData::Boolean(true), .. })
    );
    if !present {
        return Some("Optional.empty".to_string());
    }
    let v = invoke_no_arg(conn, id, type_id, tid, "get").await?;
    let rendered = render_node_boxed(conn, &v, Some(tid), opts, state, depth + 1).await;
    Some(format!("Optional[{rendered}]"))
}

/// A `Collection`'s elements, reached through `toArray()`.
#[allow(clippy::too_many_arguments)]
async fn render_elements_deep(
    conn: &mut jdwp_client::JdwpConnection,
    id: u64,
    type_id: u64,
    name: &str,
    tid: u64,
    opts: DeepOpts,
    state: &mut DeepState,
    depth: usize,
) -> Option<String> {
    let arr = as_object_id(&invoke_no_arg(conn, id, type_id, tid, "toArray").await?)?;
    let len = conn.get_array_length(arr).await.ok()?;
    render_indexed_block(conn, &format!("{name}[{len}]"), arr, len, Some(tid), opts, state, depth).await
}

/// A `Map`'s entries as `key → value` lines.
#[allow(clippy::too_many_arguments)]
async fn render_map_deep(
    conn: &mut jdwp_client::JdwpConnection,
    id: u64,
    type_id: u64,
    name: &str,
    tid: u64,
    opts: DeepOpts,
    state: &mut DeepState,
    depth: usize,
) -> Option<String> {
    // entrySet() then toArray() on that set: two invocations to reach an indexable array of
    // Map.Entry, then getKey()/getValue() per entry — which is why `child_limit` matters here.
    let set = as_object_id(&invoke_no_arg(conn, id, type_id, tid, "entrySet").await?)?;
    let set_type = conn.get_object_reference_type(set).await.ok()?;
    let arr = as_object_id(&invoke_no_arg(conn, set, set_type, tid, "toArray").await?)?;
    let len = conn.get_array_length(arr).await.ok()?;
    if len == 0 {
        return Some(format!("{name}{{}} (0 entries)"));
    }
    let take = len.min(i32::try_from(opts.child_limit).unwrap_or(i32::MAX));
    let entries = conn.get_array_values(arr, 0, take).await.ok()?;

    let pad = indent(depth + 1);
    let mut out = format!("{name}({len} entries) {{");
    for e in &entries {
        let Some((k, v)) = entry_pair(conn, e, tid).await else { continue };
        let kr = render_node_boxed(conn, &k, Some(tid), opts, state, depth + 1).await;
        let vr = render_node_boxed(conn, &v, Some(tid), opts, state, depth + 1).await;
        let _ = write!(out, "\n{pad}{kr} → {vr}");
    }
    if len > take {
        let _ = write!(out, "\n{pad}… +{} more entr(ies)", len - take);
    }
    let _ = write!(out, "\n{}}}", indent(depth));
    Some(out)
}

/// Read one `Map.Entry`'s key and value. `None` if either call fails, so the entry is skipped rather
/// than aborting the whole map.
async fn entry_pair(
    conn: &mut jdwp_client::JdwpConnection,
    entry_value: &jdwp_client::types::Value,
    tid: u64,
) -> Option<(jdwp_client::types::Value, jdwp_client::types::Value)> {
    let entry = as_object_id(entry_value)?;
    let etype = conn.get_object_reference_type(entry).await.ok()?;
    let k = invoke_no_arg(conn, entry, etype, tid, "getKey").await?;
    let v = invoke_no_arg(conn, entry, etype, tid, "getValue").await?;
    Some((k, v))
}

/// Render `len` elements of the array `arr` as an indented `{ [i] = … }` block under `header`,
/// honouring `child_limit`. Shared by real arrays and by collections (which reach an array via
/// `toArray()`), so both truncate and indent identically.
#[allow(clippy::too_many_arguments)]
async fn render_indexed_block(
    conn: &mut jdwp_client::JdwpConnection,
    header: &str,
    arr: u64,
    len: i32,
    thread_id: Option<u64>,
    opts: DeepOpts,
    state: &mut DeepState,
    depth: usize,
) -> Option<String> {
    if len == 0 {
        return Some(format!("{header} {{}}"));
    }
    let take = len.min(i32::try_from(opts.child_limit).unwrap_or(i32::MAX));
    let elems = conn.get_array_values(arr, 0, take).await.ok()?;
    let pad = indent(depth + 1);
    let mut out = format!("{header} {{");
    for (i, e) in elems.iter().enumerate() {
        let rendered = render_node_boxed(conn, e, thread_id, opts, state, depth + 1).await;
        let _ = write!(out, "\n{pad}[{i}] = {rendered}");
    }
    if len > take {
        let _ = write!(out, "\n{pad}… +{} more", len - take);
    }
    let _ = write!(out, "\n{}}}", indent(depth));
    Some(out)
}

/// Invoke a no-arg method by name, returning its value. `None` if there is no such method, the call
/// fails, or it throws — every caller treats all three as "can't expand this, fall back".
async fn invoke_no_arg(
    conn: &mut jdwp_client::JdwpConnection,
    id: u64,
    type_id: u64,
    tid: u64,
    method: &str,
) -> Option<jdwp_client::types::Value> {
    let (decl, m) = find_method_arity(conn, type_id, method, 0).await.ok()??;
    let (ret, exc) = conn.invoke_method(id, tid, decl, m.method_id, vec![]).await.ok()?;
    (exc == 0).then_some(ret)
}

/// The non-null object id in a value, if it is one.
const fn as_object_id(v: &jdwp_client::types::Value) -> Option<u64> {
    match v.data {
        jdwp_client::types::ValueData::Object(0) => None,
        jdwp_client::types::ValueData::Object(id) => Some(id),
        _ => None,
    }
}

/// The `java.lang.*` primitive wrappers. Each holds exactly one private `value` field, so reading
/// that field is both the whole content and the useful rendering.
const BOXED_PRIMITIVES: [&str; 8] = [
    "java.lang.Integer",
    "java.lang.Long",
    "java.lang.Short",
    "java.lang.Byte",
    "java.lang.Character",
    "java.lang.Boolean",
    "java.lang.Float",
    "java.lang.Double",
];

/// If `id` is a boxed primitive, render the primitive it holds. `None` for anything else, or if the
/// `value` field can't be read (in which case the caller renders it as an ordinary object).
async fn render_boxed_primitive(conn: &mut jdwp_client::JdwpConnection, id: u64) -> Option<String> {
    let type_id = conn.get_object_reference_type(id).await.ok()?;
    let name = decode_signature(&conn.get_signature(type_id).await.unwrap_or_default());
    if !BOXED_PRIMITIVES.contains(&name.as_str()) {
        return None;
    }
    let (_, f) = find_field_info(conn, type_id, "value", Some(false)).await.ok()??;
    let v = conn.get_object_values(id, vec![f.field_id]).await.ok()?.into_iter().next()?;
    v.data.format_primitive()
}

/// The type name of a live object, or a placeholder if it can't be read.
async fn type_name_of(conn: &mut jdwp_client::JdwpConnection, id: u64) -> String {
    match conn.get_object_reference_type(id).await {
        Ok(t) => decode_signature(&conn.get_signature(t).await.unwrap_or_default()),
        Err(_) => "object".to_string(),
    }
}

/// Render an object value: strings show contents; arrays show their elements; other objects show
/// their type name (and, when `thread_id` is Some, a best-effort `toString()`).
async fn render_object(
    conn: &mut jdwp_client::JdwpConnection,
    id: u64,
    tag: u8,
    thread_id: Option<u64>,
    max_len: usize,
) -> String {
    if tag == 115 {
        if let Ok(s) = conn.get_string_value(id).await {
            return format!("\"{}\"", truncate(&s, max_len));
        }
    }
    let Ok(type_id) = conn.get_object_reference_type(id).await else {
        return format!("(object) @{id:x}");
    };
    let name = decode_signature(&conn.get_signature(type_id).await.unwrap_or_default());
    if name == "java.lang.String" {
        if let Ok(s) = conn.get_string_value(id).await {
            return format!("\"{}\"", truncate(&s, max_len));
        }
    }
    // Array contents
    if tag == 91 {
        if let Some(rendered) = render_array(conn, id, &name).await {
            return rendered;
        }
    }
    // A boxed primitive reads better as the value it holds than as `java.lang.Integer "2"`, and this
    // needs no thread — unlike the toString() below. (render_node checks this too, so a boxed value
    // stays a leaf there regardless of depth rather than being expanded into a `value` field.)
    if let Some(unboxed) = render_boxed_primitive(conn, id).await {
        return unboxed;
    }
    // best-effort toString() when we have a thread to run it on
    if let Some(tid) = thread_id {
        match render_via_tostring(conn, id, type_id, tid, &name, max_len).await {
            ToStringOutcome::Rendered(rendered) => return rendered,
            // Say so. Before this the reply was byte-identical to the free shallow render below, so a
            // caller had no way to know the VM had just been frozen for the whole budget (EVAL-5).
            ToStringOutcome::TimedOut(ms) => {
                return format!(
                    "{name} (id=0x{id:x}) ⚠️ toString() did not return within {ms}ms — value not rendered.                      JDWP cannot cancel an invocation, so that thread is STILL executing it and its frames                      are unreadable until it finishes or you debug.continue. Use expand_objects:true                      instead, which reads fields and invokes nothing."
                );
            }
            ToStringOutcome::Unavailable => {}
        }
    }
    format!("{name} (id=0x{id:x})")
}

/// Render up to 16 elements of an array object; `None` if its length/values can't be read.
async fn render_array(conn: &mut jdwp_client::JdwpConnection, id: u64, name: &str) -> Option<String> {
    let len = conn.get_array_length(id).await.ok()?;
    let take = len.min(16);
    let elems = conn.get_array_values(id, 0, take).await.ok()?;
    let mut parts = Vec::with_capacity(elems.len());
    for e in &elems {
        parts.push(render_element(conn, e).await);
    }
    let more = if len > take { format!(", … +{} more", len - take) } else { String::new() };
    let base = name.strip_suffix("[]").unwrap_or(name);
    Some(format!("{}[{}]{{{}{}}}", base, len, parts.join(", "), more))
}

/// Best-effort `toString()` render (0-arg, returning String); `None` if unavailable or it throws.
async fn render_via_tostring(
    conn: &mut jdwp_client::JdwpConnection,
    id: u64,
    type_id: u64,
    tid: u64,
    name: &str,
    max_len: usize,
) -> ToStringOutcome {
    let Ok(Some((decl, m))) = find_method_arity(conn, type_id, "toString", 0).await else {
        return ToStringOutcome::Unavailable;
    };
    if m.signature != "()Ljava/lang/String;" {
        return ToStringOutcome::Unavailable;
    }
    let (ret, exc) = match conn.invoke_method(id, tid, decl, m.method_id, vec![]).await {
        Ok(pair) => pair,
        // EVAL-5: a budget expiry is not the same as "this type has no toString". Reporting them the same
        // way is what made a 40-second freeze indistinguishable from a free shallow render.
        Err(jdwp_client::JdwpError::InvokeTimeout(ms)) => return ToStringOutcome::TimedOut(ms),
        Err(_) => return ToStringOutcome::Unavailable,
    };
    if exc != 0 {
        return ToStringOutcome::Unavailable;
    }
    let jdwp_client::types::ValueData::Object(sid) = ret.data else {
        return ToStringOutcome::Unavailable;
    };
    if sid == 0 {
        return ToStringOutcome::Unavailable;
    }
    conn.get_string_value(sid).await.map_or(ToStringOutcome::Unavailable, |s| {
        ToStringOutcome::Rendered(format!("{} \"{}\"", name, truncate(&s, max_len)))
    })
}

/// What happened when a value's `toString()` was tried (EVAL-5).
///
/// Three outcomes, and the middle one is the whole point of this enum: a value whose `toString()` blew the
/// invocation budget must not render identically to one that never had a `toString()` to call.
enum ToStringOutcome {
    Rendered(String),
    /// The invocation budget expired — the debuggee thread is very likely blocked on a monitor held by
    /// another suspended thread, and is still blocked now.
    TimedOut(u64),
    /// No usable `toString()`, or it threw. Nothing was spent worth reporting.
    Unavailable,
}

/// Convert a literal string to a Value, coercing int literals to the slot's primitive type.
async fn literal_to_value(
    conn: &mut jdwp_client::JdwpConnection,
    s: &str,
    sig_byte: u8,
) -> Result<jdwp_client::types::Value, String> {
    Ok(match parse_lit(s)? {
        ArgLit::Str(st) => {
            let id = conn.create_string(&st).await.map_err(|e| format!("Failed to create string: {e}"))?;
            value_object(id)
        }
        ArgLit::Null => value_null(),
        ArgLit::Bool(b) => value_bool(b),
        ArgLit::Long(n) => value_long(n),
        // `debug.set_value` writes a literal only — copying another live value would need the
        // caller's frame, which this coercion path (also used for deferred writes) doesn't have.
        ArgLit::Expr(e) => {
            return Err(format!(
                "'{e}' is not a literal — set_value takes a literal (int, 123L, true/false, null, \"string\")"
            ))
        }
        // Assigning an integer literal to a narrower Java primitive performs Java's own
        // narrowing conversion (`(byte)`, `(short)`, `(char)`, `(float)`) — a deliberate,
        // possibly-lossy reinterpretation, exactly as `javac` would compile it.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
        ArgLit::Int(n) => match sig_byte {
            b'J' => value_long(i64::from(n)),
            b'Z' => value_bool(n != 0),
            b'B' => jdwp_client::types::Value { tag: 66, data: jdwp_client::types::ValueData::Byte(n as i8) },
            b'S' => {
                jdwp_client::types::Value { tag: 83, data: jdwp_client::types::ValueData::Short(n as i16) }
            }
            b'C' => {
                jdwp_client::types::Value { tag: 67, data: jdwp_client::types::ValueData::Char(n as u16) }
            }
            b'F' => {
                jdwp_client::types::Value { tag: 70, data: jdwp_client::types::ValueData::Float(n as f32) }
            }
            b'D' => jdwp_client::types::Value {
                tag: 68,
                data: jdwp_client::types::ValueData::Double(f64::from(n)),
            },
            _ => value_int(n),
        },
    })
}

/// Resolve the value a `set_value` write should store (SETF-2): a literal coerced to `declared_sig`
/// (the existing path), OR a live expression whose value is copied by reference.
///
/// An expression's runtime type is validated against the target's declared reference type — the
/// EVAL-3 assignability check, interfaces included — and a mismatch is refused, because a reference
/// store is exactly what the JVM does NOT type-check for you (see the EVAL-3 SIGSEGV note): writing a
/// value of the wrong type would corrupt the field silently. Primitive targets defer to the caller's
/// existing `tag_compatible` guard.
async fn value_to_write(
    conn: &mut jdwp_client::JdwpConnection,
    thread_opt: Option<u64>,
    frame_index: usize,
    value_str: &str,
    declared_sig: &str,
) -> Result<jdwp_client::types::Value, String> {
    let sig_byte = declared_sig.as_bytes().first().copied().unwrap_or(b'L');
    match parse_lit(value_str.trim())? {
        ArgLit::Expr(e) => {
            let tid = thread_opt.ok_or_else(|| {
                format!(
                "Copying the live value '{e}' needs a suspended thread — pause one or hit a breakpoint first"
            )
            })?;
            let frame = conn
                .get_frames(tid, 0, -1)
                .await
                .ok()
                .and_then(|f| f.get(frame_index).cloned().or_else(|| f.first().cloned()));
            let v = resolve_expression(conn, Some(tid), frame.as_ref(), &e).await?;
            validate_ref_assignable(conn, declared_sig, &v).await?;
            Ok(v)
        }
        _ => literal_to_value(conn, value_str, sig_byte).await,
    }
}

/// Refuse an expression-sourced write whose runtime type isn't assignable to a reference target
/// (SETF-2). A primitive target returns `Ok` and leaves the check to the caller's `tag_compatible`;
/// `null` fits any reference; an array target accepts any array; otherwise the source's runtime type
/// must be the target type, a subtype, or an implementer (`implements_interface` answers all three).
async fn validate_ref_assignable(
    conn: &mut jdwp_client::JdwpConnection,
    declared_sig: &str,
    v: &jdwp_client::types::Value,
) -> Result<(), String> {
    if !(declared_sig.starts_with('L') || declared_sig.starts_with('[')) {
        return Ok(()); // primitive target — the caller's tag_compatible guard applies
    }
    match v.data {
        jdwp_client::types::ValueData::Object(0) => Ok(()), // null fits any reference
        jdwp_client::types::ValueData::Object(id) => {
            let rt = conn
                .get_object_reference_type(id)
                .await
                .map_err(|e| format!("Failed to resolve the source value's type: {e}"))?;
            let ok = if declared_sig.starts_with('[') {
                conn.get_signature(rt).await.is_ok_and(|s| s.starts_with('['))
            } else {
                conn.implements_interface(rt, declared_sig).await.unwrap_or(false)
            };
            if ok {
                Ok(())
            } else {
                let actual = decode_signature(&conn.get_signature(rt).await.unwrap_or_default());
                Err(format!(
                    "Type mismatch: the source is {actual}, but the target is {} — a reference of the wrong type is refused, because the JVM would not catch it.",
                    decode_signature(declared_sig)
                ))
            }
        }
        _ => Err(format!(
            "The target is {} (a reference), but the source resolved to a primitive.",
            decode_signature(declared_sig)
        )),
    }
}

// ----- event / thread / location helpers -----

fn event_location(d: &EventKind) -> Option<(u64, Location)> {
    match d {
        EventKind::Breakpoint { thread, location }
        | EventKind::Step { thread, location }
        | EventKind::MethodExit { thread, location, .. }
        | EventKind::Exception { thread, location, .. } => Some((*thread, location.clone())),
        EventKind::FieldAccess { field } | EventKind::FieldModification { field, .. } => {
            Some((field.thread, field.location.clone()))
        }
        _ => None,
    }
}

fn event_thread(es: &jdwp_client::EventSet) -> Option<u64> {
    es.events.first().and_then(|e| event_location(&e.details).map(|(t, _)| t))
}

fn event_suspends(es: &jdwp_client::EventSet) -> bool {
    es.suspend_policy != 0
        && es.events.iter().any(|e| {
            matches!(
                e.details,
                EventKind::Breakpoint { .. }
                    | EventKind::Step { .. }
                    | EventKind::Exception { .. }
                    | EventKind::MethodExit { .. }
                    | EventKind::FieldAccess { .. }
                    | EventKind::FieldModification { .. }
            )
        })
}

const fn event_type_name(d: &EventKind) -> &'static str {
    match d {
        EventKind::Breakpoint { .. } => "breakpoint",
        EventKind::Step { .. } => "step",
        EventKind::Exception { .. } => "exception",
        EventKind::MethodExit { .. } => "method_exit",
        EventKind::VMStart { .. } => "vm_start",
        EventKind::VMDeath => "vm_death",
        EventKind::ThreadStart { .. } => "thread_start",
        EventKind::ThreadDeath { .. } => "thread_death",
        EventKind::ClassPrepare { .. } => "class_prepare",
        EventKind::FieldAccess { .. } => "field_access",
        EventKind::FieldModification { .. } => "field_modification",
        EventKind::Unknown { .. } => "unknown",
    }
}

/// Emit `get_stack`'s collapsed "hidden frames" marker (from `package_filter`) and reset the counter.
fn flush_hidden(output: &mut String, hidden: &mut usize) {
    if *hidden > 0 {
        let _ = writeln!(output, "   … {} frame(s) hidden", *hidden);
        *hidden = 0;
    }
}

/// One `debug.list_threads` row: the id, the name, and the status label — the last only when
/// `only_suspended` made us read it.
type ThreadRow = (u64, String, Option<String>);

/// What a `debug.list_threads` call read, and how it chose the rows it kept.
struct ThreadListing {
    /// The kept rows, in creation order — selection and presentation are separate jobs (ADR-0013).
    rows: Vec<ThreadRow>,
    /// Which threads the `limit` was spent on and which groups it passed over, so the reply can say.
    selection: FamilySelection,
}

/// Collect the rows for `debug.list_threads`, choosing them by name family rather than by the order the
/// JVM listed them in (DUMP-5, #51).
///
/// **It used to stop at `limit` while walking `AllThreads`, and that is creation order.** The JVM's own
/// threads exist first, the container's next, and the request pool a caller came to look at exists
/// **last**, because an application server does not start it until everything it depends on is up. On a
/// real `WildFly` at 267 threads the first 40 in that order contained *zero* application threads (#24).
/// `debug.thread_dump` was fixed in #43 and this tool was left doing the old thing, which is worse than
/// it sounds: `list_threads` is the *cheap reconnaissance call*, the one you run to decide what to dump,
/// so being systematically wrong here aims the expensive call at the wrong threads. It is ADR-0013's rule,
/// applied by ADR-0013's code — `family_round_robin`, `candidates_by_family`, `family_order_note` — rather
/// than a second rule of its own, because two truncation rules across one tool surface would be worse
/// than the bug.
///
/// **The cost, which is the reason this was ever in doubt.** Choosing needs every thread's name, so this
/// reads `threads` names where the old loop read `limit` of them: 268 packets rather than 41 on that
/// `WildFly`, ~6.5×. It stays the cheap call all the same — a name is *one* packet where a dump's row is
/// ~8, so the whole 267-thread listing costs a third of what the same JVM's truncated default dump does
/// (~790, ADR-0013), and the reply prints the figure so nobody has to take that on trust. Nothing is paid
/// on the shapes that never truncate: a listing whose `limit` covers the JVM, or one already narrowed by
/// `name_filter`, reads exactly the names the old loop read, thread for thread.
///
/// Unlike the dump this holds no suspension, so there is no budget on the pass: the only thing it spends
/// is the caller's own latency, and it is linear in a number the reply states.
async fn collect_thread_rows(
    conn: &mut jdwp_client::JdwpConnection,
    all: &[u64],
    limit: usize,
    name_filter: Option<&str>,
    only_suspended: bool,
) -> ThreadListing {
    // Every thread that could take a slot, in creation order. The `status` read is skipped unless
    // `only_suspended` asked for it — one packet per thread rather than the dump's two, because a
    // listing shows no status it did not already have to fetch to filter on.
    let mut candidates: Vec<ThreadRow> = Vec::new();
    for tid in all {
        let name = conn.get_thread_name(*tid).await.unwrap_or_default();
        if let Some(f) = name_filter {
            if !name.to_lowercase().contains(f) {
                continue;
            }
        }
        let status = if only_suspended {
            match conn.get_thread_status(*tid).await {
                Ok((ts, ss)) => {
                    if ss == 0 {
                        continue; // not suspended
                    }
                    Some(thread_status_name(ts).to_string())
                }
                // The read failing is how a thread that died under us announces itself. Skipped rather
                // than shown as a row of unknowns, exactly as the dump's triage does.
                Err(_) => continue,
            }
        } else {
            None
        };
        candidates.push((*tid, name, status));
    }

    let eligible = candidates.len();
    let tally = candidates_by_family(candidates.iter().map(|(_, n, _)| n.as_str()));
    let (order, families) = {
        let names: Vec<&str> = candidates.iter().map(|(_, n, _)| n.as_str()).collect();
        family_round_robin(&names)
    };
    // Chosen by family, then sorted back into the order the JVM listed them: the caller asked what threads
    // this JVM has, not what order the debugger decided to ask in, and an untruncated listing is therefore
    // byte-for-byte what it always was.
    let mut picked: Vec<usize> = order.into_iter().take(limit).collect();
    picked.sort_unstable();
    let rows: Vec<ThreadRow> = picked
        .into_iter()
        .filter_map(|i| candidates.get_mut(i).map(|c| (c.0, std::mem::take(&mut c.1), c.2.take())))
        .collect();

    let withheld = withheld_by_family(tally, rows.iter().map(|(_, n, _)| n.as_str()));
    ThreadListing { rows, selection: FamilySelection { eligible, families, withheld } }
}

/// One thread's entry in a `debug.thread_dump` (DUMP-1).
///
/// `stack` is a three-state `DumpStack` rather than a `Vec` because "no frames", "not readable" and
/// "not asked for" are three different answers on a wedged JVM, and collapsing any pair of them would
/// make a thread look idle when it is not.
struct DumpRow {
    id: u64,
    name: String,
    /// Short `threadStatus` label (`running` / `monitor` / `wait` / …).
    status: &'static str,
    /// Whether the debugger currently holds this thread suspended (`suspendStatus` != 0).
    suspended: bool,
    /// The JVM reported `ZOMBIE` — this thread has already run to completion. Independent of
    /// `suspended`, and the reason the header must not count this row among the ones `suspend:true`
    /// would rescue (DUMP-4, #47).
    finished: bool,
    /// The frames, why they couldn't be read, or that they were never requested.
    stack: DumpStack,
    /// How many frames were dropped by `max_frames` / `package_filter`.
    frames_hidden: usize,
    /// Monitors this thread holds, as `(rendered, object id)`.
    holds: Vec<(String, u64)>,
    /// The monitor it is blocked entering, if any.
    waiting_on: Option<(String, u64)>,
    /// Set when monitors were asked for but the read failed on this thread.
    monitor_note: Option<String>,
}

/// What a dump row has to say about one thread's frames.
///
/// Three states, not two. `monitors_only` (#17) deliberately reads no frames, and rendering that the
/// same way as a failed read would report a healthy VM as unreadable — while rendering it as an empty
/// stack would report every thread as idle. Both are worse than saying nothing was asked for.
enum DumpStack {
    /// Frames read. Empty is a real answer: a thread genuinely can have no frames.
    Frames(Vec<String>),
    /// Frames could not be read, and why — including the running-thread case, which is JDWP's rule
    /// rather than a fault.
    Unreadable(String),
    /// Frames were deliberately not requested (`monitors_only`). Stated once in the header, not per row.
    Omitted,
}

/// What a dump collected, and what the suspension budget stopped it from collecting (#17).
struct DumpOutcome {
    rows: Vec<DumpRow>,
    /// Matching threads left unread because the budget expired. `0` means the dump is complete.
    unread: usize,
    /// Threads that stopped existing while the dump was reading the list (DUMP-4, #47). Reported
    /// apart from `unread` and apart from the `limit`, because it is the one shortfall a caller can do
    /// nothing about.
    vanished: usize,
    /// How the `limit` was spent, for the header to state (DUMP-3).
    selection: FamilySelection,
}

/// How a reply chose which threads to spend its `limit` on, so it can say (DUMP-3, #43; DUMP-5, #51).
///
/// Carried out of the collection rather than recomputed at render time: the rows that survived are not
/// enough to reconstruct what was passed over, and "what am I NOT seeing" is the question a truncated
/// reply has to answer. A header that says `40/267 thread(s)` and nothing else reads as a sample.
///
/// Shared by `debug.thread_dump` and `debug.list_threads` on purpose, and it is the same struct rather
/// than two similar ones because the two tools must not be able to drift apart: a caller runs the cheap
/// list to decide what to dump, and if the list's population were chosen by a different rule than the
/// dump's, the reconnaissance would send the expensive call after the wrong threads (#51).
struct FamilySelection {
    /// Threads that passed `name_filter` and were therefore in the running for a slot.
    eligible: usize,
    /// Distinct name families among them — the number of independent things the pool is made of.
    families: usize,
    /// Per family, how many eligible threads never made it into the reply. Biggest first.
    withheld: Vec<(String, usize)>,
}

/// A thread name with every run of digits collapsed to `#` — the shape a pool's threads share.
///
/// `default task-17` and `default task-91` become one family; `default I/O-3` stays another. Crude on
/// purpose: the alternative is a vocabulary of framework thread names, which is guessing at somebody
/// else's naming convention and goes stale the first time they change it. Numbering the workers is the
/// one thing every pool in every framework actually does.
fn thread_name_family(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut in_digits = false;
    for c in name.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
            }
            in_digits = true;
        } else {
            in_digits = false;
            out.push(c);
        }
    }
    out
}

/// The order a dump reads its candidates in: one thread from each name family before a second from any.
///
/// Returns indices into `names`, plus how many families there were. Round-robin over the families in
/// first-appearance order, each family internally still in creation order — so the result is a
/// deterministic function of the thread list, and a dump taken twice against a still debuggee reads the
/// same threads.
///
/// This is the DUMP-3 fix in one function. Creation order is not an arbitrary slice of a pool, it is a
/// *biased* one: the JVM's own threads exist first, the container's next, and the request workers a
/// caller came to look at exist last. Round-robin refuses to let any one family spend the whole `limit`,
/// which is the only property needed — 40 slots across ~25 families reaches every family, including the
/// 13-thread one that mattered, instead of being eaten by 16 Undertow selectors and 8 MSC service threads.
fn family_round_robin(names: &[&str]) -> (Vec<usize>, usize) {
    let mut families: Vec<Vec<usize>> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, n) in names.iter().enumerate() {
        let key = thread_name_family(n);
        if let Some(members) = index.get(&key).and_then(|f| families.get_mut(*f)) {
            members.push(i);
        } else {
            index.insert(key, families.len());
            families.push(vec![i]);
        }
    }
    let deepest = families.iter().map(Vec::len).max().unwrap_or(0);
    let mut order = Vec::with_capacity(names.len());
    for round in 0..deepest {
        for f in &families {
            if let Some(i) = f.get(round) {
                order.push(*i);
            }
        }
    }
    (order, families.len())
}

/// A thread that survived the triage pass: where `AllThreads` listed it, its id, and everything the two
/// cheap per-thread reads already answered.
///
/// The creation position is kept because the rows are *rendered* in it. Selection is by family and
/// presentation is by creation order deliberately — the caller asked "what is this JVM doing", not "what
/// order did the debugger decide to ask in", and a stable presentation means the only thing DUMP-3
/// changed about an untruncated dump is nothing at all.
struct DumpCandidate {
    seen: usize,
    tid: u64,
    name: String,
    status: &'static str,
    suspended: bool,
    /// The JVM answered `ZOMBIE`: this thread has run to completion. Carried as a flag rather than
    /// re-derived from `status` because it changes what the row is allowed to *say* — see
    /// `unreadable_reason` (DUMP-4, #47).
    finished: bool,
}

/// What the triage pass learned about a thread list that is already out of date.
///
/// Three outcomes, not two, and the third is the one DUMP-4 (#47) is about. A thread can be a candidate,
/// it can be one the budget never reached, or it can have **stopped existing** between `AllThreads` and
/// the question we asked about it. Collapsing the last two loses the only thing a caller can act on: an
/// unexamined thread is worth another dump, and a vanished one is not.
struct DumpTriage {
    candidates: Vec<DumpCandidate>,
    /// Threads the triage pass ran out of its share of the suspension budget before reaching.
    untriaged: usize,
    /// Threads whose id was live when the JVM listed it and invalid by the time we asked — a retiring
    /// pool worker, collected before the read got to it. A JDWP thread id is a weak reference, so on a
    /// real request pool this is the normal path rather than the exotic one.
    vanished: usize,
}

/// Read every thread's name and status before deciding which ones to read *properly* (DUMP-3, #43).
///
/// ADR-0008's shape — fetch wide, then truncate deliberately — applied to threads instead of frames. Both
/// reads are flat single round trips with no per-frame lookups behind them, ~2 packets against the ~8 a
/// full row costs, and they are the only per-thread data a ranking is allowed to use: #43 rules out
/// anything needing an *extra* round trip per thread to decide, because that would cost the very thing
/// the ordering is trying to save.
///
/// **Both reads happen here, together, rather than the name here and the status when the row is built.**
/// Name-only triage is the cheaper shape and it was the first one written — but it puts the whole first
/// pass between `AllThreads` and every status read, and on a pool that turns over several times a second
/// that is the difference between a thread that has *died* and one that has died **and been collected**.
/// TEST-10's churning-pool test caught it immediately: across three runs of twelve dumps it could no
/// longer observe a single `[zombie]` row, because every thread in the snapshot was already gone by the
/// time the second pass reached it. Two packets per thread buys data that is about the JVM the caller
/// asked about. See ADR-0013.
///
/// **The pass gets at most half the remaining budget.** A dump that spent its entire suspension window
/// deciding what to read and then read nothing would be a worse answer than the bug it fixes, and on a
/// slow wire that is exactly what an unbounded first pass would do. Threads it never reached are returned
/// as a count, never dropped silently.
///
/// **So are the threads that stopped existing** (DUMP-4, #47). The status read failing is how a thread
/// that died under the dump announces itself, and until this counted them the rows they cost were
/// indistinguishable from rows `limit` withheld — see `render_thread_dump`'s footer for why that
/// mattered.
async fn triage_dump_threads(
    conn: &mut jdwp_client::JdwpConnection,
    all: &[u64],
    a: &crate::args::ThreadDumpArgs,
    name_filter: Option<&str>,
    deadline: Option<std::time::Instant>,
) -> DumpTriage {
    let now = std::time::Instant::now();
    let triage_deadline = deadline.map(|d| now + d.saturating_duration_since(now) / 2);
    let mut candidates = Vec::new();
    let mut vanished = 0usize;
    for (seen, tid) in all.iter().enumerate() {
        if triage_deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            return DumpTriage { candidates, untriaged: all.len() - seen, vanished };
        }
        let name = conn.get_thread_name(*tid).await.unwrap_or_default();
        if name_filter.is_some_and(|f| !name.to_lowercase().contains(f)) {
            continue;
        }
        // A thread that can't report its status has almost certainly died; skip it rather than showing a
        // row of unknowns — but COUNT it, because the reply has to say what became of the difference, and
        // "the caller's limit" was the wrong answer (DUMP-4, #47).
        let Ok((ts, ss)) = conn.get_thread_status(*tid).await else {
            vanished += 1;
            continue;
        };
        let (status, suspended, finished) = (thread_status_name(ts), ss != 0, ts == THREAD_STATUS_ZOMBIE);
        // Applied here rather than when the row is built, so the `limit` is spent on threads that are
        // actually readable instead of on slots that turn out empty.
        if a.only_suspended && !suspended {
            continue;
        }
        candidates.push(DumpCandidate { seen, tid: *tid, name, status, suspended, finished });
    }
    DumpTriage { candidates, untriaged: 0, vanished }
}

/// How many candidates each name family has, before any of them have been printed.
///
/// Taken before the read loop rather than after, because that loop *moves* each name into the row it
/// builds — a thread name is used exactly once, so handing it over beats cloning it per row. Over names
/// rather than over candidates so `debug.list_threads`, whose rows are not `DumpRow`s, tallies with the
/// same code as the dump (#51).
fn candidates_by_family<'a>(
    names: impl IntoIterator<Item = &'a str>,
) -> std::collections::HashMap<String, usize> {
    let mut tally = std::collections::HashMap::new();
    for n in names {
        *tally.entry(thread_name_family(n)).or_default() += 1;
    }
    tally
}

/// The tally above minus what actually reached the reply — the "what am I not seeing" answer.
///
/// Counted against the candidates rather than against `limit`, so it covers every reason a row is
/// missing: the limit, and the suspension budget stopping the read pass part way.
fn withheld_by_family<'a>(
    mut tally: std::collections::HashMap<String, usize>,
    shown: impl IntoIterator<Item = &'a str>,
) -> Vec<(String, usize)> {
    for name in shown {
        if let Some(n) = tally.get_mut(&thread_name_family(name)) {
            *n = n.saturating_sub(1);
        }
    }
    let mut out: Vec<(String, usize)> = tally.into_iter().filter(|(_, n)| *n > 0).collect();
    // Biggest group first, ties broken by name so the line is reproducible rather than hash-ordered.
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// The reporting context a dump reply needs beyond the rows themselves.
///
/// Grouped rather than passed one by one: the held duration and the unread count pushed
/// `render_thread_dump` past the argument-count lint, and every field here is *about* the dump rather
/// than part of it.
struct DumpMeta<'a> {
    /// Threads the JVM reported, before any filter.
    total: usize,
    already_suspended: bool,
    resume_note: &'a str,
    cost: u32,
    /// Wall time spent on JDWP traffic for this dump — every round trip in `cost`, from the thread list to
    /// the resume. Divided by `cost` it gives this connection's **observed** per-packet price, which is the
    /// one environment-specific term in `held ≈ packets × (our processing + RTT)` (TEST-8, ADR-0011).
    ///
    /// Reported because it is the number a caller would otherwise have to derive by hand, and the one that
    /// makes a figure measured on loopback inapplicable to their instance. Present even when nothing was
    /// suspended: the traffic happened either way.
    wire: std::time::Duration,
    /// How long the VM was actually held. `None` when this dump did not suspend it — a default dump, or
    /// one reading a VM someone else already stopped, owns no freeze to report.
    held: Option<std::time::Duration>,
    unread: usize,
    /// Threads that ceased to exist between the JVM listing them and this dump asking about them
    /// (DUMP-4, #47). Its own field because it is its own cause: the footer must not fold it into the
    /// count it blames on `limit`.
    vanished: usize,
    /// Which threads the `limit` was spent on, and which groups it passed over (DUMP-3).
    selection: &'a FamilySelection,
}

/// Read one `DumpRow` per thread, honouring the name/suspended filters and the thread limit.
///
/// Every per-thread read is allowed to fail on its own: a thread can die between `AllThreads` and the
/// questions we ask about it, and on a running VM the frame read fails by design. One bad thread must
/// not cost the rest of the dump (that is the difference between this and one `get_stack` per thread).
///
/// `deadline` bounds the **suspension**, not the call (#17): it is checked between threads, so the loop
/// stops at a thread boundary rather than leaving a half-read row, and the caller resumes immediately
/// after. Threads not reached are counted, never silently dropped.
///
/// **Two passes, since DUMP-3 (#43).** The first reads the cheap half of every thread's row — its name
/// and status; the second spends the `limit` on them in `family_round_robin` order and pays for frames
/// and locks only there. This used to be one pass that stopped at the first `limit` ids `AllThreads`
/// handed over, and `AllThreads` is creation order — see `family_round_robin` for the `WildFly` reading
/// that made the difference visible. The extra cost is two packets per thread the dump does not go on to
/// show, and it is paid **only when the dump is truncated**: when `limit` covers the whole JVM the first
/// pass reads exactly what the single loop used to read, thread for thread.
async fn collect_dump_rows(
    conn: &mut jdwp_client::JdwpConnection,
    all: &[u64],
    a: &crate::args::ThreadDumpArgs,
    caps: Option<&jdwp_client::vm::VmCapabilities>,
    deadline: Option<std::time::Instant>,
) -> DumpOutcome {
    let name_filter = a.name_filter.as_deref().filter(|s| !s.is_empty()).map(str::to_lowercase);
    let package_filter = a.package_filter.as_deref().filter(|s| !s.is_empty()).map(str::to_lowercase);
    let limit = a.limit.max(1);
    // Monitors need the JVM to support them; without the capability the commands answer
    // NOT_IMPLEMENTED, so skip them rather than collect a per-thread error for every thread.
    let want_monitors = a.monitors
        && caps.is_some_and(|c| c.can_get_owned_monitor_info || c.can_get_current_contended_monitor);

    let DumpTriage { mut candidates, untriaged, vanished } =
        triage_dump_threads(conn, all, a, name_filter.as_deref(), deadline).await;
    let eligible = candidates.len();
    let tally = candidates_by_family(candidates.iter().map(|c| c.name.as_str()));
    let (order, families) = {
        let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        family_round_robin(&names)
    };

    // Paired with each row's position in `AllThreads`, so the reply can be put back into creation order
    // once the selection has done its job. Choosing fairly and presenting stably are separate jobs.
    let mut rows: Vec<(usize, DumpRow)> = Vec::new();
    // Class names are shared across every thread in the dump — a request pool's stacks are largely the
    // same frames, so this is where most of the lookup cost disappears.
    let mut class_names: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    let mut monitor_names: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    // Line tables, keyed by (class, method) — the single biggest cost in a dump, and one that was paid
    // over and over for the same method (TEST-8, #24). A request pool's threads sit in the SAME code, so
    // the reuse is across threads: 300 workers 60 frames deep asked for ~19,000 line tables covering ~60
    // distinct methods. Held **for this call only** — see `dump_frame_method` for why that scope is the
    // entire safety argument.
    let mut line_tables: LineTableCache = std::collections::HashMap::new();

    // `untriaged` is already unread: the triage pass ran out of its share of the budget before it got to
    // those threads, so they were never even candidates.
    let mut unread = untriaged;
    for (taken, pick) in order.iter().enumerate() {
        if rows.len() >= limit {
            break;
        }
        // Checked at the thread boundary, before spending anything on this one: the budget bounds how
        // long the VM is frozen, so stopping mid-thread would hold it longer to produce a partial row.
        // Everything still unexamined is counted so the reply can say what it skipped.
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            unread += order.len().saturating_sub(taken);
            break;
        }
        // `order` indexes `candidates` by construction; `get_mut` rather than `[]` so a future edit that
        // breaks that invariant skips a row instead of taking the whole dump down with it. The name is
        // *moved* out — each candidate is picked at most once, so nothing is left to read it.
        let Some(c) = candidates.get_mut(*pick) else { continue };
        let (seen, tid, status, suspended, finished) = (c.seen, c.tid, c.status, c.suspended, c.finished);
        let name = std::mem::take(&mut c.name);

        let (stack, frames_hidden) = if !suspended {
            (DumpStack::Unreadable(unreadable_reason(finished, a.monitors_only)), 0)
        } else if a.monitors_only {
            // The cheap mode (#17): no `Frames` request, and none of the per-frame class/method/line
            // lookups that dominate a dump's packet cost and therefore its suspension window.
            (DumpStack::Omitted, 0)
        } else {
            let (frames, hidden) = read_dump_stack(
                conn,
                tid,
                a.max_frames,
                package_filter.as_deref(),
                &mut class_names,
                &mut line_tables,
            )
            .await;
            (frames.map_or_else(DumpStack::Unreadable, DumpStack::Frames), hidden)
        };

        // The "should we even ask?" guard lives inside the helper, so the loop body holds no
        // per-iteration collection of its own.
        let (holds, waiting_on, monitor_note) =
            read_thread_monitors(conn, tid, want_monitors && suspended, &mut monitor_names).await;

        rows.push((
            seen,
            DumpRow {
                id: tid,
                name,
                status,
                suspended,
                finished,
                stack,
                frames_hidden,
                holds,
                waiting_on,
                monitor_note,
            },
        ));
    }
    rows.sort_by_key(|(seen, _)| *seen);
    let rows: Vec<DumpRow> = rows.into_iter().map(|(_, r)| r).collect();
    let selection = FamilySelection {
        eligible,
        families,
        withheld: withheld_by_family(tally, rows.iter().map(|r| r.name.as_str())),
    };
    DumpOutcome { rows, unread, vanished, selection }
}

/// Why a thread's frames and locks could not be read, phrased for the state the thread is actually in.
///
/// DUMP-4 (#47). This used to be one sentence — `running — … pass suspend:true` — printed for every
/// unreadable row, and TEST-10's churning pool is where that goes wrong: the JVM has just answered
/// `ZOMBIE`, so the thread is *finished*, and the row described it as running and then advised a remedy
/// that can never apply. A finished thread will never be suspendable, so `suspend:true` is not a smaller
/// help here, it is no help at all.
///
/// It is ADR-0009's rule read the other way round. That decision says a running thread must never render
/// as `(no frames)` "because 'unreadable' and 'idle' are opposite answers on a wedged JVM". Finished and
/// running are opposite answers too, and this is the dump picking between them rather than guessing.
///
/// `monitors_only` decides which noun is named, so a dump that never wanted a stack is not told about one.
fn unreadable_reason(finished: bool, monitors_only: bool) -> String {
    let what = if monitors_only { "locks" } else { "stack" };
    if finished {
        return format!(
            "finished — this thread has already terminated (JDWP reports ZOMBIE), so there is no {what} \
             left to read; suspend:true cannot help, because a finished thread can never be suspended"
        );
    }
    // Not a failure of ours to explain away: JDWP defines both frames and locks as readable only on a
    // suspended thread.
    format!("running — JDWP can only read a suspended thread's {what}; pass suspend:true")
}

/// Read one suspended thread's lock state, as `(monitors held, monitor blocked on, failure note)`.
///
/// `ask` false returns the empty answer without a round trip — monitors not requested, or a thread whose
/// locks JDWP won't report because it is running.
///
/// Each half is allowed to fail independently: a JVM can support `canGetOwnedMonitorInfo` without
/// `canGetCurrentContendedMonitor`, and a thread can die between the two calls. Either way the note is
/// reported on that thread's line rather than aborting the dump.
async fn read_thread_monitors(
    conn: &mut jdwp_client::JdwpConnection,
    tid: u64,
    ask: bool,
    names: &mut std::collections::HashMap<u64, String>,
) -> (Vec<(String, u64)>, Option<(String, u64)>, Option<String>) {
    let mut holds = Vec::new();
    let mut waiting_on = None;
    let mut note = None;
    if !ask {
        return (holds, waiting_on, note);
    }
    match conn.owned_monitors(tid).await {
        Ok(ms) => {
            for m in ms {
                let rendered = monitor_label(conn, m.object_id, names).await;
                holds.push((rendered, m.object_id));
            }
        }
        Err(e) => note = Some(format!("owned monitors unreadable: {e}")),
    }
    match conn.current_contended_monitor(tid).await {
        Ok(Some(m)) => {
            let rendered = monitor_label(conn, m.object_id, names).await;
            waiting_on = Some((rendered, m.object_id));
        }
        Ok(None) => {}
        Err(e) => note = Some(format!("contended monitor unreadable: {e}")),
    }
    (holds, waiting_on, note)
}

/// Read and render one thread's frames for a dump, returning `(frames, hidden count)`.
///
/// `-1` (all frames) then truncate, for the same reason the trace capture does it: JDWP fails a
/// `Frames` request whose length exceeds what the thread actually has.
async fn read_dump_stack(
    conn: &mut jdwp_client::JdwpConnection,
    tid: u64,
    max_frames: usize,
    package_filter: Option<&str>,
    class_names: &mut std::collections::HashMap<u64, String>,
    line_tables: &mut LineTableCache,
) -> (Result<Vec<String>, String>, usize) {
    let frames = match conn.get_frames(tid, 0, -1).await {
        Ok(f) => f,
        Err(e) => return (Err(format!("stack unreadable: {e}")), 0),
    };
    let depth = frames.len();
    let mut out = Vec::new();
    let mut hidden = 0usize;
    for (idx, f) in frames.iter().enumerate() {
        if out.len() >= max_frames {
            hidden = depth - out.len() - hidden;
            break;
        }
        let class = resolve_class_name(conn, f.location.class_id, class_names).await;
        // Filtered-out frames cost only the (cached) class name, never a method or line lookup.
        if package_filter.is_some_and(|p| !class.to_lowercase().contains(p)) {
            hidden += 1;
            continue;
        }
        let (method, line) = dump_frame_method(conn, &f.location, line_tables).await;
        out.push(
            line.map_or_else(
                || format!("#{idx} {class}.{method}"),
                |l| format!("#{idx} {class}.{method}:{l}"),
            ),
        );
    }
    (Ok(out), hidden)
}

/// Render a monitor object as `Type@<id>`, caching the type name by object id.
///
/// Cached because the interesting case is *the same lock* appearing on several threads — which is the
/// whole point of the correlation — and each name otherwise costs two round trips.
async fn monitor_label(
    conn: &mut jdwp_client::JdwpConnection,
    object_id: u64,
    cache: &mut std::collections::HashMap<u64, String>,
) -> String {
    if let Some(n) = cache.get(&object_id) {
        return n.clone();
    }
    let name = match conn.get_object_reference_type(object_id).await {
        Ok(rt) => conn
            .get_signature(rt)
            .await
            .ok()
            .map(|s| decode_signature(&s))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "?".to_string()),
        Err(_) => "?".to_string(),
    };
    let label = format!("{name}@{object_id:x}");
    cache.insert(object_id, label.clone());
    label
}

/// The `debug.thread_dump` header: what was asked for, what the suspension did, and every reason the
/// dump might be less complete than it looks.
///
/// Each of those caveats is here because its absence would read as a positive answer — no lock lines as
/// "nothing is contended", unreadable threads as "nothing to see", absent stacks as "no frames". They
/// are split across three helpers because there are now enough of them to trip the complexity gate, and
/// because "what was asked" and "why this may be incomplete" are separate things to read.
fn render_dump_header(
    rows: &[DumpRow],
    a: &crate::args::ThreadDumpArgs,
    caps: Option<&jdwp_client::vm::VmCapabilities>,
    meta: &DumpMeta<'_>,
) -> String {
    let mut out =
        format!("🧵 Thread dump — {}/{} thread(s){}\n", rows.len(), meta.total, dump_filter_note(a));
    out.push_str(&family_order_note(rows.len(), meta.selection));
    if meta.already_suspended {
        out.push_str("   VM was already suspended — read as it is, and left suspended.\n");
    }
    if !meta.resume_note.is_empty() {
        let _ = writeln!(out, "   {}", meta.resume_note);
    }
    // How long the VM was actually frozen (#17). Reported even on a fast dump, because the useful thing
    // is the trend on a shared instance, not only the times it went wrong.
    if let Some(held) = meta.held {
        let _ = writeln!(out, "   ⏱  Held the VM suspended for {}ms.", held.as_millis());
    }
    // The budget stopped it. Separate from the resume note on purpose: "I stopped early" and "I could not
    // resume" are different problems, and a truncated dump must never read as a complete one.
    if meta.unread > 0 {
        let _ = writeln!(
            out,
            "   ✂️  Stopped early — the {}ms suspension budget ran out with {} thread(s) still \
             unexamined, so this dump is INCOMPLETE. Raise max_suspend_ms for a deeper dump, or narrow \
             with name_filter / limit / max_frames / package_filter, which costs nothing.{}",
            a.max_suspend_ms,
            meta.unread,
            truncation_estimate(rows.len(), meta)
        );
    }
    out.push_str(&dump_monitor_caveats(a, caps));
    // Finished threads are excluded on purpose (DUMP-4, #47): they are unreadable too, but `suspend:true`
    // is not the answer for them, so counting them here would inflate the number of threads the advice
    // below would actually rescue.
    let unreadable =
        rows.iter().filter(|r| matches!(r.stack, DumpStack::Unreadable(_)) && !r.finished).count();
    if unreadable > 0 && !a.suspend {
        let _ = writeln!(
            out,
            "   ℹ️  {unreadable} thread(s) are running, so their stacks and locks can't be read. Pass \
             suspend:true to freeze the VM briefly for a full dump, or only_suspended:true to list just \
             the readable ones."
        );
    }
    out
}

/// The line that says WHICH threads a shortened reply chose, and on what rule (DUMP-3, #43).
///
/// Printed only when something was left out, because that is the only time the rule changes what you see.
/// It exists because the alternative is a header that reads as a representative sample: a default dump of
/// a real `WildFly` said `40/267 thread(s)` while every one of those 40 was a JVM internal, an MSC service
/// thread or an Undertow selector, and the 13 request workers a caller came for sat 328 frames deep and
/// unread. Nothing in that reply said so, which is this repo's recurring failure — a check that reports
/// success without having looked.
///
/// Silent on a single-family reply: `name_filter: "default task"` narrows to one pool, round-robin over one
/// family IS creation order, and announcing a rule that did nothing is noise.
///
/// Printed word for word by `debug.thread_dump` and `debug.list_threads` (DUMP-5, #51) — one function,
/// because the two tools stating the same rule in two wordings is how they start meaning different things.
fn family_order_note(shown: usize, sel: &FamilySelection) -> String {
    if shown >= sel.eligible || sel.families < 2 {
        return String::new();
    }
    format!(
        "   🔀 Chose {shown} of {} by NAME FAMILY, not by the order the JVM listed them in: one thread \
         from each of the {} distinct names (digits ignored, so \"task-3\" and \"task-91\" are one \
         family) before a second from any, so no single pool can spend every slot. JDWP `AllThreads` \
         order is *creation* order, and an app server creates its request pool last (DUMP-3). Rows below \
         are printed in creation order.\n",
        sel.eligible, sel.families
    )
}

/// The `— biggest groups not shown: 227 × "churn-worker-#"` tail on the truncation footer (DUMP-3).
///
/// A count of what is missing answers "is this dump short?"; naming the groups answers "short of what?",
/// which is the question that decides whether to raise `limit` or reach for `name_filter`. Capped at five
/// groups, because a 267-thread JVM has more families than anyone reads in a footer.
fn withheld_note(withheld: &[(String, usize)]) -> String {
    const LISTED: usize = 5;
    if withheld.is_empty() {
        return String::new();
    }
    let named: Vec<String> = withheld.iter().take(LISTED).map(|(f, n)| format!("{n} × \"{f}\"")).collect();
    let rest = withheld.len().saturating_sub(LISTED);
    format!(
        " — biggest groups not shown: {}{}",
        named.join(", "),
        if rest > 0 { format!(", and {rest} other group(s)") } else { String::new() }
    )
}

/// The `, 0.42ms each` suffix on the cost line — this connection's observed per-packet price (TEST-8).
///
/// The whole point of reporting it: the ~0.2ms figure in this repo's notes is a **loopback** measurement,
/// and the term that changes on a real instance is the round trip. A caller who can read what their own
/// connection costs never has to wonder whether a documented number applies to them. Suppressed below two
/// packets, where a mean is not a measurement.
fn per_packet_note(cost: u32, wire: std::time::Duration) -> String {
    if cost < 2 {
        return String::new();
    }
    let per = wire.as_secs_f64() * 1000.0 / f64::from(cost);
    format!(", {per:.2}ms each (round trip + our own processing)")
}

/// What a truncated `debug.list_threads` spent, and what the rule it now selects by added (DUMP-5, #51).
///
/// Stated in the reply rather than only in the docs, because this tool's whole value is being cheap and a
/// claim about cost that the caller cannot check is exactly the kind this repo keeps having to withdraw.
/// The counterfactual is the honest comparison and it is arithmetic, not an estimate: the loop this
/// replaced read one name per row it printed and then stopped, so `shown + 1` (the thread list, plus a
/// name each) is precisely what the old behaviour would have cost on this same call.
///
/// **Offered only when it is true.** A listing narrowed by `name_filter` or `only_suspended` already had
/// to read every name to apply the filter, so selection added nothing to it and claiming a saving would be
/// inventing one.
fn list_cost_note(cost: u32, wire: std::time::Duration, shown: usize, filtering: bool) -> String {
    let comparison = if filtering {
        " — one per thread NAME. A filtered listing always read every name to apply the filter, so \
         choosing by family costs it nothing extra."
            .to_string()
    } else {
        format!(
            " — one per thread NAME, because choosing by family has to read them all: taking the first \
             {shown} in list order would have cost {}, and would have been the wrong {shown}. Still one \
             packet per thread, against a dump's ~8 per thread it shows.",
            shown + 1
        )
    };
    format!("💸 Cost: {cost} JDWP packet(s){}{comparison}\n", per_packet_note(cost, wire))
}

/// What the threads a truncated dump never reached would have cost, extrapolated from the ones it did
/// (TEST-8).
///
/// Not a guess: the rate comes from this dump's own held window and the threads it actually read, so it is
/// the observed cost of this pool on this connection. It is the number that decides between the two ways
/// out of a truncation — narrow the dump, or raise the budget — and deriving it by hand is exactly the
/// arithmetic #24 was going to ask a human for.
///
/// Deliberately says "at the rate this dump ran", because a pool is not uniform: the estimate is honest
/// about being one.
fn truncation_estimate(rows_read: usize, meta: &DumpMeta<'_>) -> String {
    let (Some(held), true) = (meta.held, rows_read > 0 && meta.unread > 0) else {
        return String::new();
    };
    // Thread counts, not values near 2^53 — a pool that could lose precision here would not fit in memory.
    #[allow(clippy::cast_precision_loss)]
    let (read, skipped) = (rows_read as f64, meta.unread as f64);
    let held_ms = held.as_secs_f64() * 1000.0;
    let per_thread_ms = held_ms / read;
    let remaining_ms = per_thread_ms * skipped;
    let full_ms = per_thread_ms.mul_add(skipped, held_ms);
    format!(
        " At the rate this dump ran ({per_thread_ms:.1}ms per thread), the {} it skipped need \
         ~{remaining_ms:.0}ms more — about {full_ms:.0}ms for the whole set, so either narrow it or raise \
         max_suspend_ms past that.",
        if meta.unread == 1 { "1 thread".to_string() } else { format!("{} threads", meta.unread) }
    )
}

/// The ` name~"x" suspended-only frames~"y" monitors-only` suffix on a dump's title line — what the
/// caller asked to narrow by.
///
/// A frame filter is echoed only when frames were actually read. In monitors-only mode it is reported as
/// ignored instead (see `dump_monitor_caveats`), because echoing it here would credit the dump with a
/// narrowing it never performed.
fn dump_filter_note(a: &crate::args::ThreadDumpArgs) -> String {
    let mut note = String::new();
    if let Some(f) = a.name_filter.as_deref().filter(|s| !s.is_empty()) {
        let _ = write!(note, " name~\"{f}\"");
    }
    if a.only_suspended {
        note.push_str(" suspended-only");
    }
    if !a.monitors_only {
        if let Some(p) = a.package_filter.as_deref().filter(|s| !s.is_empty()) {
            let _ = write!(note, " frames~\"{p}\"");
        }
    }
    if a.monitors_only {
        note.push_str(" monitors-only");
    }
    note
}

/// Everything the header has to say about locks and omitted stacks: that stacks were skipped by request,
/// that a frame filter therefore did nothing, and that this JVM may not be able to answer at all.
///
/// All three exist because silence would read as a finding — an absent stack as an idle thread, an
/// absent lock line as an uncontended one.
fn dump_monitor_caveats(
    a: &crate::args::ThreadDumpArgs,
    caps: Option<&jdwp_client::vm::VmCapabilities>,
) -> String {
    let mut out = String::new();
    if a.monitors_only {
        out.push_str(
            "   🔒 monitors-only — locks were read and stacks deliberately were NOT (~4 JDWP packets \
             per thread rather than ~4 plus ~3 per frame), so a thread with no frames listed here means \
             \"not requested\", not \"idle\". Drop monitors_only for stacks.\n",
        );
        if let Some(p) = a.package_filter.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(
                out,
                "   ℹ️  package_filter \"{p}\" and max_frames had no effect — monitors-only reads no \
                 frames to filter."
            );
        }
    }
    if !a.monitors {
        return out;
    }
    // A JVM that can't answer the monitor questions must say so, rather than silently returning a dump
    // with no locks in it — which reads as "nothing is contended".
    let (owned, contended) =
        caps.map_or((false, false), |c| (c.can_get_owned_monitor_info, c.can_get_current_contended_monitor));
    match caps {
        Some(_) if owned && contended => {}
        Some(c) => {
            let _ = writeln!(
                out,
                "   ⚠️  This JVM cannot report all monitor info (canGetOwnedMonitorInfo={}, \
                 canGetCurrentContendedMonitor={}) — lock lines are limited to what it supports.",
                c.can_get_owned_monitor_info, c.can_get_current_contended_monitor
            );
        }
        None => out.push_str("   ⚠️  Could not read this JVM's capabilities — monitors were skipped.\n"),
    }
    // In monitors-only mode the locks are the ENTIRE payload, so a JVM that can report none of them
    // returns a dump with nothing in it at all. That emptiness must not be read as "nothing is
    // contended" — it is "nothing was askable".
    if a.monitors_only && !owned && !contended {
        out.push_str(
            "   🛑 …and monitors-only asked for nothing else, so this dump has NO lock payload — its \
             emptiness says nothing about contention. Drop monitors_only to at least get stacks.\n",
        );
    }
    out
}

/// One thread's block: header, lock lines, then frames (or why there are none).
///
/// The header keeps the two states **visually apart**, because they are independent axes and reading them
/// as one list gets the important one backwards. `monitor` is the application's own state — this thread is
/// blocked on a lock — while `debugger-suspended` means we are holding it, which is the only reason its
/// stack is readable at all. `[monitor, suspended]` invited the reading "suspended at a monitor", which
/// attributes the freeze to the application instead of to us.
fn render_dump_row(out: &mut String, r: &DumpRow, holder: &std::collections::HashMap<u64, (u64, &str)>) {
    let _ = write!(
        out,
        "\n0x{:x} \"{}\" [{}]{}\n",
        r.id,
        r.name,
        r.status,
        if r.suspended { " debugger-suspended" } else { "" }
    );
    if let Some((label, oid)) = &r.waiting_on {
        // The holder is looked up among the rows actually dumped, so a lock held by a thread that was
        // filtered out or fell past `limit` is shown WITHOUT a holder rather than with a wrong one.
        let by = holder
            .get(oid)
            .map_or_else(String::new, |(htid, hname)| format!(" ← held by 0x{htid:x} \"{hname}\""));
        let _ = writeln!(out, "   waiting to enter: {label}{by}");
    }
    if !r.holds.is_empty() {
        let labels: Vec<&str> = r.holds.iter().map(|(l, _)| l.as_str()).collect();
        let _ = writeln!(out, "   holds: {}", labels.join(", "));
    }
    if let Some(n) = &r.monitor_note {
        let _ = writeln!(out, "   ⚠️  {n}");
    }
    match &r.stack {
        DumpStack::Frames(frames) if frames.is_empty() => out.push_str("   (no frames)\n"),
        DumpStack::Frames(frames) => {
            for f in frames {
                let _ = writeln!(out, "   {f}");
            }
            if r.frames_hidden > 0 {
                let _ = writeln!(out, "   … {} frame(s) hidden", r.frames_hidden);
            }
        }
        // "Unreadable" and "idle" are opposite answers on a wedged JVM, so this never renders as
        // `(no frames)`.
        DumpStack::Unreadable(why) => {
            let _ = writeln!(out, "   ⚠️  {why}");
        }
        // Nothing per row: the header says once that stacks were not requested. Repeating it on forty
        // threads would bury the lock lines the mode exists to show.
        DumpStack::Omitted => {}
    }
}

/// Format a whole `debug.thread_dump` reply.
///
/// The lock correlation — `← held by 0x2b "worker-2"` — is computed here from the rows already
/// collected, costing nothing extra. It is the line that turns two separate facts ("A waits for L",
/// "B holds L") into a visible cycle, which is what a deadlock investigation is looking for.
fn render_thread_dump(
    rows: &[DumpRow],
    a: &crate::args::ThreadDumpArgs,
    caps: Option<&jdwp_client::vm::VmCapabilities>,
    meta: &DumpMeta<'_>,
) -> String {
    // object id -> the thread holding it, for the "held by" annotation.
    let mut holder: std::collections::HashMap<u64, (u64, &str)> = std::collections::HashMap::new();
    for r in rows {
        for (_, oid) in &r.holds {
            holder.insert(*oid, (r.id, r.name.as_str()));
        }
    }

    let mut out = render_dump_header(rows, a, caps, meta);
    for r in rows {
        render_dump_row(&mut out, r, &holder);
    }

    // Every thread the JVM listed and this reply did not show, split by WHY (DUMP-4, #47).
    //
    // It used to be one sentence, and it named the caller's `limit` whatever the cause was. TEST-10's
    // churning pool is where that becomes a lie the caller can act on: 41 rows were missing because
    // those threads had *died* mid-read, and the reply advised raising a `limit` of 500 that had never
    // bound, or narrowing with a `name_filter` that cannot bring a dead thread back. Two no-ops offered
    // as remedies. The header already keeps the budget truncation apart from a failed resume (ADR-0009)
    // for the same reason; this is the third cause finally getting its own voice.
    //
    // The two counts still sum to the shortfall, so the arithmetic a caller checks is unchanged.
    let hidden = meta.total.saturating_sub(rows.len());
    let vanished = meta.vanished.min(hidden);
    let withheld = hidden - vanished;
    if withheld > 0 {
        let _ = writeln!(
            out,
            "\n… +{withheld} more thread(s) (raise limit, or narrow with name_filter){}",
            withheld_note(&meta.selection.withheld)
        );
    }
    if vanished > 0 {
        let _ = writeln!(
            out,
            "\n… +{vanished} more thread(s) ENDED while this dump was reading — the JVM listed them and \
             their ids were already invalid by the time it asked, which is what a pool retiring its \
             workers looks like from here. Nothing to raise or narrow: those threads are gone, and a \
             later dump will simply not list them."
        );
    }
    let _ = write!(out, "\nCost: {} JDWP packet(s){}.", meta.cost, per_packet_note(meta.cost, meta.wire));
    out
}

/// JDWP threadStatus code -> short label (see `types::ThreadStatus`).
const fn thread_status_name(ts: i32) -> &'static str {
    match ts {
        0 => "zombie",
        1 => "running",
        2 => "sleeping",
        3 => "monitor",
        4 => "wait",
        _ => "unknown",
    }
}

/// Best-effort source line for a (class, method, bytecode index): the line whose code index
/// is the greatest <= the given index.
async fn source_line(
    conn: &mut jdwp_client::JdwpConnection,
    class_id: u64,
    method_id: u64,
    index: u64,
) -> Option<i32> {
    let lt = conn.get_line_table(class_id, method_id).await.ok()?;
    line_at(&lt, index)
}

/// The source line covering bytecode `index`: the last table entry at or before it.
///
/// Split out so the cached and uncached paths cannot disagree about what a line table means.
fn line_at(lt: &jdwp_client::method::LineTable, index: u64) -> Option<i32> {
    lt.lines
        .iter()
        .filter(|e| e.line_code_index <= index)
        .max_by_key(|e| e.line_code_index)
        .map(|e| e.line_number)
}

/// Line tables for one dump, keyed by (class, method). `None` records a method that HAS no table — a
/// native or abstract one answers `ABSENT_INFORMATION`, and a refusal has to be remembered too, or every
/// thread re-asks the same question and gets the same refusal.
type LineTableCache = std::collections::HashMap<(u64, u64), Option<jdwp_client::method::LineTable>>;

/// One dump frame's (method name, source line), reading each line table at most once per dump (TEST-8).
///
/// A dump's cost is dominated by `Method.LineTable`: one round trip per frame. Measured against a
/// production-shaped pool (#24), that was ~19,000 of the 21,364 packets a 300-thread, 60-frame dump spent,
/// while covering only ~60 distinct methods — because the threads of a request pool are all standing in the
/// same code. Method *lists* were already cached on the connection; line tables were not, so the identical
/// question was asked once per frame per thread.
///
/// **The cache is per call, and that is the point rather than an implementation detail.** ADR-0009 records
/// #17's rejection of caching line tables *across* dumps on BP-4 grounds: `RedefineClasses` keeps the
/// referenceTypeID and replaces the code, so a connection-lifetime entry can serve a line number that is
/// quietly wrong, and a stale source line is worse than a round trip. Within one call there is no such
/// window — the VM is suspended for the read when `suspend:true`, the map dies with the reply, and every
/// hit is another thread standing in the very code just read. So this takes the win that decision declined
/// without taking the risk it declined it for.
async fn dump_frame_method(
    conn: &mut jdwp_client::JdwpConnection,
    loc: &Location,
    line_tables: &mut LineTableCache,
) -> (String, Option<i32>) {
    // `get_methods` is already cached per connection, so this costs a round trip once per class, ever.
    let method = conn
        .get_methods(loc.class_id)
        .await
        .ok()
        .and_then(|ms| ms.into_iter().find(|m| m.method_id == loc.method_id).map(|m| m.name))
        .unwrap_or_else(|| format!("method@{:x}", loc.method_id));

    // `get` then `insert` rather than the `entry` API: producing the value needs an `await`, and an
    // occupied `Entry` would borrow the map across it. The lookup resolves to an owned `Option<i32>`
    // immediately so the borrow ends before the miss branch inserts (edition 2021 keeps an `if let`
    // condition's borrow alive through the `else` otherwise).
    let key = (loc.class_id, loc.method_id);
    let cached = line_tables.get(&key).map(|lt| lt.as_ref().and_then(|t| line_at(t, loc.index)));
    let line = if let Some(line) = cached {
        line
    } else {
        let fetched = conn.get_line_table(loc.class_id, loc.method_id).await.ok();
        let line = fetched.as_ref().and_then(|lt| line_at(lt, loc.index));
        line_tables.insert(key, fetched);
        line
    };
    (method, line)
}

/// Resolve (class name, method name, source line) for a location.
async fn describe_location(
    conn: &mut jdwp_client::JdwpConnection,
    loc: &Location,
) -> (String, String, Option<i32>) {
    let class = conn.get_signature(loc.class_id).await.ok().map(|s| decode_signature(&s)).unwrap_or_default();
    let method = conn
        .get_methods(loc.class_id)
        .await
        .ok()
        .and_then(|ms| ms.into_iter().find(|m| m.method_id == loc.method_id).map(|m| m.name))
        .unwrap_or_default();
    let line = source_line(conn, loc.class_id, loc.method_id, loc.index).await;
    (class, method, line)
}

/// Snapshot a trace/logpoint hit: source location, the calling chain above it, in-scope locals/args,
/// the kind-specific detail (exception type + catch site, or a watched field's old → new pair), and
/// any trace expression.
///
/// The hit thread is suspended (`EventThread` policy) while this runs; the caller resumes it right
/// after. Argument values are rendered WITHOUT invoking `toString()` (`thread_id` None), so tracing
/// stays side-effect free; the explicit `trace_expr` may invoke methods since the user asked for it.
///
/// `trace_frames` caller frames are recorded above the hit (TRACE-5) as bare `class.method:line`
/// locations — no locals — because they are context rather than payload, and a logpoint may fire
/// hundreds of times. Asking for them costs no extra `Frames` round trip (the same call that fetches
/// the hit frame fetches them), only the per-frame location lookups.
///
/// The watchpoint detail must be captured HERE rather than at read time for the same reason
/// `get_last_event` reports it inline: the old value is only readable while the pending store has not
/// committed, which is exactly this window.
async fn capture_trace(
    conn: &mut jdwp_client::JdwpConnection,
    bp_id: &str,
    trace_expr: Option<&str>,
    trace_frames: usize,
    thread: u64,
    loc: &Location,
    details: &EventKind,
) -> crate::session::TraceRecord {
    let (class, method, line) = describe_location(conn, loc).await;
    let mut args: Vec<(String, String)> = Vec::new();
    let mut callers: Vec<String> = Vec::new();
    let mut expr: Option<(String, String)> = None;

    // The hit frame plus however many callers were asked for, in ONE `Frames` request.
    //
    // `-1` (all frames) rather than the exact count, then truncate: JDWP answers `INVALID_LENGTH` when
    // `length` exceeds the frames a thread actually has, and a thread is routinely shallower than the
    // requested depth (`main` is only two frames under a helper). Asking for the exact number failed
    // the whole read on those hits — losing the LOCALS as well as the callers, silently, on precisely
    // the shallow stacks a small depth was meant to cover. `get_stack` avoids it the same way.
    let frames = if trace_frames == 0 {
        // Depth 0 keeps the original single-frame request, so turning the feature off costs exactly
        // what it did before: every live thread has at least one frame, so length 1 is always valid.
        conn.get_frames(thread, 0, 1).await
    } else {
        conn.get_frames(thread, 0, -1).await.map(|mut f| {
            f.truncate(1 + trace_frames);
            f
        })
    };
    if let Ok(frames) = frames {
        // A thread may be shallower than the requested depth — that is normal (a request thread's
        // entry point has no caller), not an error, so take whatever came back.
        callers = describe_caller_chain(conn, frames.get(1..).unwrap_or_default()).await;
        if let Some(frame) = frames.first().cloned() {
            if let Ok(var_table) = conn.get_variable_table(loc.class_id, loc.method_id).await {
                let ci = loc.index;
                // Own each in-scope variable's (name, slot) so the names can be moved into `args`
                // below without cloning.
                let in_scope: Vec<(String, jdwp_client::stackframe::VariableSlot)> = var_table
                    .into_iter()
                    .filter(|v| ci >= v.code_index && ci < v.code_index + u64::from(v.length))
                    .map(|v| {
                        let slot = i32::try_from(v.slot).unwrap_or(0);
                        let sig_byte = v.signature.as_bytes().first().copied().unwrap_or(b'I');
                        (v.name, jdwp_client::stackframe::VariableSlot { slot, sig_byte })
                    })
                    .collect();
                let slots: Vec<jdwp_client::stackframe::VariableSlot> =
                    in_scope.iter().map(|(_, s)| *s).collect();
                if !slots.is_empty() {
                    if let Ok(vals) = conn.get_frame_values(thread, frame.frame_id, slots).await {
                        for ((name, _), val) in in_scope.into_iter().zip(vals.iter()) {
                            let rendered = render_value(conn, val, None, 100).await;
                            args.push((name, rendered));
                        }
                    }
                }
            }
            if let Some(e) = trace_expr {
                let rendered = match resolve_expression(conn, Some(thread), Some(&frame), e).await {
                    Ok(v) => render_value(conn, &v, Some(thread), 200).await,
                    Err(err) => format!("<error: {err}>"),
                };
                expr = Some((e.to_string(), rendered));
            }
        }
    }

    // Reuse the same describers `get_last_event` uses, so a traced exception/watch hit says exactly
    // what a suspending one would; the pairs are flattened for the one-line trace rendering.
    let mut obj = serde_json::Map::new();
    describe_exception_event(conn, details, &mut obj).await;
    describe_field_event(conn, details, &mut obj).await;
    describe_method_exit_event(conn, details, &mut obj).await;
    let detail = obj.into_iter().map(|(k, v)| (k, json_scalar_to_string(&v))).collect();

    crate::session::TraceRecord {
        seq: 0,
        bp_id: bp_id.to_string(),
        thread,
        class,
        method,
        line,
        args,
        callers,
        expr,
        detail,
    }
}

/// Render a run of caller frames as `class.method:line`, nearest caller first (TRACE-5).
///
/// Locations only — `frame_method_info` is called with `include_variables: false`, so no variable table
/// is read and nothing is invoked. That is what keeps a caller chain usable in a read-only session
/// (SAFE-6) and its per-hit cost proportional to the depth rather than to how many locals each caller
/// happens to hold.
///
/// Class names are memoised **within this one call** (the same cache `get_stack` uses), since a caller
/// chain often repeats a class — recursion, or a framework dispatching into itself. Deliberately not
/// cached across hits: a reference type id is only stable while that type stays loaded, and a debugger
/// that reports a stale source line after a redeploy is worse than one that costs a round trip (BP-4).
async fn describe_caller_chain(
    conn: &mut jdwp_client::JdwpConnection,
    frames: &[jdwp_client::thread::Frame],
) -> Vec<String> {
    let mut class_names: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    let mut out = Vec::with_capacity(frames.len());
    for f in frames {
        let class = resolve_class_name(conn, f.location.class_id, &mut class_names).await;
        let (method, line, _) = frame_method_info(conn, &f.location, false).await;
        out.push(line.map_or_else(|| format!("{class}.{method}"), |l| format!("{class}.{method}:{l}")));
    }
    out
}

/// Render a JSON scalar for a one-line trace record: strings unquoted (they are already rendered
/// values like `"OPEN"` or `(int) 3`), everything else as-is.
fn json_scalar_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Format a traced hit's kind-specific detail as ` k=v k=v` (empty for a plain line logpoint).
///
/// Ahead of the locals on purpose: for an exception or watchpoint hit this *is* the answer — which
/// exception, or which field went from what to what — and the locals are supporting context.
fn format_trace_detail(rec: &crate::session::TraceRecord) -> String {
    rec.detail.iter().fold(String::new(), |mut acc, (k, v)| {
        let _ = write!(acc, " {k}={v}");
        acc
    })
}

/// Format a traced hit's calling chain as ` ← caller ← caller` (empty when none was captured).
///
/// Placed immediately after the hit location rather than at the end of the line: it is a continuation
/// of *where this fired*, so `Class.method:12 ← Caller.method:40` reads as one call chain, and it stays
/// legible next to the location instead of trailing a long list of locals (TRACE-5).
fn format_trace_callers(rec: &crate::session::TraceRecord) -> String {
    rec.callers.iter().fold(String::new(), |mut acc, c| {
        let _ = write!(acc, " ← {c}");
        acc
    })
}

/// Format a trace record's captured args as ` {n=v, …}` (empty string when there are none).
fn format_trace_args(rec: &crate::session::TraceRecord) -> String {
    if rec.args.is_empty() {
        String::new()
    } else {
        let parts: Vec<String> = rec.args.iter().map(|(n, v)| format!("{n}={v}")).collect();
        format!(" {{{}}}", parts.join(", "))
    }
}

/// Format a trace record's optional trace expression as ` | expr => value` (empty when absent).
fn format_trace_expr(rec: &crate::session::TraceRecord) -> String {
    match &rec.expr {
        Some((e, v)) => format!(" | {e} => {v}"),
        None => String::new(),
    }
}

// ----- conditional breakpoints -----

/// Evaluate a breakpoint condition on a thread's top frame. Returns true to KEEP the VM
/// suspended (condition true, or it couldn't be evaluated), false to auto-resume.
async fn evaluate_condition_on_thread(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: u64,
    condition: &str,
) -> bool {
    let frame = match conn.get_frames(thread_id, 0, 1).await {
        Ok(f) => match f.into_iter().next() {
            Some(fr) => fr,
            None => return true,
        },
        Err(_) => return true,
    };
    eval_condition(conn, thread_id, &frame, condition).await.unwrap_or(true)
}

/// Split a boolean expression on a doubled operator (`&&` or `||`, given `op` = `'&'` or `'|'`) at
/// bracket/paren/quote depth 0 (EVAL-4). Returns the whole string as one part when the operator is
/// absent, so a plain comparison flows through unchanged.
fn split_bool(s: &str, op: char) -> Vec<String> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut last = 0usize;
    let mut k = 0usize;
    while let Some(&(i, c)) = chars.get(k) {
        match c {
            '"' => in_str = !in_str,
            '(' | '[' if !in_str => depth += 1,
            ')' | ']' if !in_str => depth -= 1,
            _ if !in_str && depth == 0 && c == op && chars.get(k + 1).is_some_and(|n| n.1 == op) => {
                parts.push(s.get(last..i).unwrap_or("").trim().to_string());
                last = chars.get(k + 1).map_or(i, |n| n.0) + op.len_utf8();
                k += 2;
                continue;
            }
            _ => {}
        }
        k += 1;
    }
    parts.push(s.get(last..).unwrap_or("").trim().to_string());
    parts
}

/// A parsed boolean expression (EVAL-4): a tree of `||`/`&&` over comparison/bool leaf strings.
enum BoolTree {
    Or(Vec<Self>),
    And(Vec<Self>),
    Leaf(String),
}

/// Parse a boolean expression into a [`BoolTree`]: `||` is lowest precedence and `&&` binds tighter,
/// so `a || b && c` parses as `a || (b && c)` — documented and tested — and parentheses regroup, so
/// `(a || b) && c` nests the other way. Recursive, so a parenthesised sub-expression is parsed in full.
fn parse_bool_tree(s: &str) -> BoolTree {
    let s = strip_enclosing_parens(s.trim());
    let ors = split_bool(s, '|');
    if ors.len() > 1 {
        return BoolTree::Or(ors.iter().map(|p| parse_bool_tree(p)).collect());
    }
    let ands = split_bool(s, '&');
    if ands.len() > 1 {
        return BoolTree::And(ands.iter().map(|p| parse_bool_tree(p)).collect());
    }
    BoolTree::Leaf(s.to_string())
}

/// Whether `s` is wholly wrapped in one matching pair of parens, so they can be stripped before
/// splitting a comparison inside them (`(total > 100)`).
fn parens_enclose(s: &str) -> bool {
    if !(s.starts_with('(') && s.ends_with(')')) {
        return false;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    return i == s.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

/// Strip any layers of fully-enclosing parens from a boolean leaf, so `((a == b))` splits like `a == b`.
fn strip_enclosing_parens(s: &str) -> &str {
    let mut t = s.trim();
    while parens_enclose(t) {
        t = t[1..t.len() - 1].trim();
    }
    t
}

/// Split a condition into `left OP right` at the top level (outside parens/quotes).
fn split_comparison(cond: &str) -> Option<(String, String, String)> {
    let ops = ["==", "!=", "<=", ">=", "<", ">"];
    let mut depth = 0i32;
    let mut in_str = false;
    for (i, c) in cond.char_indices() {
        if !in_str && depth == 0 && c != '"' && c != '(' && c != ')' {
            for op in &ops {
                if cond[i..].starts_with(op) {
                    let left = cond[..i].trim().to_string();
                    let right = cond[i + op.len()..].trim().to_string();
                    if !left.is_empty() && !right.is_empty() {
                        return Some((left, op.to_string(), right));
                    }
                }
            }
        }
        match c {
            '"' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => depth -= 1,
            _ => {}
        }
    }
    None
}

async fn eval_condition(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: u64,
    frame: &jdwp_client::thread::Frame,
    condition: &str,
) -> Result<bool, String> {
    eval_bool_tree_on_frame(conn, thread_id, frame, &parse_bool_tree(condition)).await
}

/// Evaluate a boolean tree against a frame, short-circuiting (EVAL-4): `||` stops at the first true
/// branch, `&&` at the first false — so a later, possibly more expensive clause isn't resolved unless
/// it's actually needed. Boxed because the tree is recursive.
fn eval_bool_tree_on_frame<'a>(
    conn: &'a mut jdwp_client::JdwpConnection,
    thread_id: u64,
    frame: &'a jdwp_client::thread::Frame,
    tree: &'a BoolTree,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + 'a>> {
    Box::pin(async move {
        match tree {
            BoolTree::Or(branches) => {
                for b in branches {
                    if eval_bool_tree_on_frame(conn, thread_id, frame, b).await? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            BoolTree::And(branches) => {
                for b in branches {
                    if !eval_bool_tree_on_frame(conn, thread_id, frame, b).await? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            BoolTree::Leaf(leaf) => eval_condition_leaf(conn, thread_id, frame, leaf).await,
        }
    })
}

/// Evaluate one leaf of a condition (a comparison or a boolean expression) on a frame — the original
/// single-clause condition logic, now a leaf so `&&`/`||` can compose several (EVAL-4).
async fn eval_condition_leaf(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: u64,
    frame: &jdwp_client::thread::Frame,
    leaf: &str,
) -> Result<bool, String> {
    if let Some((lhs, op, rhs)) = split_comparison(leaf) {
        let lv = resolve_expression(conn, Some(thread_id), Some(frame), &lhs).await?;
        // A non-literal right-hand side (`other.id`, `this.limit`) is resolved in the same frame and
        // compared value-to-value; literals keep their existing coercion path.
        match parse_lit(rhs.trim())? {
            ArgLit::Expr(e) => {
                let rv = resolve_expression(conn, Some(thread_id), Some(frame), &e).await?;
                compare_resolved(conn, &lv, &op, &rv).await
            }
            rlit => compare_values(conn, &lv, &op, &rlit).await,
        }
    } else {
        let v = resolve_expression(conn, Some(thread_id), Some(frame), leaf).await?;
        match v.data {
            jdwp_client::types::ValueData::Boolean(b) => Ok(b),
            _ => Err("Condition did not evaluate to a boolean".to_string()),
        }
    }
}

// Implements the debugger's numeric comparison operators (`==`, `!=`, `<`, …).
// Exact float equality is intentional here — it mirrors the source-level `==`
// the user typed, so an epsilon tolerance would give wrong answers. All numeric
// operands are normalized to f64 to be compared on one scale; widening an i64
// may lose precision for values above 2^53, which is acceptable for this
// best-effort comparison of debugger literals.
async fn compare_values(
    conn: &mut jdwp_client::JdwpConnection,
    lv: &jdwp_client::types::Value,
    op: &str,
    rlit: &ArgLit,
) -> Result<bool, String> {
    use jdwp_client::types::ValueData::{Boolean, Object};
    if let (Some(l), Some(r)) = (value_as_f64(&lv.data), arglit_as_f64(rlit)) {
        return compare_f64(l, r, op);
    }
    if let (Boolean(l), ArgLit::Bool(r)) = (&lv.data, rlit) {
        return match op {
            "==" => Ok(l == r),
            "!=" => Ok(l != r),
            _ => Err("only == / != for booleans".to_string()),
        };
    }
    if let Object(id) = &lv.data {
        return compare_object(conn, *id, op, rlit).await;
    }
    Err("Unsupported comparison (numbers, booleans, null, or String value compares only)".to_string())
}

/// Compare two already-resolved values — used when the right-hand side of a condition is an
/// expression rather than a literal. Numbers compare on one f64 scale, booleans with `==`/`!=`,
/// two Strings by content, and any other pair of references by identity.
async fn compare_resolved(
    conn: &mut jdwp_client::JdwpConnection,
    lv: &jdwp_client::types::Value,
    op: &str,
    rv: &jdwp_client::types::Value,
) -> Result<bool, String> {
    use jdwp_client::types::ValueData::{Boolean, Object};
    if let (Some(l), Some(r)) = (value_as_f64(&lv.data), value_as_f64(&rv.data)) {
        return compare_f64(l, r, op);
    }
    if let (Boolean(l), Boolean(r)) = (&lv.data, &rv.data) {
        return match op {
            "==" => Ok(l == r),
            "!=" => Ok(l != r),
            _ => Err("only == / != for booleans".to_string()),
        };
    }
    if let (Object(l), Object(r)) = (&lv.data, &rv.data) {
        if op != "==" && op != "!=" {
            return Err("only == / != when comparing objects".to_string());
        }
        // Two live Strings compare by content (what the user means by `s == other.name`); anything
        // else compares by reference identity, matching Java's own `==` on objects.
        let equal = match (string_value_of(conn, *l).await, string_value_of(conn, *r).await) {
            (Some(a), Some(b)) => a == b,
            _ => l == r,
        };
        return Ok(if op == "==" { equal } else { !equal });
    }
    Err("Unsupported comparison (compare numbers with numbers, booleans with booleans, or objects with objects)"
        .to_string())
}

/// The contents of `id` if it is a live `java.lang.String`; `None` for null, a non-String, or a
/// read failure.
async fn string_value_of(conn: &mut jdwp_client::JdwpConnection, id: u64) -> Option<String> {
    if id == 0 {
        return None;
    }
    let t = conn.get_object_reference_type(id).await.ok()?;
    if conn.get_signature(t).await.ok()? != "Ljava/lang/String;" {
        return None;
    }
    conn.get_string_value(id).await.ok()
}

/// A JDWP numeric value widened to f64 for comparison; `None` for non-numeric values. Widening an
/// i64 may lose precision above 2^53, acceptable for this best-effort comparison of debugger literals.
#[allow(clippy::cast_precision_loss)]
fn value_as_f64(data: &jdwp_client::types::ValueData) -> Option<f64> {
    use jdwp_client::types::ValueData::{Byte, Char, Double, Float, Int, Long, Short};
    Some(match data {
        Int(v) => f64::from(*v),
        Long(v) => *v as f64,
        Short(v) => f64::from(*v),
        Byte(v) => f64::from(*v),
        Char(v) => f64::from(*v),
        Float(v) => f64::from(*v),
        Double(v) => *v,
        _ => return None,
    })
}

/// A numeric literal widened to f64 for comparison; `None` for non-numeric literals.
#[allow(clippy::cast_precision_loss)]
fn arglit_as_f64(rlit: &ArgLit) -> Option<f64> {
    match rlit {
        ArgLit::Int(v) => Some(f64::from(*v)),
        ArgLit::Long(v) => Some(*v as f64),
        _ => None,
    }
}

/// Compare two f64 operands with the given operator. Exact float equality is intentional here — it
/// mirrors the source-level `==`/`!=` the user typed, so an epsilon tolerance would give wrong answers.
#[allow(clippy::float_cmp)]
fn compare_f64(l: f64, r: f64, op: &str) -> Result<bool, String> {
    Ok(match op {
        "==" => l == r,
        "!=" => l != r,
        "<" => l < r,
        ">" => l > r,
        "<=" => l <= r,
        ">=" => l >= r,
        _ => return Err("bad operator".to_string()),
    })
}

/// Compare an object value against a `null` or `String` literal (`==`/`!=` only).
async fn compare_object(
    conn: &mut jdwp_client::JdwpConnection,
    id: u64,
    op: &str,
    rlit: &ArgLit,
) -> Result<bool, String> {
    match rlit {
        ArgLit::Null => match op {
            "==" => Ok(id == 0),
            "!=" => Ok(id != 0),
            _ => Err("only == / != with null".to_string()),
        },
        ArgLit::Str(s) => {
            if id == 0 {
                return Ok(op == "!=");
            }
            let t = conn
                .get_object_reference_type(id)
                .await
                .map_err(|e| format!("Failed to resolve type: {e}"))?;
            if conn.get_signature(t).await.unwrap_or_default() == "Ljava/lang/String;" {
                let sv =
                    conn.get_string_value(id).await.map_err(|e| format!("Failed to read string: {e}"))?;
                match op {
                    "==" => Ok(&sv == s),
                    "!=" => Ok(&sv != s),
                    _ => Err("only == / != for strings".to_string()),
                }
            } else {
                Err("Left side is not a String".to_string())
            }
        }
        _ => {
            Err("Unsupported comparison (numbers, booleans, null, or String value compares only)".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a parsed boolean tree as a flat string so precedence/grouping can be asserted.
    fn shape(t: &BoolTree) -> String {
        match t {
            BoolTree::Or(v) => format!("OR({})", v.iter().map(shape).collect::<Vec<_>>().join(", ")),
            BoolTree::And(v) => format!("AND({})", v.iter().map(shape).collect::<Vec<_>>().join(", ")),
            BoolTree::Leaf(s) => s.clone(),
        }
    }

    // EVAL-4: `||` is lower precedence than `&&`, so `a || b && c` is `a || (b && c)`.
    #[test]
    fn boolean_precedence_puts_and_below_or() {
        assert_eq!(shape(&parse_bool_tree("a == 1 && b == 2")), "AND(a == 1, b == 2)");
        assert_eq!(shape(&parse_bool_tree("a == 1 || b == 2")), "OR(a == 1, b == 2)");
        assert_eq!(shape(&parse_bool_tree("a == 1 || b == 2 && c == 3")), "OR(a == 1, AND(b == 2, c == 3))");
    }

    // Parentheses regroup, overriding the default precedence.
    #[test]
    fn parentheses_regroup_a_boolean_expression() {
        assert_eq!(
            shape(&parse_bool_tree("(a == 1 || b == 2) && c == 3")),
            "AND(OR(a == 1, b == 2), c == 3)"
        );
        // A wholly-enclosed expression is unwrapped, not treated as a leaf.
        assert_eq!(shape(&parse_bool_tree("((a == 1))")), "a == 1");
    }

    // A splitter must ignore operators inside strings, parens and brackets.
    #[test]
    fn boolean_split_respects_quotes_and_brackets() {
        // The `||` lives inside a string literal, so it is not a top-level operator.
        assert_eq!(shape(&parse_bool_tree("name == \"a || b\"")), "name == \"a || b\"");
        // The `&&` is inside a subscript predicate, so the outer split leaves it alone.
        assert_eq!(shape(&parse_bool_tree("tags[?x && y] == 1")), "tags[?x && y] == 1");
    }

    // A plain comparison is a single leaf — the common case is unchanged by EVAL-4.
    #[test]
    fn a_plain_comparison_is_one_leaf() {
        assert_eq!(shape(&parse_bool_tree("qty > 3")), "qty > 3");
    }

    // TRACE-5: the caller depth is clamped, and a clamp is REPORTED rather than silently applied — an
    // ignored argument would leave a caller believing they had a deeper chain than they do.
    #[test]
    fn trace_frames_are_clamped_and_the_clamp_is_reported() {
        assert_eq!(clamp_trace_frames(true, 3), (3, None), "a depth within the cap passes through");
        assert_eq!(clamp_trace_frames(true, 0), (0, None), "0 is the one-frame snapshot, not a default");
        assert_eq!(
            clamp_trace_frames(true, MAX_TRACE_FRAMES),
            (MAX_TRACE_FRAMES, None),
            "the cap itself is allowed, so the boundary is inclusive"
        );

        let (depth, note) = clamp_trace_frames(true, MAX_TRACE_FRAMES + 1);
        assert_eq!(depth, MAX_TRACE_FRAMES);
        let note = note.expect("exceeding the cap must produce a note, not silence");
        assert!(note.contains("clamped"), "the note must say what happened: {note}");

        // A suspending stop point hands over a live thread, so `debug.get_stack` is the full-stack
        // answer and a snapshot depth has nothing to do.
        assert_eq!(clamp_trace_frames(false, 5), (0, None), "depth is meaningless without trace mode");
    }

    // TRACE-5: the depth is visible in `list_stop_points` (so a slowed debuggee is explainable), and
    // absent when there is nothing to report.
    #[test]
    fn trace_frames_tag_shows_only_a_real_depth() {
        assert_eq!(trace_frames_tag(true, 3), " [+3 caller frame(s)]");
        assert_eq!(trace_frames_tag(true, 0), "", "depth 0 adds no cost, so it advertises nothing");
        assert_eq!(trace_frames_tag(false, 3), "", "a non-traced stop point has no snapshot depth");
    }

    // TEST-8: the per-dump line-table cache is keyed by (class, method) and the LINE is resolved per frame
    // from the cached table. So the property that matters is that one table answers different bytecode
    // indexes differently — a cache that stored a resolved line instead would give every frame of a method
    // the same number, which still looks like a valid dump. No probe can construct two frames of one method
    // at different indexes on demand, so it is asserted here instead.
    #[test]
    fn one_cached_line_table_resolves_each_bytecode_index_to_its_own_line() {
        use jdwp_client::method::{LineTable, LineTableEntry};
        let lt = LineTable {
            start: 0,
            end: 40,
            lines: vec![
                LineTableEntry { line_code_index: 0, line_number: 10 },
                LineTableEntry { line_code_index: 8, line_number: 11 },
                LineTableEntry { line_code_index: 20, line_number: 14 },
            ],
        };
        // The covering entry is the last one at or before the index, not the nearest.
        assert_eq!(line_at(&lt, 0), Some(10));
        assert_eq!(line_at(&lt, 7), Some(10), "still inside line 10's range");
        assert_eq!(line_at(&lt, 8), Some(11));
        assert_eq!(line_at(&lt, 19), Some(11));
        assert_eq!(line_at(&lt, 20), Some(14));
        assert_eq!(line_at(&lt, 999), Some(14), "past the last entry is still that entry's line");

        // A table with no entries has no answer, which must not be confused with line 0.
        let empty = LineTable { start: 0, end: 0, lines: Vec::new() };
        assert_eq!(line_at(&empty, 0), None);
        // An index before the first entry — a synthetic or shifted table — is also no answer.
        let late = LineTable {
            start: 4,
            end: 8,
            lines: vec![LineTableEntry { line_code_index: 4, line_number: 7 }],
        };
        assert_eq!(line_at(&late, 0), None, "before the first entry, nothing covers the index");
    }

    // TRACE-7: the three states of a cost line. The middle one matters most — a traced stop point with no
    // hits must not render as one that costs nothing.
    #[test]
    fn trace_cost_reports_hits_absence_and_nothing_for_a_suspending_stop_point() {
        // A suspending stop point does no capture, so it has no capture cost to report.
        let mut out = String::new();
        render_trace_cost(&mut out, false, &crate::session::TraceCost::default());
        assert!(out.is_empty(), "a suspending stop point must report no capture cost: {out:?}");

        // Traced but never hit: unmeasured, and said so in those terms.
        let mut out = String::new();
        render_trace_cost(&mut out, true, &crate::session::TraceCost::default());
        assert!(out.contains("nothing captured yet"), "silence must not read as free: {out}");
        assert!(out.contains("UNMEASURED"), "the distinction has to be explicit: {out}");
        assert!(!out.contains("0.00ms"), "an unmeasured cost must not render as a zero one: {out}");

        // Ten captures of 1ms, 100ms apart: 1.00ms mean, ~1000/s sustainable, arriving at 10/s, so 1% of
        // the window went on capturing.
        let mut cost = crate::session::TraceCost::default();
        let t0 = std::time::Instant::now();
        for i in 0..10u32 {
            cost.record(
                t0 + std::time::Duration::from_millis(u64::from(i) * 100),
                std::time::Duration::from_millis(1),
            );
        }
        let mut out = String::new();
        render_trace_cost(&mut out, true, &cost);
        for want in ["10 capture(s)", "1.00ms mean", "arriving at 10.0/s", "(1.0% of the window"] {
            assert!(out.contains(want), "missing {want:?} in: {out}");
        }

        // A single capture prices a hit but cannot price a rate, and says which is missing.
        let mut one = crate::session::TraceCost::default();
        one.record(std::time::Instant::now(), std::time::Duration::from_millis(1));
        let mut out = String::new();
        render_trace_cost(&mut out, true, &one);
        assert!(out.contains("1 capture(s)"), "{out}");
        assert!(out.contains("no arrival rate yet"), "one hit must not imply a rate: {out}");
    }

    // TRACE-5: the chain renders as one readable run of arrows on the hit's own line, and adds nothing
    // when no callers were captured — the pre-TRACE-5 line stays byte-for-byte the same.
    #[test]
    fn caller_chain_renders_inline_and_vanishes_when_empty() {
        let mut rec = crate::session::TraceRecord {
            seq: 1,
            bp_id: "bp_1".to_string(),
            thread: 1,
            class: "Svc".to_string(),
            method: "save".to_string(),
            line: Some(10),
            args: Vec::new(),
            callers: Vec::new(),
            expr: None,
            detail: Vec::new(),
        };
        assert_eq!(format_trace_callers(&rec), "");

        rec.callers = vec!["Ctl.post:40".to_string(), "Http.run:12".to_string()];
        assert_eq!(format_trace_callers(&rec), " ← Ctl.post:40 ← Http.run:12");
    }

    /// A `DumpRow` with everything empty, for the render tests to fill in selectively.
    fn dump_row(id: u64, name: &str) -> DumpRow {
        DumpRow {
            id,
            name: name.to_string(),
            status: "monitor",
            suspended: true,
            finished: false,
            stack: DumpStack::Frames(vec!["#0 Svc.save:10".to_string()]),
            frames_hidden: 0,
            holds: Vec::new(),
            waiting_on: None,
            monitor_note: None,
        }
    }

    fn dump_args(json: serde_json::Value) -> crate::args::ThreadDumpArgs {
        serde_json::from_value(json).expect("valid ThreadDumpArgs")
    }

    /// A selection that left nothing out, so `family_order_note` stays silent — the render tests that are
    /// not about DUMP-3 keep the output they were written against.
    static WHOLE_POOL: FamilySelection = FamilySelection { eligible: 0, families: 0, withheld: Vec::new() };

    /// A `DumpMeta` for a dump that suspended nothing and completed — the fields each test varies are
    /// overridden at the call site, so a test only states what it is actually about.
    fn dump_meta(total: usize, cost: u32) -> DumpMeta<'static> {
        DumpMeta {
            total,
            already_suspended: false,
            resume_note: "",
            cost,
            // A round number against the `cost` each test passes, so a per-packet figure in an assertion is
            // arithmetic the reader can check rather than a magic constant.
            wire: std::time::Duration::from_millis(u64::from(cost)),
            held: None,
            unread: 0,
            vanished: 0,
            selection: &WHOLE_POOL,
        }
    }

    const ALL_CAPS: jdwp_client::vm::VmCapabilities = jdwp_client::vm::VmCapabilities {
        can_watch_field_modification: true,
        can_watch_field_access: true,
        can_get_bytecodes: true,
        can_get_synthetic_attribute: true,
        can_get_owned_monitor_info: true,
        can_get_current_contended_monitor: true,
        can_get_monitor_info: true,
    };

    // DUMP-3: a pool's threads differ only in the number on the end, and that is the one naming
    // convention every framework shares. Nothing here knows what WildFly or Tomcat call anything.
    #[test]
    fn a_thread_name_family_is_the_name_with_its_numbering_removed() {
        assert_eq!(thread_name_family("default task-17"), "default task-#");
        assert_eq!(thread_name_family("default task-17"), thread_name_family("default task-914"));
        // Different pools stay different, which is the half that makes the grouping useful rather than
        // merely small: collapsing selectors and request workers together would re-create the bug.
        assert_ne!(thread_name_family("default I/O-3"), thread_name_family("default task-3"));
        // Numbers in the middle count too — `MSC service thread 1-4`, `http-nio-8080-exec-3`.
        assert_eq!(thread_name_family("http-nio-8080-exec-3"), "http-nio-#-exec-#");
        assert_eq!(thread_name_family("MSC service thread 1-4"), "MSC service thread #-#");
        // A name with no digits is its own family, and an unnamed thread (a dead one reads as "") must
        // not panic its way out of a dump.
        assert_eq!(thread_name_family("Reference Handler"), "Reference Handler");
        assert_eq!(thread_name_family(""), "");
    }

    /// The `WildFly` roster from TEST-8 (#24), in `AllThreads` order: the JVM's own threads, then the
    /// service container, then the selectors, and the request pool last.
    fn wildfly_shaped_threads() -> Vec<String> {
        let mut names: Vec<String> =
            ["Reference Handler", "Finalizer", "Signal Dispatcher", "Common-Cleaner"]
                .iter()
                .map(|s| (*s).to_string())
                .collect();
        names.extend((1..=8).map(|i| format!("MSC service thread 1-{i}")));
        names.extend((1..=2).map(|i| format!("DeploymentScanner-threads - {i}")));
        names.extend((1..=38).map(|i| format!("ServerService Thread Pool -- {i}")));
        names.extend((1..=16).map(|i| format!("default I/O-{i}")));
        names.extend((1..=13).map(|i| format!("default task-{i}")));
        names
    }

    // DUMP-3 (#43), and the whole issue in one assertion. Against this roster the old rule — take the
    // first `limit` in `AllThreads` order — returned 40 threads containing ZERO `default task-*`, because
    // creation order puts an app server's request pool last. The measurement was taken on a real WildFly
    // and the arithmetic is reproduced here so a regression fails without a JVM.
    #[test]
    fn the_default_limit_reaches_a_request_pool_that_was_created_last() {
        let names = wildfly_shaped_threads();
        let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();

        // What it used to do. Pinned as the thing being fixed, not as a helper: if this ever stops being
        // zero the roster has drifted and the test below has stopped proving anything.
        let creation_order = borrowed.iter().take(40).filter(|n| n.starts_with("default task")).count();
        assert_eq!(creation_order, 0, "the roster must reproduce the finding, or the fix is untested");

        let (order, families) = family_round_robin(&borrowed);
        assert_eq!(families, 9, "four singletons plus MSC, DeploymentScanner, ServerService, I/O and task");
        let chosen: Vec<&str> = order.iter().take(40).map(|i| borrowed[*i]).collect();
        let pool = chosen.iter().filter(|n| n.starts_with("default task")).count();
        assert!(pool >= 5, "a default dump must reach the request pool, got {pool} of 13:\n{chosen:?}");
        // …and not by starving everything else: the point is a fair sample, not a different bias.
        assert!(chosen.iter().any(|n| n.starts_with("default I/O")), "selectors are still represented");
        assert!(chosen.contains(&"Finalizer"), "so are the JVM's own threads");

        // Every thread is still offered, exactly once — a selection rule that quietly dropped candidates
        // would make `limit: 500` unable to reach what `limit: 40` skipped.
        assert_eq!(order.len(), borrowed.len());
        let mut seen = order.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), borrowed.len(), "each thread appears in the order exactly once");
    }

    // A rule that only holds on a tidy list is not a rule. One family, an empty list, and a list of
    // singletons are all shapes a real JVM produces, and none of them may reorder into nonsense.
    #[test]
    fn family_round_robin_degenerates_to_creation_order_when_there_is_nothing_to_interleave() {
        let (order, families) = family_round_robin(&[]);
        assert!(order.is_empty());
        assert_eq!(families, 0);

        // `name_filter: "default task"` narrows to one pool; interleaving one family IS creation order,
        // which is what lets the header stay silent about a rule that did nothing.
        let one_pool = ["task-1", "task-2", "task-3"];
        assert_eq!(family_round_robin(&one_pool), (vec![0, 1, 2], 1));

        let all_distinct = ["main", "Finalizer", "collector"];
        assert_eq!(family_round_robin(&all_distinct), (vec![0, 1, 2], 3));
    }

    // DUMP-3: the caller must be able to know what the forty they got are without reading this file —
    // and must not be told about a rule that changed nothing.
    #[test]
    fn a_dump_states_its_selection_rule_only_when_the_rule_mattered() {
        let truncated = FamilySelection { eligible: 267, families: 25, withheld: Vec::new() };
        let note = family_order_note(40, &truncated);
        assert!(note.contains("Chose 40 of 267"), "the note states the arithmetic: {note}");
        assert!(note.contains("NAME FAMILY"), "and the rule: {note}");
        assert!(note.contains("printed in creation order"), "and how to read the rows: {note}");

        // Nothing was left out, so there is no "which forty" to answer.
        let whole = FamilySelection { eligible: 12, families: 5, withheld: Vec::new() };
        assert_eq!(family_order_note(12, &whole), "");
        // One family: round-robin over it is creation order, so announcing it would be noise.
        let narrowed = FamilySelection { eligible: 300, families: 1, withheld: Vec::new() };
        assert_eq!(family_order_note(40, &narrowed), "");
    }

    // DUMP-5 (#51): `list_threads` now reads every thread's name before choosing, and the criterion for
    // that change was never "is it fair" alone — it was "is it still the CHEAP call". A reply that widened
    // its reads and did not say so would be asking to be trusted about the one property this tool is for.
    #[test]
    fn a_truncated_listing_reports_what_choosing_by_family_cost_it() {
        let wire = std::time::Duration::from_millis(112);
        let note = list_cost_note(268, wire, 40, false);
        assert!(note.contains("268 JDWP packet(s)"), "the number it actually spent: {note}");
        assert!(note.contains("0.42ms each"), "priced on THIS connection, not on loopback: {note}");
        // The counterfactual is arithmetic, not an estimate: the loop this replaced read one name per row
        // it printed, so 41 is exactly what the old behaviour would have cost on the same call.
        assert!(note.contains("would have cost 41"), "and what the old rule would have cost: {note}");
        assert!(note.contains("wrong 40"), "a cost is only half the trade — say what it bought: {note}");

        // A filtered listing always read every name to apply the filter, so selection added nothing to
        // it. Offering the same saving here would be inventing one.
        let filtered = list_cost_note(268, wire, 40, true);
        assert!(filtered.contains("costs it nothing extra"), "{filtered}");
        assert!(!filtered.contains("would have cost"), "no counterfactual where none applies: {filtered}");

        // One packet is not a mean, and the dump suppresses the per-packet figure for the same reason.
        assert!(!list_cost_note(1, wire, 0, false).contains("ms each"));
    }

    // "227 more" answers "is this short?"; naming the groups answers "short of WHAT?", which is the
    // question that decides between raising `limit` and reaching for `name_filter`.
    #[test]
    fn the_truncation_footer_names_the_groups_it_withheld_and_stops_at_five() {
        let nothing: Vec<(String, usize)> = Vec::new();
        assert_eq!(withheld_note(&nothing), "");

        let pair = [("default task-#".to_string(), 11), ("default I/O-#".to_string(), 14)];
        let listed = withheld_note(&pair);
        assert!(listed.contains("11 × \"default task-#\""), "{listed}");
        assert!(listed.contains("14 × \"default I/O-#\""), "{listed}");
        assert!(!listed.contains("other group(s)"), "two groups is not a truncated list: {listed}");

        // A 267-thread JVM has more families than anyone reads in a footer, so the tail is counted.
        let many: Vec<(String, usize)> = (0..9).map(|i| (format!("pool-{i}-#"), 9 - i)).collect();
        let long = withheld_note(&many);
        assert!(long.contains("and 4 other group(s)"), "the rest are counted, not dropped: {long}");
        assert!(!long.contains("pool-6-#"), "the sixth group is past the cap: {long}");
    }

    // DUMP-1: the whole point of collecting monitors is the correlation — "A waits for L" plus "B holds
    // L" has to render as one readable fact, or a deadlock stays invisible in a correct-looking dump.
    #[test]
    fn thread_dump_names_the_holder_of_a_contended_lock() {
        let mut one = dump_row(0x8, "deadlock-one");
        one.holds = vec![("LockA@d".to_string(), 0xd)];
        one.waiting_on = Some(("LockB@f".to_string(), 0xf));
        let mut two = dump_row(0x9, "deadlock-two");
        two.holds = vec![("LockB@f".to_string(), 0xf)];
        two.waiting_on = Some(("LockA@d".to_string(), 0xd));

        let out = render_thread_dump(&[one, two], &dump_args(json!({})), Some(&ALL_CAPS), &dump_meta(2, 44));
        assert!(
            out.contains("waiting to enter: LockB@f ← held by 0x9 \"deadlock-two\""),
            "the cycle's first half must name its holder:\n{out}"
        );
        assert!(
            out.contains("waiting to enter: LockA@d ← held by 0x8 \"deadlock-one\""),
            "the cycle's second half must name its holder:\n{out}"
        );
        assert!(out.contains("Cost: 44 JDWP packet(s)"), "the round-trip cost is reported:\n{out}");
    }

    // A lock whose holder is not in the dump (filtered out, or past `limit`) must be reported WITHOUT a
    // holder rather than with a wrong one — the annotation is only as good as the rows it was built from.
    #[test]
    fn thread_dump_omits_the_holder_when_it_is_not_in_the_dump() {
        let mut one = dump_row(0x8, "deadlock-one");
        one.waiting_on = Some(("LockB@f".to_string(), 0xf));
        let out =
            render_thread_dump(&[one], &dump_args(json!({"limit": 1})), Some(&ALL_CAPS), &dump_meta(9, 10));
        assert!(out.contains("waiting to enter: LockB@f"), "the contended lock is still shown:\n{out}");
        assert!(!out.contains("held by"), "no holder may be invented for a thread not dumped:\n{out}");
        assert!(out.contains("+8 more thread(s)"), "the threads left out are counted:\n{out}");
        assert!(out.contains("raise limit"), "…and a genuine `limit` truncation still says so:\n{out}");
    }

    // DUMP-4 (#47): "running" and "finished" are opposite answers, and a churning pool is where the dump
    // was picking the wrong one — the JVM had just said ZOMBIE and the row said `running — … pass
    // suspend:true`, which is unfollowable because a finished thread can never be suspended. ADR-0009
    // makes the same point in the other direction: a running thread is never rendered as `(no frames)`.
    #[test]
    fn a_finished_thread_says_so_and_is_not_offered_a_suspend_that_cannot_help() {
        let running = unreadable_reason(false, false);
        assert!(
            running.starts_with("running —"),
            "a live unsuspended thread still reads as running: {running}"
        );
        assert!(running.contains("pass suspend:true"), "…with the remedy that does work: {running}");

        let finished = unreadable_reason(true, false);
        assert!(finished.starts_with("finished —"), "a ZOMBIE thread has finished, not started: {finished}");
        assert!(finished.contains("ZOMBIE"), "and names the answer the JVM actually gave: {finished}");
        assert!(
            !finished.contains("pass suspend:true"),
            "suspending a finished thread is impossible, so the advice must not be offered: {finished}"
        );

        // Which noun is named still follows what the caller asked for, in both states.
        assert!(unreadable_reason(false, true).contains("locks"), "monitors-only names locks, not a stack");
        assert!(unreadable_reason(true, true).contains("locks"), "…and still does once the thread has ended");
    }

    // …and the header's offer is counted over the threads it could actually rescue. A finished thread in
    // that tally would inflate "pass suspend:true and you get N more stacks" with rows that will never
    // come back.
    #[test]
    fn the_suspend_offer_counts_only_the_threads_a_suspend_would_rescue() {
        let mut zombie = dump_row(0x8, "churn-worker-3");
        zombie.status = "zombie";
        zombie.suspended = false;
        zombie.finished = true;
        zombie.stack = DumpStack::Unreadable(unreadable_reason(true, false));
        let mut live = dump_row(0x9, "stable-worker-1");
        live.status = "running";
        live.suspended = false;
        live.stack = DumpStack::Unreadable(unreadable_reason(false, false));

        let out =
            render_thread_dump(&[zombie, live], &dump_args(json!({})), Some(&ALL_CAPS), &dump_meta(2, 9));
        assert!(out.contains("1 thread(s) are running"), "only the live thread is offered a freeze:\n{out}");
        assert!(
            out.contains("finished — this thread has already terminated"),
            "and the finished one says what it is on its own row:\n{out}"
        );
    }

    // DUMP-4 (#47): with `limit: 500` against 63 threads, 41 rows were missing because those threads had
    // DIED mid-read — and the only explanation offered was "raise limit, or narrow with name_filter",
    // two remedies that cannot change the outcome. Counted apart now, and the two counts still sum to
    // the shortfall the header's arithmetic promises.
    #[test]
    fn rows_lost_to_dying_threads_are_reported_apart_from_rows_the_limit_withheld() {
        let rows: Vec<DumpRow> = (0..22).map(|i| dump_row(i, &format!("stable-worker-{i}"))).collect();

        // The churn case exactly as TEST-10 produced it: 63 listed, 22 read, 41 gone, `limit` untouched.
        let mut churned = dump_meta(63, 130);
        churned.vanished = 41;
        let out = render_thread_dump(&rows, &dump_args(json!({"limit": 500})), Some(&ALL_CAPS), &churned);
        assert!(
            out.contains("… +41 more thread(s) ENDED while this dump was reading"),
            "the 41 that died are counted, and the cause is named:\n{out}"
        );
        assert!(
            !out.contains("raise limit"),
            "`limit` was 500 against 63 threads and never bound, so advising it is a no-op:\n{out}"
        );
        assert!(
            !out.contains("name_filter"),
            "narrowing cannot bring back a thread that no longer exists:\n{out}"
        );

        // Both causes at once. Neither absorbs the other, and 12 + 41 is still the 53 not shown.
        let mut both = dump_meta(63, 130);
        both.vanished = 41;
        let mixed = render_thread_dump(&rows[..10], &dump_args(json!({"limit": 10})), Some(&ALL_CAPS), &both);
        assert!(
            mixed.contains("… +12 more thread(s) (raise limit, or narrow with name_filter)"),
            "the rows the limit really withheld keep their own line and their own advice:\n{mixed}"
        );
        assert!(
            mixed.contains("… +41 more thread(s) ENDED while this dump was reading"),
            "and the ones that died keep theirs:\n{mixed}"
        );
    }

    // DUMP-1: a JVM that cannot answer the monitor questions must SAY so. A dump with no lock lines
    // otherwise reads as "nothing is contended", which is the opposite of the truth.
    #[test]
    fn thread_dump_reports_a_jvm_that_cannot_do_monitors() {
        let caps = jdwp_client::vm::VmCapabilities {
            can_get_owned_monitor_info: false,
            can_get_current_contended_monitor: false,
            ..ALL_CAPS
        };
        let out = render_thread_dump(
            &[dump_row(0x8, "worker")],
            &dump_args(json!({})),
            Some(&caps),
            &dump_meta(1, 5),
        );
        assert!(out.contains("cannot report all monitor info"), "the gap must be stated:\n{out}");
        assert!(out.contains("canGetOwnedMonitorInfo=false"), "and named precisely:\n{out}");

        // Capabilities unreadable is its own case, and must not silently look like "no locks held".
        let unknown =
            render_thread_dump(&[dump_row(0x8, "worker")], &dump_args(json!({})), None, &dump_meta(1, 5));
        assert!(unknown.contains("monitors were skipped"), "an unknown capability set is stated:\n{unknown}");

        // ...but a dump that never asked for monitors says nothing about them at all.
        let off = render_thread_dump(
            &[dump_row(0x8, "worker")],
            &dump_args(json!({"monitors": false})),
            None,
            &dump_meta(1, 5),
        );
        assert!(!off.contains("monitor info"), "monitors:false should not editorialise:\n{off}");
    }

    // #17 item 3: monitors-only reads the lock graph and skips the frames. The rendering has to keep
    // "omitted by request" apart from the two states it superficially resembles — a thread with no
    // frames, and a thread whose frames could not be read — because both of those are findings and this
    // is not.
    #[test]
    fn thread_dump_monitors_only_omits_stacks_without_claiming_there_are_none() {
        let mut one = dump_row(0x8, "deadlock-one");
        one.stack = DumpStack::Omitted;
        one.holds = vec![("LockA@d".to_string(), 0xd)];
        one.waiting_on = Some(("LockB@f".to_string(), 0xf));
        let mut two = dump_row(0x9, "deadlock-two");
        two.stack = DumpStack::Omitted;
        two.holds = vec![("LockB@f".to_string(), 0xf)];

        let args = dump_args(json!({"monitors_only": true}));
        let out = render_thread_dump(&[one, two], &args, Some(&ALL_CAPS), &dump_meta(2, 4));

        assert!(out.contains("monitors-only"), "the mode is named in the header:\n{out}");
        assert!(
            out.contains("\"not requested\""),
            "an absent stack must be attributed to the request, not to the thread:\n{out}"
        );
        assert!(!out.contains("(no frames)"), "omitted must not render as a frameless thread:\n{out}");
        assert!(!out.contains("⚠️"), "omitted must not render as a failed read:\n{out}");
        // The cheap mode still has to answer the question it exists for (#17 story 22).
        assert!(
            out.contains("waiting to enter: LockB@f ← held by 0x9 \"deadlock-two\""),
            "the blocker of a contended lock is named without any stacks:\n{out}"
        );
    }

    // #17 story 23: in monitors-only mode the locks ARE the payload, so a JVM that cannot report any of
    // them returns a dump with nothing in it. The existing "cannot report all monitor info" warning is
    // too soft for that case — an empty cheap dump reads as "nothing is contended" unless it is told
    // otherwise. No HotSpot exercises this, which is why it is unit-tested.
    #[test]
    fn thread_dump_monitors_only_on_a_jvm_without_monitors_says_it_has_no_payload() {
        let caps = jdwp_client::vm::VmCapabilities {
            can_get_owned_monitor_info: false,
            can_get_current_contended_monitor: false,
            ..ALL_CAPS
        };
        let mut row = dump_row(0x8, "worker");
        row.stack = DumpStack::Omitted;
        let args = dump_args(json!({"monitors_only": true}));
        let out = render_thread_dump(&[row], &args, Some(&caps), &dump_meta(1, 2));

        assert!(out.contains("cannot report all monitor info"), "the gap is still named:\n{out}");
        assert!(out.contains("NO lock payload"), "and its consequence here is stated:\n{out}");
        assert!(
            out.contains("says nothing about contention"),
            "the emptiness must be disclaimed, not left to be read as an answer:\n{out}"
        );

        // Capabilities unreadable is the same no-payload state, reached a different way.
        let mut row = dump_row(0x8, "worker");
        row.stack = DumpStack::Omitted;
        let unknown = render_thread_dump(&[row], &args, None, &dump_meta(1, 2));
        assert!(
            unknown.contains("NO lock payload"),
            "an unknown capability set is no payload too:\n{unknown}"
        );

        // A capable JVM says none of this.
        let mut row = dump_row(0x8, "worker");
        row.stack = DumpStack::Omitted;
        let fine = render_thread_dump(&[row], &args, Some(&ALL_CAPS), &dump_meta(1, 2));
        assert!(!fine.contains("NO lock payload"), "a capable JVM must not be disclaimed:\n{fine}");
    }

    // #17 story 21: monitors-only composes with the thread filters, but a FRAME filter is inert here.
    // Echoing it in the header as though it had applied would credit the dump with a narrowing it never
    // performed — the same silence-as-an-answer failure in miniature.
    #[test]
    fn thread_dump_monitors_only_reports_a_frame_filter_as_ignored() {
        let mut row = dump_row(0x8, "default task-1");
        row.stack = DumpStack::Omitted;
        let args = dump_args(json!({
            "monitors_only": true, "package_filter": "com.acme", "name_filter": "default task"
        }));
        let out = render_thread_dump(&[row], &args, Some(&ALL_CAPS), &dump_meta(60, 2));

        assert!(out.contains("name~\"default task\""), "a thread filter still applies:\n{out}");
        assert!(
            !out.contains("frames~\"com.acme\""),
            "an inert frame filter must not read as applied:\n{out}"
        );
        assert!(out.contains("had no effect"), "it is reported as ignored instead:\n{out}");

        // Without monitors_only the same filter is real, and is echoed.
        let with_frames = render_thread_dump(
            &[dump_row(0x8, "default task-1")],
            &dump_args(json!({"package_filter": "com.acme"})),
            Some(&ALL_CAPS),
            &dump_meta(60, 2),
        );
        assert!(with_frames.contains("frames~\"com.acme\""), "a real frame filter is echoed:\n{with_frames}");
        assert!(!with_frames.contains("had no effect"), "and not disclaimed:\n{with_frames}");
    }

    // #17: the held duration is reported whenever this dump owned the freeze, and NOT when it didn't —
    // a dump that suspended nothing must not appear to have frozen the VM for 0ms, which reads as a
    // measurement rather than an absence.
    #[test]
    fn thread_dump_reports_the_held_duration_only_when_it_held_the_vm() {
        let mut meta = dump_meta(1, 5);
        meta.held = Some(std::time::Duration::from_millis(137));
        let held = render_thread_dump(&[dump_row(0x8, "worker")], &dump_args(json!({})), None, &meta);
        assert!(held.contains("Held the VM suspended for 137ms"), "the real number is reported:\n{held}");

        let not_held =
            render_thread_dump(&[dump_row(0x8, "worker")], &dump_args(json!({})), None, &dump_meta(1, 5));
        assert!(
            !not_held.contains("Held the VM"),
            "a dump that suspended nothing must claim no freeze:\n{not_held}"
        );
    }

    // TEST-8: a dump reports what its OWN connection costs per packet, because the ~0.2ms in this repo's
    // notes is a loopback figure and the round trip is the term that changes on a real instance. This is
    // the reading #24 would otherwise have needed a human to take by hand.
    #[test]
    fn a_dump_reports_its_own_observed_per_packet_cost() {
        // 500 packets over 1000ms is 2.00ms each — arithmetic the reader can check.
        let mut meta = dump_meta(1, 500);
        meta.wire = std::time::Duration::from_secs(1);
        let out = render_thread_dump(&[dump_row(0x8, "worker")], &dump_args(json!({})), None, &meta);
        assert!(out.contains("Cost: 500 JDWP packet(s), 2.00ms each"), "per-packet price missing:\n{out}");
        assert!(
            out.contains("round trip + our own processing"),
            "it must say what the figure covers:\n{out}"
        );

        // One packet is not a sample: a "mean" over it would be a number pretending to be a measurement.
        let mut single = dump_meta(1, 1);
        single.wire = std::time::Duration::from_millis(7);
        let thin = render_thread_dump(&[dump_row(0x8, "worker")], &dump_args(json!({})), None, &single);
        assert!(thin.contains("Cost: 1 JDWP packet(s)."), "expected a bare cost line:\n{thin}");
        assert!(!thin.contains("each"), "a single packet must not carry a mean:\n{thin}");
    }

    // TEST-8: a truncated dump says what finishing would have cost, extrapolated from its own rate. That
    // is the number that chooses between the two ways out — narrow it, or raise the budget — and #24 was
    // otherwise going to ask a human to do this arithmetic against a live instance.
    #[test]
    fn a_truncated_dump_estimates_what_the_rest_would_have_cost() {
        // 10 threads read in 1000ms is 100ms each; 20 skipped is ~2000ms more, ~3000ms for the whole set.
        let mut meta = dump_meta(30, 900);
        meta.held = Some(std::time::Duration::from_secs(1));
        meta.unread = 20;
        let rows: Vec<DumpRow> = (0..10).map(|i| dump_row(0x8 + i, "worker")).collect();
        let out = render_thread_dump(&rows, &dump_args(json!({"suspend": true})), None, &meta);
        assert!(out.contains("100.0ms per thread"), "the observed rate must be stated:\n{out}");
        assert!(out.contains("~2000ms more"), "and what the skipped threads need:\n{out}");
        assert!(out.contains("about 3000ms for the whole set"), "and the total:\n{out}");
        assert!(out.contains("20 threads"), "and how many were skipped:\n{out}");

        // A dump that read nothing has no rate to extrapolate from, so it must not invent one.
        let mut nothing = dump_meta(30, 4);
        nothing.held = Some(std::time::Duration::from_millis(2001));
        nothing.unread = 30;
        let empty = render_thread_dump(&[], &dump_args(json!({"suspend": true})), None, &nothing);
        assert!(empty.contains("Stopped early"), "the truncation is still announced:\n{empty}");
        assert!(!empty.contains("per thread"), "with no rows there is no rate to report:\n{empty}");

        // And a complete dump never speculates about threads it did not skip.
        let mut done = dump_meta(1, 5);
        done.held = Some(std::time::Duration::from_millis(12));
        let complete = render_thread_dump(&[dump_row(0x8, "w")], &dump_args(json!({})), None, &done);
        assert!(!complete.contains("per thread"), "nothing was skipped:\n{complete}");
    }

    // #17: an exhausted budget is announced, names what it skipped, and says the dump is INCOMPLETE.
    // Silence here would be the worst outcome — a truncated dump reads as "these are all the threads".
    #[test]
    fn thread_dump_announces_a_budget_truncation_and_never_implies_completeness() {
        let mut meta = dump_meta(60, 900);
        meta.held = Some(std::time::Duration::from_millis(2001));
        meta.unread = 47;
        let out = render_thread_dump(
            &[dump_row(0x8, "worker-0")],
            &dump_args(json!({"suspend": true})),
            None,
            &meta,
        );
        assert!(out.contains("Stopped early"), "the truncation must be stated:\n{out}");
        assert!(out.contains("47 thread(s) still"), "and name how many it skipped:\n{out}");
        assert!(out.contains("INCOMPLETE"), "and refuse to look complete:\n{out}");
        assert!(out.contains("max_suspend_ms"), "and say which knob to turn:\n{out}");

        // A completed dump says none of it.
        let mut done = dump_meta(1, 5);
        done.held = Some(std::time::Duration::from_millis(12));
        let complete = render_thread_dump(&[dump_row(0x8, "worker")], &dump_args(json!({})), None, &done);
        assert!(!complete.contains("Stopped early"), "a complete dump must not warn:\n{complete}");
    }

    // The two thread states are independent axes, and the row must not run them together: `monitor` is
    // the application blocking on a lock, `debugger-suspended` is us holding it. `[monitor, suspended]`
    // invited "suspended at a monitor", which credits the freeze to the wrong party.
    #[test]
    fn a_dump_row_keeps_blocked_and_debugger_suspended_apart() {
        let out = render_thread_dump(
            &[dump_row(0x8, "deadlock-one")],
            &dump_args(json!({})),
            None,
            &dump_meta(1, 5),
        );
        assert!(out.contains("[monitor] debugger-suspended"), "the axes must read separately:\n{out}");
        assert!(!out.contains("[monitor, suspended]"), "the old ambiguous form must be gone:\n{out}");

        let mut running = dump_row(0x9, "http-listener");
        running.suspended = false;
        running.status = "running";
        let out = render_thread_dump(&[running], &dump_args(json!({})), None, &dump_meta(1, 5));
        assert!(out.contains("[running]"), "an unsuspended thread shows only its own state:\n{out}");
        assert!(!out.contains("debugger-suspended"), "and is not labelled as held:\n{out}");
    }

    // SAFE-4: an unreadable thread is reported on its own line with what would fix it, and the reply
    // must never imply the dump was complete. A running VM is the default case, so this is the norm.
    #[test]
    fn thread_dump_explains_an_unreadable_running_thread() {
        let mut row = dump_row(0x8, "http-listener");
        row.suspended = false;
        row.status = "running";
        row.stack =
            DumpStack::Unreadable("running — JDWP can only read a suspended thread's stack".to_string());

        let out = render_thread_dump(&[row], &dump_args(json!({})), None, &dump_meta(1, 4));
        assert!(out.contains("1 thread(s) are running"), "the count of unreadable threads is stated:\n{out}");
        assert!(out.contains("suspend:true"), "and how to get a full dump:\n{out}");
        assert!(!out.contains("(no frames)"), "unreadable must not render as an idle thread:\n{out}");
    }

    // SAFE-3: an expression that calls a method is detected (so read-only can refuse it), and a `(`
    // inside a string literal is not mistaken for a call.
    #[test]
    fn expr_invokes_detects_method_calls_only() {
        assert!(expr_invokes("order.getQty()"));
        assert!(expr_invokes("a.b(c)"));
        assert!(!expr_invokes("order.status"));
        assert!(!expr_invokes("order.lines[0].sku"));
        assert!(!expr_invokes("name == \"(not a call)\""));
    }

    // TRACE-3: the budget defaults to 200 when tracing, `0` means unbounded, and a non-trace stop
    // point is always unbounded (it suspends, so it can't flood).
    #[test]
    fn trace_budget_defaults_and_zero_means_unbounded() {
        assert_eq!(trace_budget_for(true, None), Some(DEFAULT_TRACE_BUDGET));
        assert_eq!(trace_budget_for(true, Some(5)), Some(5));
        assert_eq!(trace_budget_for(true, Some(0)), None);
        assert_eq!(trace_budget_for(false, Some(5)), None);
    }

    // #22: an unbounded budget is the one setting that turns a bounded blip into sustained
    // degradation, and it used to be the one the arm reply said nothing about. Silence there reads as
    // "nothing worth mentioning", which is the opposite of true.
    #[test]
    fn an_unbounded_trace_budget_is_warned_about_rather_than_passed_over() {
        let unbounded = describe_trace_budget(true, None);
        assert!(unbounded.contains("UNBOUNDED"), "the state is named:\n{unbounded}");
        assert!(unbounded.contains("trace_max_hits: 0"), "and attributed to the argument:\n{unbounded}");
        assert!(
            unbounded.contains("720 hits/s"),
            "with the ceiling as a number, not an adjective:\n{unbounded}"
        );

        // A bounded budget is reported plainly — the warning must not fire on the safe default.
        let bounded = describe_trace_budget(true, Some(DEFAULT_TRACE_BUDGET));
        assert!(bounded.contains("Auto-disarms after 200"), "the budget is stated:\n{bounded}");
        assert!(!bounded.contains("UNBOUNDED"), "and not editorialised:\n{bounded}");

        // A suspending stop point has no trace budget to report, and must claim none. `None` here means
        // "not tracing", not "unbounded" — the same value standing for two different states is exactly
        // why this case is asserted.
        assert!(describe_trace_budget(false, None).is_empty(), "a non-traced stop point says nothing");
    }

    // SAFE-6: an invoking condition/trace_expr is refused at arm time, but only in a read-only session
    // and only when it actually invokes — a field comparison must still be allowed.
    #[test]
    fn readonly_refuses_invoking_conditions_at_arm_time() {
        assert!(check_readonly_exprs(true, Some("order.getTotal() > 1"), None).is_err());
        assert!(check_readonly_exprs(true, None, Some("this.toString()")).is_err());
        // A comparison over plain fields invokes nothing, so it is fine even read-only.
        assert!(check_readonly_exprs(true, Some("status == \"OPEN\""), None).is_ok());
        // Nothing is restricted when the session is writable.
        assert!(check_readonly_exprs(false, Some("order.getTotal() > 1"), None).is_ok());
        // The message names which of the two was at fault, so the caller knows what to change.
        let e = check_readonly_exprs(true, None, Some("x.y()")).unwrap_err();
        assert!(e.contains("trace_expr"), "should name the offending field: {e}");
    }

    // SAFE-6: a read-only refusal from the wire is turned into an actionable explanation; anything
    // else passes through untouched.
    #[test]
    fn readonly_errors_are_explained_and_others_are_not() {
        let explained = explain_readonly(
            "invoke toString() failed: read-only connection: refusing to invoke an instance method".into(),
        );
        assert!(explained.contains("Read-only session"));
        assert!(explained.contains("locals, fields, statics"), "must say what still works: {explained}");
        let untouched = explain_readonly("Unknown local variable 'foo'".to_string());
        assert_eq!(untouched, "Unknown local variable 'foo'");
    }

    // SAFE-3: JDWP_READONLY parsing accepts the common truthy spellings and nothing else.
    #[test]
    fn env_readonly_parsing() {
        for v in ["1", "true", "TRUE", "yes", " Yes "] {
            std::env::set_var("JDWP_READONLY", v);
            assert!(env_readonly(), "{v:?} should be truthy");
        }
        for v in ["0", "false", "no", ""] {
            std::env::set_var("JDWP_READONLY", v);
            assert!(!env_readonly(), "{v:?} should be falsey");
        }
        std::env::remove_var("JDWP_READONLY");
    }

    // DISC-1: the three filter shapes are anchored differently, and a prefix must not behave as a
    // substring — `com.example.*` matching `org.acme.com.example.Foo` would be wrong.
    #[test]
    fn class_filter_anchors_prefix_suffix_and_substring() {
        assert!(class_matches("com.example.Order", "com.example.*"));
        assert!(!class_matches("org.acme.com.example.Order", "com.example.*"));

        assert!(class_matches("com.example.OrderService", "*.OrderService"));
        assert!(!class_matches("com.example.OrderServiceImpl", "*.OrderService"));

        assert!(class_matches("com.example.OrderService", "Order"));
        assert!(class_matches("com.example.OrderService", "example.Order"));
        assert!(!class_matches("com.example.OrderService", "Ordr"));

        // Both anchors is a substring test, not an impossible starts-and-ends-with.
        assert!(class_matches("com.example.OrderService", "*Order*"));
    }

    // DISC-1: `*.Order` should still find a top-level Order in the default package, where there is no
    // dot to match. Missing it silently is the failure mode worth a test.
    #[test]
    fn class_filter_suffix_finds_a_default_package_class() {
        assert!(class_matches("Order", "*.Order"));
        assert!(!class_matches("Reorder", "*.Order"));
    }

    // SIG-1 (#46): the second half, and the dangerous one. `debug.list_classes` used to explain every
    // miss with class loading, including the misses it had caused itself by renaming the class. It now
    // checks before it blames, and only offers the open readings when the reading is genuinely open.
    #[test]
    fn a_miss_is_never_blamed_on_class_loading_when_the_class_is_loaded() {
        let loaded = [
            ("SyntheticProbe$$Lambda/0x0000000087040970".to_string(), false),
            ("SyntheticProbe$$Lambda$3/397187020".to_string(), false),
            ("com.example.Order".to_string(), false),
        ];

        // The spelling this tool handed out before the fix, and the JVM's internal form. Both are misses
        // and both are about spelling, so neither may mention loading.
        for filter in ["SyntheticProbe$$Lambda.0x0000000087040970", "com/example/Order"] {
            let miss = explain_no_match(&loaded, Some(filter));
            assert!(
                miss.contains("spelling difference"),
                "`{filter}` names a class that is loaded, so the reply must say so: {miss}"
            );
            assert!(
                !miss.contains("not be loaded") && !miss.contains("not loaded"),
                "`{filter}` must not be explained away as a class the JVM has not loaded: {miss}"
            );
        }
        // …and the class it does name comes back, so the caller has somewhere to go.
        assert!(
            explain_no_match(&loaded, Some("SyntheticProbe$$Lambda$3.397187020"))
                .contains("SyntheticProbe$$Lambda$3/397187020"),
            "the reply has to hand back the spelling that works"
        );

        // A name nothing matches under any spelling is the case JDWP genuinely cannot resolve, and there
        // all three readings are offered rather than one picked — CONTEXT.md's rule under **Loaded**.
        let open = explain_no_match(&loaded, Some("com.example.Invoice"));
        assert!(open.contains("may not be loaded"), "the loading reading must still be offered: {open}");
        assert!(open.contains("no such class"), "so must the no-such-class reading: {open}");
        assert!(open.contains("spelled differently"), "and so must the spelling one: {open}");
    }

    // SIG-1 (#46): a lambda's generated class is named `<class>/<suffix>` everywhere outside this tool —
    // `Class.getName()`, a stack trace, a jstack dump — and it used to be rendered `<class>.<suffix>`,
    // which is not a name the JVM will answer to.
    //
    // Every descriptor below was **read off a live JVM**, not invented: the two shapes differ between
    // JDK versions and the second one is not the shape the issue describes. Guessing here is exactly how
    // #36's matrix caught the previous assertion passing on 21 and failing on 11.
    #[test]
    fn a_hidden_class_is_named_the_way_the_jvm_names_it() {
        // JDK 15+ (measured on 21): a real hidden class. The JDK writes a DOT on the wire, because a `/`
        // would not be a legal descriptor — so the separator arrives already replaced and is put back.
        assert_eq!(
            decode_signature("LSyntheticProbe$$Lambda.0x0000000092040970;"),
            "SyntheticProbe$$Lambda/0x0000000092040970"
        );
        // JDK 11 (measured): a VM-anonymous class, which predates hidden classes — an ordinal before a
        // SLASH and a plain decimal after it. This is the one the unconditional rewrite corrupted.
        assert_eq!(
            decode_signature("LSyntheticProbe$$Lambda$3/574182878;"),
            "SyntheticProbe$$Lambda$3/574182878"
        );
        // In a package both separators appear in one name, and each has to be read for what it is.
        assert_eq!(
            decode_signature("Ljava/lang/invoke/LambdaForm$MH.0x00007f2c4c0a1800;"),
            "java.lang.invoke.LambdaForm$MH/0x00007f2c4c0a1800"
        );
        assert_eq!(
            decode_signature("Lcom/example/Handler$$Lambda$7/1234567;"),
            "com.example.Handler$$Lambda$7/1234567"
        );
        // An array of one still gets its `[]`, because the suffix is part of the element's name.
        assert_eq!(
            decode_signature("[Lcom/example/Handler$$Lambda.0x00007f2c;"),
            "com.example.Handler$$Lambda/0x00007f2c[]"
        );
    }

    // SIG-1 (#46): the other half of the same rule. An ordinary `/` is still a package separator, and an
    // anonymous inner class was never affected — it is `Outer$1`, a `$` the rewrite never touched — so
    // this pins that the fix did not go looking for work it did not have.
    #[test]
    fn ordinary_and_anonymous_class_names_are_untouched() {
        assert_eq!(decode_signature("Lcom/example/Order;"), "com.example.Order");
        assert_eq!(decode_signature("Lcom/example/Order$Line;"), "com.example.Order$Line");
        assert_eq!(decode_signature("LSyntheticProbe$1;"), "SyntheticProbe$1");
        assert_eq!(decode_signature("Lcom/example/Outer$1;"), "com.example.Outer$1");
        assert_eq!(decode_signature("[[Ljava/lang/String;"), "java.lang.String[][]");
        assert_eq!(decode_signature("[I"), "int[]");
    }

    // DISC-4 (#50): the inverse of the two rules above. A name this tool PRINTS has to be a name the
    // tool ACCEPTS, which is what `resolve_loaded_class` was failing at for a hidden class.
    //
    // Written as a round trip rather than as hand-written descriptors on purpose: the acceptance
    // criterion is that the two transforms agree, and a literal expected-descriptor per case would
    // still pass if both sides drifted together. The inputs are the same descriptors #46 measured off
    // live JVMs, so each one asserts that the exact bytes a real JVM sent are among the spellings we
    // would send back.
    #[test]
    fn a_name_this_tool_printed_resolves_back_to_the_descriptor_it_came_from() {
        for measured in [
            // JDK 15+ (measured on 21) — a DOT, and the case that used to miss entirely.
            "LSyntheticProbe$$Lambda.0x0000000092040970;",
            // JDK 11 (measured) — a SLASH, which the plain rewrite already produced.
            "LSyntheticProbe$$Lambda$3/574182878;",
            // Both separators in one name: the package dots go back to slashes, the VM's boundary does
            // not.
            "Ljava/lang/invoke/LambdaForm$MH.0x00007f2c4c0a1800;",
            "Lcom/example/Handler$$Lambda$7/1234567;",
            // Ordinary classes, which must keep costing exactly one lookup.
            "Lcom/example/Order;",
            "Lcom/example/Order$Line;",
            "LSyntheticProbe$1;",
        ] {
            let printed = decode_signature(measured);
            let candidates = descriptor_candidates(&printed);
            assert!(
                candidates.iter().any(|c| c == measured),
                "the JVM sent {measured}, this tool printed it as {printed}, and asking about that name \
                 must reach the same class — DISC-4 (#50) offered only {candidates:?}"
            );
        }
    }

    // DISC-4 (#50): and the JDK-generation trap, stated as the property that keeps it out. The suffix's
    // shape (hex on 15+, decimal on 11) is deliberately NOT what decides the separator — both spellings
    // are offered for both shapes and the debuggee picks. Keying on `0x` would pass on 21 and fail on
    // 11, which is the exact failure #36's matrix caught in #46's first draft.
    #[test]
    fn both_hidden_class_spellings_are_offered_whatever_the_suffix_looks_like() {
        assert_eq!(
            descriptor_candidates("SyntheticProbe$$Lambda/0x0000000092040970"),
            vec![
                "LSyntheticProbe$$Lambda/0x0000000092040970;".to_string(),
                "LSyntheticProbe$$Lambda.0x0000000092040970;".to_string(),
            ],
            "a hex suffix must not be assumed to mean JDK 15+"
        );
        assert_eq!(
            descriptor_candidates("SyntheticProbe$$Lambda$3/574182878"),
            vec![
                "LSyntheticProbe$$Lambda$3/574182878;".to_string(),
                "LSyntheticProbe$$Lambda$3.574182878;".to_string(),
            ],
            "nor a decimal one to mean JDK 11"
        );
        // A name with no VM-assigned suffix has one spelling and no extra packet — the digit rule is what
        // separates them, because no Java simple name may begin with a digit.
        assert_eq!(descriptor_candidates("com.example.Order"), vec!["Lcom/example/Order;".to_string()]);
        assert_eq!(descriptor_candidates("SyntheticProbe$1"), vec!["LSyntheticProbe$1;".to_string()]);
        // Internal spelling pasted straight in: still one candidate, because `Order` is not a suffix.
        assert_eq!(descriptor_candidates("com/example/Order"), vec!["Lcom/example/Order;".to_string()]);
    }

    // DISC-2: a signature the caller can paste into debug.evaluate — dotted FQNs, arrays as `T[]`,
    // primitives by name, and `void` rather than the raw `V` descriptor.
    #[test]
    fn method_rendering_reads_as_java_source() {
        assert_eq!(
            render_method("matches", "(Ljava/lang/String;I)Z", 0),
            "boolean matches(java.lang.String, int)"
        );
        assert_eq!(render_method("run", "()V", 0), "void run()");
        assert_eq!(
            render_method("main", "([Ljava/lang/String;)V", ACC_STATIC),
            "static void main(java.lang.String[])"
        );
        // Multi-dimensional arrays and the wide primitives, which the descriptor packs tightly.
        assert_eq!(
            render_method("grid", "([[JD)[Ljava/lang/Object;", 0),
            "java.lang.Object[] grid(long[][], double)"
        );
    }

    // DISC-2: abstract and native both mean "no body", which is what stops a caller wasting a
    // debug.set_line_stop on them. Flags combine rather than overriding one another.
    #[test]
    fn method_rendering_marks_bodyless_and_static_methods() {
        assert_eq!(render_method("size", "()I", ACC_ABSTRACT), "abstract int size()");
        assert_eq!(
            render_method("currentTimeMillis", "()J", ACC_STATIC | ACC_NATIVE),
            "static native long currentTimeMillis()"
        );
        // A constructor keeps its JVM spelling — it is what evaluate and a stop point both name.
        assert_eq!(render_method("<init>", "(I)V", 0), "void <init>(int)");
    }

    // DISC-3: the directory comes from the PACKAGE and the file name from the JVM, never from the
    // class name. The inner-class case is the one that proves it: `Order$Line` has no `Order$Line.java`
    // anywhere, and a resolver built on the class name alone would look for exactly that and miss.
    #[test]
    fn source_path_is_built_from_the_package_and_the_jvm_file_name() {
        let p = |c, f| {
            source_relative_path(c, f).map(|p| {
                p.components().count().to_string()
                    + ":"
                    + &p.iter().map(|s| s.to_string_lossy().into_owned()).collect::<Vec<_>>().join("/")
            })
        };

        assert_eq!(p("com.example.Order", "Order.java").as_deref(), Some("3:com/example/Order.java"));
        // Inner, and doubly-nested inner: both live in the enclosing compilation unit.
        assert_eq!(p("com.example.Order$Line", "Order.java").as_deref(), Some("3:com/example/Order.java"));
        assert_eq!(
            p("com.example.Order$Line$Key", "Order.java").as_deref(),
            Some("3:com/example/Order.java")
        );
        // A file whose name differs from the type — a package-private class declared in Order.java.
        assert_eq!(p("com.example.OrderRow", "Order.java").as_deref(), Some("3:com/example/Order.java"));
        // Default package: no directories at all, which the package split must not turn into an
        // empty leading segment.
        assert_eq!(p("EvalProbe", "EvalProbe.java").as_deref(), Some("1:EvalProbe.java"));
        assert_eq!(p("EvalProbe$Item", "EvalProbe.java").as_deref(), Some("1:EvalProbe.java"));
    }

    // DISC-3: the source file name arrives from the DEBUGGEE, so it is untrusted input — a SourceFile
    // attribute reading `../../../etc/passwd` is a perfectly valid class file. Every shape that could
    // make the joined path leave the root has to be refused before the join, not after.
    #[test]
    fn source_path_refuses_every_segment_that_could_leave_a_root() {
        for (class, file) in [
            ("com.example.Order", "../../../../etc/passwd"),
            ("com.example.Order", "..\\..\\windows\\win.ini"),
            ("com.example.Order", ".."),
            ("com.example.Order", "."),
            ("com.example.Order", ""),
            ("com.example.Order", "sub/Order.java"),
            // A Windows drive-relative name and an NTFS alternate data stream: neither joins onto a
            // root the way `join` makes it look.
            ("com.example.Order", "C:Order.java"),
            ("com.example.Order", "Order.java:secret"),
            // …and the same escapes hidden in the package half.
            ("com...Order", "Order.java"),
            ("...Order", "Order.java"),
        ] {
            assert!(
                source_relative_path(class, file).is_none(),
                "({class}, {file}) must be refused, not turned into a path"
            );
        }
    }

    // DISC-3: the second layer of the traversal defence, which exists because the first is lexical and
    // a symlink is not. `..` is used here rather than a symlink only because creating one needs
    // privileges on Windows — the code path exercised (canonicalise, then containment) is the same one
    // a symlink out of the tree takes.
    #[test]
    fn a_path_resolving_outside_its_root_is_refused_rather_than_read() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("com/example")).expect("mkdir");
        std::fs::write(root.join("com/example/Order.java"), "class Order {}\n").expect("write");
        std::fs::write(tmp.path().join("Secret.java"), "class Secret {}\n").expect("write");

        let roots = vec![root];
        let found = find_under_roots(&roots, std::path::Path::new("com/example/Order.java"));
        assert!(matches!(found, SourceLookup::Found(_)), "a file genuinely under the root must resolve");

        // The file exists and `root.join(..)` reaches it, so only the containment check stops it.
        let escaped = find_under_roots(&roots, std::path::Path::new("../Secret.java"));
        assert!(
            matches!(escaped, SourceLookup::Escaped(_)),
            "a path that resolves outside its root must be refused, not read"
        );

        let missing = find_under_roots(&roots, std::path::Path::new("com/example/Nope.java"));
        assert!(matches!(missing, SourceLookup::Missing), "an absent file is a miss, not an escape");
    }

    // DISC-3: the window arithmetic, which is where this can be wrong in a way no probe would catch —
    // a line within `context` of either end makes the window run off one side.
    #[test]
    fn the_line_window_stays_inside_the_file() {
        // The ordinary case: `context` either side, inclusive.
        assert_eq!(line_window(100, Some(50), 2, 400), (48, 52));
        // Against either end, the window is clipped rather than wrapping or underflowing to 0.
        assert_eq!(line_window(100, Some(1), 20, 400), (1, 21));
        assert_eq!(line_window(100, Some(100), 20, 400), (80, 100));
        // A file smaller than the window is returned whole.
        assert_eq!(line_window(5, Some(3), 20, 400), (1, 5));
        // A line past the end clamps to the end: the caller is chasing a frame, and a file shorter
        // than the line it named is itself the finding.
        assert_eq!(line_window(10, Some(999), 2, 400), (8, 10));
        // No line means the whole file, capped.
        assert_eq!(line_window(1000, None, 20, 400), (1, 400));
        assert_eq!(line_window(30, None, 20, 400), (1, 30));
        // An empty file has no lines to show, and must not report line 1.
        assert_eq!(line_window(0, Some(5), 20, 400), (1, 0));
    }

    // DISC-3: when `max_lines` is the binding constraint the requested line stays CENTRED. Cutting the
    // tail off instead would drop the lines after the frame, which are usually the ones being read.
    #[test]
    fn a_capped_window_keeps_the_requested_line_centred() {
        assert_eq!(line_window(1000, Some(500), 100, 11), (495, 505));
        // An odd cap is used whole; an even one loses the spare line rather than overshooting.
        assert_eq!(line_window(1000, Some(500), 100, 10), (496, 504));
        // A cap of 1 is the requested line alone, not an empty window.
        assert_eq!(line_window(1000, Some(500), 100, 1), (500, 500));
        // The cap never widens a window the caller asked to be narrow.
        assert_eq!(line_window(1000, Some(500), 2, 400), (498, 502));
    }
}
