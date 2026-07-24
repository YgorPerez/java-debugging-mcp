// MCP request handlers
//
// Handles initialize, list tools, and debug tool execution

use crate::protocol::{JsonRpcRequest, JsonRpcResponse, JsonRpcError, METHOD_NOT_FOUND, JsonRpcNotification, InitializeParams, INVALID_PARAMS, INTERNAL_ERROR, InitializeResult, ServerCapabilities, ToolsCapability, ServerInfo, ListToolsResult, CallToolParams, CallToolResult, ContentBlock};
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
}

impl RequestHandler {
    pub fn new() -> Self {
        Self {
            session_manager: SessionManager::new(),
        }
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

    pub fn handle_notification(notification: &JsonRpcNotification) {
        match notification.method.as_str() {
            "notifications/initialized" => {
                info!("Client initialized");
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
        let _params: InitializeParams = serde_json::from_value(params.unwrap_or_else(|| json!({})))
            .map_err(|e| JsonRpcError {
                code: INVALID_PARAMS,
                message: format!("Invalid initialize params: {e}"),
                data: None,
            })?;

        let result = InitializeResult {
            protocol_version: "2024-11-05".to_string(),
            capabilities: ServerCapabilities {
                tools: ToolsCapability {},
            },
            server_info: ServerInfo {
                name: "jdwp-mcp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            instructions: Some(
                "JDWP debugging server for Java applications. \
                Start by using debug.attach to connect to a JVM, \
                then use debug.set_breakpoint, debug.get_stack, etc."
                    .to_string(),
            ),
        };

        to_json(&result)
    }

    fn handle_list_tools() -> Result<serde_json::Value, JsonRpcError> {
        let result = ListToolsResult {
            tools: tools::get_tools(),
        };

        to_json(&result)
    }

    async fn handle_call_tool(&self, params: Option<serde_json::Value>) -> Result<serde_json::Value, JsonRpcError> {
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
                let call_result = CallToolResult {
                    content: vec![ContentBlock::Text { text: content }],
                    is_error: None,
                };
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
            "debug.set_breakpoint" => self.handle_set_breakpoint(args).await,
            "debug.list_breakpoints" => self.handle_list_breakpoints(args).await,
            "debug.clear_breakpoint" => self.handle_clear_breakpoint(args).await,
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
            "debug.get_last_event" => self.handle_get_last_event(args).await,
            "debug.set_value" => self.handle_set_value(args).await,
            "debug.force_return" => self.handle_force_return(args).await,
            "debug.set_exception_breakpoint" => self.handle_set_exception_breakpoint(args).await,
            "debug.set_watchpoint" => self.handle_set_watchpoint(args).await,
            "debug.get_traces" => self.handle_get_traces(args).await,
            _ => return None,
        })
    }

    async fn handle_attach(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::AttachArgs = crate::args::parse(&args)?;
        let host = a.host.as_str();
        let port = a.port;

        let connection = jdwp_client::JdwpConnection::connect(host, port).await
            .map_err(|e| format!("Failed to connect: {e}"))?;

        let session_id = self.session_manager
            .create_session(connection, format!("{host}:{port}"))
            .await;
        // Get the session guard once so the listener/watchdog handles are stored before we return.
        let session_guard = self.resolve_session(&args).await
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
            session.watchdog_task = Some(spawn_watchdog(
                self.session_manager.clone(),
                session_id.clone(),
            ));
        }

        Ok(format!("Connected to JVM at {host}:{port} (session: {session_id})"))
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

    async fn handle_set_breakpoint(&self, args: serde_json::Value) -> Result<String, String> {
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
            suspend_policy,
        };

        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session. Use debug.attach first.".to_string())?;
        let mut session = session_guard.lock().await;

        let classes = session.connection.classes_by_signature(&spec.signature).await
            .map_err(|e| format!("Failed to find class: {e}"))?;
        let Some(first_class) = classes.first() else {
            return register_deferred_breakpoint(&mut session, &spec).await;
        };
        let class_type_id = first_class.type_id;

        let (bp_id, line, method_name, request_id) =
            arm_and_insert(&mut session, class_type_id, &spec).await?;
        drop(session);

        let mut extra = String::new();
        if spec.trace {
            extra.push_str("\n   Mode: trace (non-suspending) — read hits with debug.get_traces");
            if let Some(e) = &spec.trace_expr {
                let _ = write!(extra, "\n   Trace expr: {e}");
            }
        }
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
            "✅ {} set at {}:{}\n   Method: {}\n   Breakpoint ID: {}\n   JDWP Request ID: {}{}",
            if spec.trace { "Trace breakpoint" } else { "Breakpoint" },
            spec.class_pattern, line, method_name, bp_id, request_id, extra
        ))
    }

    async fn handle_list_breakpoints(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;

        let session = session_guard.lock().await;

        if session.breakpoints.is_empty() && session.pending_breakpoints.is_empty()
            && session.exception_requests.is_empty() && session.watchpoints.is_empty()
        {
            return Ok("No breakpoints set".to_string());
        }

        let mut output = format!(
            "📍 {} breakpoint(s), {} deferred, {} exception, {} watchpoint(s):\n\n",
            session.breakpoints.len(), session.pending_breakpoints.len(),
            session.exception_requests.len(), session.watchpoints.len()
        );

        for (bp_id, bp) in &session.breakpoints {
            render_breakpoint_line(&mut output, bp_id, bp);
        }

        for pb in &session.pending_breakpoints {
            render_pending_line(&mut output, pb);
        }

        for er in session.exception_requests.values() {
            render_exception_line(&mut output, er);
        }

        for (watch_id, wp) in &session.watchpoints {
            render_watchpoint_line(&mut output, watch_id, wp);
        }
        drop(session);

        Ok(output)
    }

    async fn handle_clear_breakpoint(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::ClearBreakpointArgs = crate::args::parse(&args)?;
        let bp_id = a.breakpoint_id.as_str();

        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        // An exception breakpoint lives in exception_requests as an EXCEPTION event request.
        if let Some(er) = session.exception_requests.remove(bp_id) {
            let _ = session.connection.clear_exception_request(er.request_id).await;
            return Ok(format!("✅ Exception breakpoint cleared: {} ({})", bp_id, er.class_pattern));
        }

        // A watchpoint lives in watchpoints as a FIELD_ACCESS / FIELD_MODIFICATION request; Clear
        // must name the same event kind the request was created with.
        if let Some(wp) = session.watchpoints.remove(bp_id) {
            let _ = session.connection.clear_field_watch(wp.request_id, wp.kind).await;
            return Ok(format!(
                "✅ Watchpoint cleared: {} ({}.{} {})",
                bp_id, wp.class_name, wp.field_name, wp.kind.label()
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
        let bp_info = session.breakpoints.get(bp_id)
            .ok_or_else(|| format!("Breakpoint not found: {bp_id}"))?
            .clone();

        // Clear the breakpoint in the JVM
        session.connection.clear_breakpoint(bp_info.request_id).await
            .map_err(|e| format!("Failed to clear breakpoint: {e}"))?;

        // Remove from session
        session.breakpoints.remove(bp_id);
        drop(session);

        Ok(format!(
            "✅ Breakpoint cleared: {} at {}:{}\n   JDWP Request ID: {}",
            bp_id, bp_info.class_pattern, bp_info.line, bp_info.request_id
        ))
    }

    async fn handle_continue(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        // Drop any pending single-step request first, or it would re-fire on resume.
        if let Some(req) = session.pending_step.take() {
            let _ = session.connection.clear_step(req).await;
        }
        session.suspended_since = None;
        session.connection.resume_all().await
            .map_err(|e| format!("Failed to resume: {e}"))?;
        drop(session);

        Ok("▶️  Execution resumed".to_string())
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
        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;

        let a: crate::args::StepArgs = crate::args::parse(&args)?;
        let thread_id = crate::args::parse_thread_id(a.thread_id.as_deref())
            .or(session.last_thread)
            .ok_or_else(|| "No thread to step. Pass thread_id, or hit a breakpoint first.".to_string())?;

        // One active step request at a time; clear the previous before setting a new one.
        if let Some(req) = session.pending_step.take() {
            let _ = session.connection.clear_step(req).await;
        }
        let req = session.connection.set_step(thread_id, depth).await
            .map_err(|e| format!("Failed to set step: {e}"))?;
        session.pending_step = Some(req);
        session.suspended_since = None;
        session.connection.resume_all().await
            .map_err(|e| format!("Failed to resume for step: {e}"))?;
        drop(session);

        Ok(format!(
            "👣 Stepping {label} on thread 0x{thread_id:x}. Call debug.get_last_event to see where it stopped."
        ))
    }

    async fn handle_panic(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;

        if let Some(req) = session.pending_step.take() {
            let _ = session.connection.clear_step(req).await;
        }
        let n = session.breakpoints.len();
        let np = session.pending_breakpoints.len();
        let ne = session.exception_requests.len();
        let nw = session.watchpoints.len();
        let _ = session.connection.clear_all_breakpoints().await;
        session.breakpoints.clear();
        // Also drop deferred breakpoints' CLASS_PREPARE watches.
        let pend: Vec<i32> = session.pending_breakpoints.drain(..).map(|p| p.class_prepare_request_id).collect();
        for req in pend {
            let _ = session.connection.clear_class_prepare(req).await;
        }
        // ClearAllBreakpoints only removes BREAKPOINT requests — clear exception requests too.
        let excs: Vec<i32> = session.exception_requests.drain().map(|(_, e)| e.request_id).collect();
        for req in excs {
            let _ = session.connection.clear_exception_request(req).await;
        }
        // Field watches are likewise untouched by ClearAllBreakpoints, and leaving one armed keeps
        // the debuggee de-optimised — so panic must drop them too.
        let watches: Vec<(i32, jdwp_client::WatchKind)> =
            session.watchpoints.drain().map(|(_, w)| (w.request_id, w.kind)).collect();
        for (req, kind) in watches {
            let _ = session.connection.clear_field_watch(req, kind).await;
        }
        session.suspended_since = None;
        session.connection.resume_all().await
            .map_err(|e| format!("Failed to resume: {e}"))?;
        drop(session);

        Ok(format!("🧯 Panic: cleared {} breakpoint(s){}{}{} and resumed all threads.", n,
            if np > 0 { format!(" + {np} deferred") } else { String::new() },
            if ne > 0 { format!(" + {ne} exception") } else { String::new() },
            if nw > 0 { format!(" + {nw} watchpoint") } else { String::new() }))
    }

    async fn handle_get_stack(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        let a: crate::args::GetStackArgs = crate::args::parse(&args)?;
        let thread_id = crate::args::parse_thread_id(a.thread_id.as_deref());
        let max_frames = a.max_frames;
        let include_variables = a.include_variables;

        let last_thread = session.last_thread;
        let target_thread =
            resolve_target_thread(&mut session.connection, thread_id, last_thread).await?;

        // Get frames (-1 means all frames to avoid INVALID_LENGTH errors)
        let mut frames = session.connection.get_frames(target_thread, 0, -1).await
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
        let package_filter = a.package_filter.as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);

        let mut output = package_filter.as_ref().map_or_else(
            || format!("Stack (thread 0x{:x}, {} frames):\n", target_thread, frames.len()),
            |f| format!("Stack (thread 0x{:x}, {} frames, filter \"{}\"):\n", target_thread, frames.len(), f),
        );

        // Cache class-name resolution across frames (recursion / same-class frames are common).
        let mut class_names: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
        let mut hidden = 0usize;
        // ONE node budget for the whole call — see STACK_NODE_BUDGET. Deep expansion invokes methods
        // in the debuggee, which needs the suspended thread, so `deep` is Some only when asked for;
        // the default path stays cheap and side-effect-free (no toString() per local).
        let mut deep = a.expand_objects.then(|| {
            (
                DeepOpts {
                    depth_limit: a.max_depth,
                    child_limit: a.max_children.max(1),
                    text_len: 200,
                },
                DeepState::new(STACK_NODE_BUDGET),
            )
        });

        for (idx, frame) in frames.iter().enumerate() {
            let class_id = frame.location.class_id;
            let class_name = resolve_class_name(&mut session.connection, class_id, &mut class_names).await;

            // Collapse frames whose class doesn't match the filter (and skip their lookups).
            if let Some(f) = &package_filter {
                if !class_name.to_lowercase().contains(f.as_str()) {
                    hidden += 1;
                    continue;
                }
            }
            flush_hidden(&mut output, &mut hidden);

            // Method name + source line, and the variable slots live at this bytecode index.
            let (method_name, line, active) =
                frame_method_info(&mut session.connection, &frame.location, include_variables).await;

            let _ = match line {
                Some(l) => writeln!(output, "#{idx} {class_name}.{method_name}:{l}"),
                None => writeln!(output, "#{idx} {class_name}.{method_name}"),
            };

            if include_variables && !active.is_empty() {
                let stopped_at = render_frame_variables(
                    &mut session.connection,
                    &mut output,
                    target_thread,
                    (idx, frame.frame_id),
                    &active,
                    deep.as_mut().map(|(opts, state)| (*opts, state)),
                ).await;
                // Out of budget: name where it stopped and abandon the remaining frames, rather than
                // repeating "budget exhausted" under every local of every frame left.
                if let Some(local) = stopped_at {
                    let _ = writeln!(
                        output,
                        "   … node budget ({STACK_NODE_BUDGET}) exhausted at #{idx} {class_name}.{method_name} local `{local}` — remaining frames not expanded. Narrow with package_filter/max_frames/max_depth, or inspect one value with debug.evaluate."
                    );
                    break;
                }
            }
        }
        drop(session);
        flush_hidden(&mut output, &mut hidden);

        Ok(output)
    }

    async fn handle_evaluate(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::EvaluateArgs = crate::args::parse(&args)?;
        let expression = a.expression.as_str();
        let frame_index = a.frame_index;
        let max_len = a.max_result_length;

        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;
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

        let resolved = resolve_expression_multi(conn, thread_id, frame.as_ref(), expression).await?;
        let deep = a.expand_objects.then(|| DeepOpts {
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
        Ok(format!("{} = {}", expression.trim(), rendered))
    }

    async fn handle_list_threads(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        let a: crate::args::ListThreadsArgs = crate::args::parse(&args)?;
        let name_filter = a.name_filter.as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);
        let only_suspended = a.only_suspended;
        let limit = a.limit.max(1);
        let filtering = name_filter.is_some() || only_suspended;

        let all = session.connection.get_all_threads().await
            .map_err(|e| format!("Failed to get threads: {e}"))?;
        let total = all.len();

        let rows = collect_thread_rows(
            &mut session.connection, &all, filtering, limit, name_filter.as_deref(), only_suspended,
        ).await;
        drop(session);

        let shown = rows.len().min(limit);
        let hidden = if filtering {
            rows.len().saturating_sub(shown)
        } else {
            total.saturating_sub(rows.len())
        };

        let mut note = String::new();
        if let Some(f) = &name_filter {
            let _ = write!(note, " name~\"{f}\"");
        }
        if only_suspended {
            note.push_str(" suspended-only");
        }

        let mut output = format!("{shown}/{total} thread(s){note}:\n");
        for (tid, name, status) in rows.iter().take(limit) {
            let _ = match status {
                Some(s) => writeln!(output, "0x{tid:x} {name} [{s}]"),
                None => writeln!(output, "0x{tid:x} {name}"),
            };
        }
        if hidden > 0 {
            let _ = writeln!(output, "… +{hidden} more (raise limit or use name_filter)");
        }

        Ok(output)
    }

    async fn handle_pause(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        session.connection.suspend_all().await
            .map_err(|e| format!("Failed to suspend: {e}"))?;
        drop(session);

        Ok("⏸️  Execution paused (all threads suspended)".to_string())
    }

    async fn handle_disconnect(&self, args: serde_json::Value) -> Result<String, String> {
        let target = match args.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => Some(s.to_string()),
            None => self.session_manager.get_current_session_id().await,
        };

        if let Some(session_id) = target {
            self.session_manager.remove_session(&session_id).await;
            Ok(format!("✅ Disconnected from debug session: {session_id}"))
        } else {
            Err("No active debug session to disconnect".to_string())
        }
    }

    async fn handle_get_last_event(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::GetLastEventArgs = crate::args::parse(&args)?;
        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;

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
        drop(session);

        lines.push(format!("[suspended] {suspended}"));
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

        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;
        let thread_opt = crate::args::parse_thread_id(a.thread_id.as_deref()).or(session.last_thread);
        let conn = &mut session.connection;

        let segs = parse_expr(&target)?;

        // A slice or filter names several elements, so there is no single place to write. Refused
        // explicitly: this used to parse the subscript and then silently drop it, writing the whole
        // field instead of the elements the caller named.
        if let Some(seg) = segs.iter().find(|s| {
            s.subs.iter().any(|x| !matches!(x, Subscript::Index(_)))
        }) {
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
        let field_seg = last_seg;
        if field_seg.args.is_some() {
            return Err("The last segment must be a field, not a method call".to_string());
        }
        let field_name = field_seg.name.clone();
        let raws = split_segments(&target)?;
        let container_expr = raws
            .split_last()
            .map_or_else(String::new, |(_, prefix)| prefix.join("."));

        // Instance-field attempt: resolve the container to an object using a suspended frame.
        let instance_err = match set_instance_field(
            conn, thread_opt, frame_index, &container_expr, &field_name, value_str,
        ).await? {
            FieldWrite::Done(msg) => return Ok(msg),
            FieldWrite::Fallthrough(e) => e,
        };

        // Static-field attempt: treat the container as a dotted class name.
        if let Some(msg) = set_static_field(conn, &container_expr, &field_name, value_str).await? {
            return Ok(msg);
        }

        drop(session);
        Err(instance_err.map_or_else(
            || format!(
                "Could not write '{target}': '{container_expr}' is not a loaded class, and there's no suspended thread to resolve it as an object."
            ),
            |e| format!(
                "Could not write '{target}': '{container_expr}' didn't resolve to an object ({e}) and isn't a loaded class."
            ),
        ))
    }

    async fn handle_force_return(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::ForceReturnArgs = crate::args::parse(&args)?;

        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;
        let thread_id = crate::args::parse_thread_id(a.thread_id.as_deref()).or(session.last_thread)
            .ok_or_else(|| "No thread. Pass thread_id, or hit a breakpoint first.".to_string())?;
        let conn = &mut session.connection;

        let frames = conn.get_frames(thread_id, 0, -1).await
            .map_err(|e| format!("Failed to get frames (is the thread suspended?): {e}"))?;
        let frame = frames.first().cloned()
            .ok_or_else(|| "Thread has no frames (not suspended?)".to_string())?;

        // The forced value must match the top method's declared return type. Pull the return
        // descriptor (the part after ')') so we coerce the literal correctly and handle void.
        let methods = conn.get_methods(frame.location.class_id).await
            .map_err(|e| format!("Failed to get methods: {e}"))?;
        let method = methods.iter().find(|m| m.method_id == frame.location.method_id)
            .ok_or_else(|| "Could not resolve the current method".to_string())?;
        let ret_sig = method.signature.rsplit(')').next().unwrap_or("V");
        let ret_byte = *ret_sig.as_bytes().first().unwrap_or(&b'V');

        let raw = a.value.as_deref().map_or("", str::trim);
        let value = if ret_byte == b'V' {
            jdwp_client::types::Value { tag: 86, data: jdwp_client::types::ValueData::Void }
        } else if raw.is_empty() {
            return Err(format!(
                "{}() returns {} — a 'value' is required (int, 123L, true/false, null, or \"string\")",
                method.name, decode_signature(ret_sig)
            ));
        } else {
            literal_to_value(conn, raw, ret_byte).await?
        };

        conn.force_early_return(thread_id, &value).await
            .map_err(|e| format!("ForceEarlyReturn failed (JVM may lack canForceEarlyReturn, or the value type is wrong): {e}"))?;
        drop(session);

        let shown = if ret_byte == b'V' { "void".to_string() } else { raw.to_string() };
        Ok(format!(
            "✅ Forced {}() to return {} — thread still suspended; call debug.continue to let it proceed.",
            method.name, shown
        ))
    }

    async fn handle_set_exception_breakpoint(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::SetExceptionBreakpointArgs = crate::args::parse(&args)?;
        if !a.caught && !a.uncaught {
            return Err("Set at least one of caught/uncaught to true — otherwise nothing is reported.".to_string());
        }

        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session. Use debug.attach first.".to_string())?;
        let mut session = session_guard.lock().await;

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

        // A traced request suspends only the throwing thread, which the pump snapshots and resumes —
        // so a shared JVM keeps serving while you collect throws.
        let request_id = session.connection
            .set_exception_request(ref_type, a.caught, a.uncaught, suspend_policy_for(a.trace))
            .await
            .map_err(|e| format!("Failed to set exception breakpoint: {e}"))?;

        let class_pattern = pattern.unwrap_or("*").to_string();
        let exc_id = format!("exc_{request_id}");
        session.exception_requests.insert(exc_id.clone(), crate::session::ExceptionRequestInfo {
            id: exc_id.clone(),
            request_id,
            class_pattern: class_pattern.clone(),
            caught: a.caught,
            uncaught: a.uncaught,
            trace: a.trace,
            trace_expr: a.trace_expr.clone(),
        });
        drop(session);

        // `(false, false)` is rejected above, so the remaining case is "caught only".
        let which = match (a.caught, a.uncaught) {
            (true, true) => "caught + uncaught",
            (false, true) => "uncaught only",
            _ => "caught only",
        };
        let noisy = if pattern.is_none() {
            "\n   ⚠️  Matches ALL exceptions — expect frequent hits; clear it as soon as you're done."
        } else {
            ""
        };
        let mode = if a.trace {
            "\n   Mode: trace (non-suspending) — throws are snapshotted and the thread resumed; read them with debug.get_traces"
        } else {
            "\n   Hits are reported via debug.get_last_event.\n   ⚠️  Suspends ALL threads on each throw — on a shared JVM use trace:true instead."
        };
        Ok(format!(
            "✅ Exception breakpoint set on {class_pattern} ({which})\n   Breakpoint ID: {exc_id}{mode}{noisy}"
        ))
    }

    async fn handle_set_watchpoint(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::SetWatchpointArgs = crate::args::parse(&args)?;
        if !a.modify && !a.access {
            return Err("Set at least one of modify/access to true — otherwise nothing is reported.".to_string());
        }
        let class_name = a.class_name.trim();
        let field_name = a.field_name.trim();

        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session. Use debug.attach first.".to_string())?;
        let mut session = session_guard.lock().await;

        // A watchpoint needs a concrete fieldID up front, so — unlike a line breakpoint — it can't
        // be deferred until the class loads.
        let type_id = resolve_class_by_dotted(&mut session.connection, class_name).await?
            .ok_or_else(|| format!(
                "Class '{class_name}' is not loaded yet — exercise it once so the JVM loads it, then retry (watchpoints can't be deferred)."
            ))?;
        let (declaring_type, field) =
            find_field_info(&mut session.connection, type_id, field_name, None).await?
                .ok_or_else(|| format!("Class '{class_name}' has no field '{field_name}' (nor does any superclass)"))?;
        let is_static = (field.mod_bits & ACC_STATIC) != 0;

        // Each kind is its own JDWP request, so "modify + access" registers two and reports two ids.
        let mut kinds = Vec::with_capacity(2);
        if a.modify {
            kinds.push(jdwp_client::WatchKind::Modify);
        }
        if a.access {
            kinds.push(jdwp_client::WatchKind::Access);
        }

        let mut ids = Vec::with_capacity(kinds.len());
        for kind in kinds {
            let request_id = session.connection
                .set_field_watch(declaring_type, field.field_id, kind, suspend_policy_for(a.trace))
                .await
                .map_err(|e| format!(
                    "Failed to set {} watchpoint: {e} (error 99 NOT_IMPLEMENTED means this JVM lacks canWatchField{})",
                    kind.label(),
                    if kind == jdwp_client::WatchKind::Access { "Access" } else { "Modification" },
                ))?;
            let watch_id = format!("watch_{}_{request_id}", kind.label());
            ids.push(format!("{watch_id} ({})", kind.label()));
            session.watchpoints.insert(watch_id, crate::session::WatchpointInfo {
                request_id,
                kind,
                class_name: class_name.to_string(),
                field_name: field_name.to_string(),
                is_static,
                trace: a.trace,
                trace_expr: a.trace_expr.clone(),
            });
        }
        drop(session);

        let kindness = if is_static { "static" } else { "instance" };
        let where_hits = if a.trace {
            "   Mode: trace (non-suspending) — each hit is snapshotted with the mutating location and old → new value, then the thread resumes; read them with debug.get_traces."
        } else {
            "   Hits are reported via debug.get_last_event with the mutating location and old → new value.\n   ⚠️  Suspends ALL threads on each hit — on a shared JVM use trace:true instead."
        };
        Ok(format!(
            "✅ Watchpoint set on {}.{} ({kindness} {})\n   Breakpoint ID(s): {}\n{where_hits}\n   ⚠️  A watched field can't be JIT-optimised — expect the debuggee to slow down; clear it when done.",
            class_name,
            field_name,
            decode_signature(&field.signature),
            ids.join(", "),
        ))
    }

    async fn handle_get_traces(&self, args: serde_json::Value) -> Result<String, String> {
        let a: crate::args::GetTracesArgs = crate::args::parse(&args)?;
        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;

        if session.traces.is_empty() {
            return Ok("No trace snapshots yet. Set a breakpoint with trace:true and trigger it.".to_string());
        }
        let total = session.traces.len();
        let take = a.limit.min(total);
        let start = total - take;
        let mut lines = Vec::with_capacity(take + 2);
        lines.push(format!(
            "📢 {} trace snapshot(s) (showing {}, buffer cap {}):",
            total, take, crate::session::MAX_TRACES
        ));
        for rec in session.traces.iter().skip(start) {
            let detail_s = format_trace_detail(rec);
            let args_s = format_trace_args(rec);
            let expr_s = format_trace_expr(rec);
            lines.push(format!(
                "#{} [{}] {}.{}:{} thread=0x{:x}{}{}{}",
                rec.seq, rec.bp_id, rec.class, rec.method, rec.line.unwrap_or(-1), rec.thread,
                detail_s, args_s, expr_s
            ));
        }
        if a.clear {
            session.traces.clear();
            drop(session);
            lines.push("(buffer cleared)".to_string());
        }
        Ok(lines.join("\n"))
    }
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
    let stops = s.breakpoints.len()
        + s.pending_breakpoints.len()
        + s.exception_requests.len()
        + s.watchpoints.len();
    let mut line = format!(
        "  {} [{}] {} — {}, {} stop point(s), {} JDWP packet(s)",
        if is_current { "▶" } else { " " },
        sid,
        s.endpoint,
        state,
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

/// Format one active breakpoint into the `debug.list_breakpoints` output. `bp_id` is its map key.
fn render_breakpoint_line(output: &mut String, bp_id: &str, bp: &crate::session::BreakpointInfo) {
    let _ = writeln!(
        output,
        "  {} [{}] {}:{}{}",
        if bp.enabled { "✓" } else { "✗" },
        bp_id,
        bp.class_pattern,
        bp.line,
        if bp.trace { " (trace)" } else { "" },
    );
    if let Some(method) = &bp.method {
        let _ = writeln!(output, "     Method: {method}");
    }
    if let Some(e) = &bp.trace_expr {
        let _ = writeln!(output, "     Trace expr: {e}");
    }
    if bp.hit_count > 0 {
        let _ = writeln!(output, "     Hits: {}", bp.hit_count);
    }
}

/// Format one deferred (class-prepare) breakpoint into the `debug.list_breakpoints` output.
fn render_pending_line(output: &mut String, pb: &crate::session::PendingBreakpoint) {
    let where_ = match (pb.line, &pb.method) {
        (Some(l), _) => format!("line {l}"),
        (None, Some(m)) => format!("method {m}"),
        _ => "?".to_string(),
    };
    let _ = writeln!(output, "  ⏳ [{}] {} ({}) — waiting for class load", pb.bp_id, pb.class_pattern, where_);
}

/// Format one exception breakpoint into the `debug.list_breakpoints` output.
fn render_exception_line(output: &mut String, er: &crate::session::ExceptionRequestInfo) {
    let which = match (er.caught, er.uncaught) {
        (true, true) => "caught+uncaught",
        (true, false) => "caught",
        (false, true) => "uncaught",
        (false, false) => "none",
    };
    let _ = writeln!(
        output,
        "  ⚡ [{}] exception {} ({which}){}",
        er.id,
        er.class_pattern,
        if er.trace { " (trace)" } else { "" },
    );
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
    let info = conn.get_fields(ref_type).await.ok()
        .and_then(|fs| fs.into_iter().find(|f| f.field_id == field_id));
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

fn render_watchpoint_line(output: &mut String, watch_id: &str, wp: &crate::session::WatchpointInfo) {
    let _ = writeln!(
        output,
        "  👁  [{}] watch {}.{} on {} ({}){}",
        watch_id,
        wp.class_name,
        wp.field_name,
        wp.kind.label(),
        if wp.is_static { "static" } else { "instance" },
        if wp.trace { " (trace)" } else { "" },
    );
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
    let n = conn.get_signature(class_id).await
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
                    for v in var_table.into_iter()
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
    let threads = conn.get_all_threads().await
        .map_err(|e| format!("Failed to get threads: {e}"))?;
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
    let slots: Vec<jdwp_client::stackframe::VariableSlot> =
        active.iter().map(|(_, s)| *s).collect();
    let Ok(values) = conn.get_frame_values(target_thread, frame_id, slots).await else {
        return None;
    };
    for ((name, _), value) in active.iter().zip(values.iter()) {
        let formatted_value = match &mut deep {
            Some((opts, state)) => {
                render_node(conn, value, Some(target_thread), *opts, state, 0).await
            }
            None => render_value(conn, value, None, 200).await,
        };
        let _ = writeln!(output, "     {name} = {formatted_value}");
        if deep.as_ref().is_some_and(|(_, state)| state.exhausted()) {
            return Some(name.clone());
        }
    }
    None
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

/// JNI signature -> readable type name. "Lpkg/Cls;" -> "pkg.Cls"; "[I" -> "int[]".
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
            sig.get(i + 1..end).unwrap_or_default().replace('/', ".")
        }
        Some(b'Z') => "boolean".to_string(),
        Some(b'B') => "byte".to_string(),
        Some(b'C') => "char".to_string(),
        Some(b'S') => "short".to_string(),
        Some(b'I') => "int".to_string(),
        Some(b'J') => "long".to_string(),
        Some(b'F') => "float".to_string(),
        Some(b'D') => "double".to_string(),
        _ => sig.to_string(),
    };
    format!("{}{}", base, "[]".repeat(dims))
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
async fn assignable(
    conn: &mut jdwp_client::JdwpConnection,
    param: &str,
    arg: &ArgType,
) -> Option<u32> {
    match arg {
        // Handled entirely by `score_param`: null fits any reference, and a primitive either widens
        // into a primitive parameter or boxes into its own wrapper.
        ArgType::Null => None,
        ArgType::Primitive(tag) => {
            boxed_wrapper_of(*tag).filter(|w| w == &param).map(|_| 1)
        }
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
            let scored = params
                .iter()
                .zip(&argtypes)
                .try_fold(0u32, |acc, (p, a)| score_param(p, a).map(|s| acc + s));
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
        jdwp_client::events::EventKind::ClassPrepare { thread, ref_type, signature, .. } =>
            Some((*thread, *ref_type, signature.clone())),
        _ => None,
    }) else {
        return false;
    };
    let pending: Vec<crate::session::PendingBreakpoint> = session
        .pending_breakpoints.iter().filter(|p| p.signature == cp_sig).cloned().collect();
    for pend in pending {
        match resolve_bp_location(&mut session.connection, cp_ref, pend.line, pend.method.as_deref()).await {
            Ok((method, index, line)) => {
                let sp = suspend_policy_for(pend.trace);
                match session.connection.set_breakpoint_ex(
                    cp_ref, method.method_id, index, sp, pend.hit_count, pend.thread_filter,
                ).await {
                    Ok(req_id) => {
                        // Do the bookkeeping that only borrows `pend` first, so its owned fields can
                        // be moved (not cloned) into the stored BreakpointInfo below.
                        let _ = session.connection.clear_class_prepare(pend.class_prepare_request_id).await;
                        session.pending_breakpoints.retain(|p| p.bp_id != pend.bp_id);
                        info!("Armed deferred breakpoint {} on {} (line {})", pend.bp_id, pend.class_pattern, line);
                        session.breakpoints.insert(pend.bp_id, crate::session::BreakpointInfo {
                            request_id: req_id,
                            class_pattern: pend.class_pattern,
                            line: u32::try_from(line).unwrap_or(0),
                            method: Some(method.name),
                            enabled: true,
                            hit_count: 0,
                            condition: pend.condition,
                            trace: pend.trace,
                            trace_expr: pend.trace_expr,
                        });
                    }
                    Err(e) => warn!("Failed to arm deferred breakpoint {}: {}", pend.bp_id, e),
                }
            }
            Err(e) => warn!("Deferred breakpoint {}: class {} loaded but location unresolved: {}", pend.bp_id, pend.class_pattern, e),
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
}

/// Find the traced stop point that a JDWP request id belongs to, across all three kinds.
///
/// One lookup, three maps — deliberately not a fourth map keyed by request id. Each kind already owns
/// its bookkeeping (and its `clear`/`panic` handling), so a parallel index would be a second source of
/// truth that could outlive an entry it points at. The maps are small enough that scanning is free.
fn find_traced_request(
    session: &crate::session::DebugSession,
    req_id: i32,
) -> Option<TracedRequest> {
    if let Some((id, b)) = session.breakpoints.iter().find(|(_, b)| b.request_id == req_id && b.trace) {
        return Some(TracedRequest {
            id: id.clone(),
            condition: b.condition.clone(),
            trace_expr: b.trace_expr.clone(),
        });
    }
    if let Some((id, e)) =
        session.exception_requests.iter().find(|(_, e)| e.request_id == req_id && e.trace)
    {
        return Some(TracedRequest { id: id.clone(), condition: None, trace_expr: e.trace_expr.clone() });
    }
    if let Some((id, w)) = session.watchpoints.iter().find(|(_, w)| w.request_id == req_id && w.trace) {
        return Some(TracedRequest { id: id.clone(), condition: None, trace_expr: w.trace_expr.clone() });
    }
    None
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
    // Honor a condition, if any: skip recording when it isn't true. Only a line breakpoint can have
    // one, so for a traced exception/watch hit this is always false.
    let skip = match &req.condition {
        Some(cond) => !evaluate_condition_on_thread(&mut session.connection, thread, cond).await,
        None => false,
    };
    let record = if skip {
        None
    } else {
        Some(capture_trace(
            &mut session.connection, &req.id, req.trace_expr.as_deref(), thread, &loc, &details,
        ).await)
    };
    if let Some(mut rec) = record {
        session.trace_seq += 1;
        rec.seq = session.trace_seq;
        if session.traces.len() >= crate::session::MAX_TRACES {
            session.traces.pop_front();
        }
        session.traces.push_back(rec);
    }
    let _ = session.connection.resume_thread(thread).await;
    true
}

/// Evaluate a conditional breakpoint on the hit thread and auto-resume (without reporting) when the
/// condition is not true; otherwise record the suspension and store the event for the caller.
async fn store_reportable_event(
    session: &mut crate::session::DebugSession,
    event_set: jdwp_client::EventSet,
) {
    let mut skip = false;
    if let (Some((thread, _)), Some(req_id)) = (
        event_set.events.first().and_then(|e| event_location(&e.details)),
        event_set.events.first().map(|e| e.request_id),
    ) {
        let cond = session.breakpoints.values()
            .find(|b| b.request_id == req_id)
            .and_then(|b| b.condition.clone());
        if let Some(cond) = cond {
            if !evaluate_condition_on_thread(&mut session.connection, thread, &cond).await {
                let _ = session.connection.resume_all().await;
                skip = true;
            }
        }
    }
    if !skip {
        if let Some(tid) = event_thread(&event_set) {
            session.last_thread = Some(tid);
        }
        if event_suspends(&event_set) {
            session.suspended_since = Some(std::time::Instant::now());
        }
        session.push_event(event_set);
    }
}

/// Spawn the watchdog: auto-resume the VM if a breakpoint leaves it suspended past
/// `JDWP_WATCHDOG_SECS` (default 120; `0` disables), so a forgotten breakpoint can't freeze a
/// request thread on a shared instance.
fn spawn_watchdog(
    session_manager: SessionManager,
    sid: crate::session::SessionId,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let secs: u64 = std::env::var("JDWP_WATCHDOG_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);
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
                    if let Some(req) = s.pending_step.take() {
                        let _ = s.connection.clear_step(req).await;
                    }
                    let _ = s.connection.resume_all().await;
                    s.suspended_since = None;
                    drop(s);
                    info!("watchdog auto-resumed VM after {}s suspended", secs);
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
    suspend_policy: jdwp_client::SuspendPolicy,
}

/// Resolve the location on a loaded class, set the JDWP breakpoint, and record it in the session.
/// Returns `(bp_id, resolved source line, method name, JDWP request id)`.
async fn arm_and_insert(
    session: &mut crate::session::DebugSession,
    class_type_id: u64,
    spec: &BreakpointSpec,
) -> Result<(String, i32, String, i32), String> {
    let (method, index, line) = resolve_bp_location(
        &mut session.connection, class_type_id, spec.line_opt, spec.method_hint.as_deref(),
    ).await.map_err(|e| format!("{e} in {}", spec.class_pattern))?;
    let request_id = session.connection.set_breakpoint_ex(
        class_type_id, method.method_id, index, spec.suspend_policy, spec.hit_count, spec.thread_filter,
    ).await.map_err(|e| format!("Failed to set breakpoint: {e}"))?;
    let bp_id = format!("bp_{request_id}");
    session.breakpoints.insert(bp_id.clone(), crate::session::BreakpointInfo {
        request_id,
        class_pattern: spec.class_pattern.clone(),
        line: u32::try_from(line).unwrap_or(0),
        method: Some(method.name.clone()),
        enabled: true,
        hit_count: 0,
        condition: spec.condition.clone(),
        trace: spec.trace,
        trace_expr: spec.trace_expr.clone(),
    });
    Ok((bp_id, line, method.name, request_id))
}

/// The target class isn't loaded yet: register a `CLASS_PREPARE` watch (`EventThread` suspend, so the
/// real breakpoint can be armed before any of the class's code runs) and stash the spec; the event
/// pump arms it when the class loads. Closes the load race by re-checking once the watch is in
/// place, arming immediately if the class appeared in between.
async fn register_deferred_breakpoint(
    session: &mut crate::session::DebugSession,
    spec: &BreakpointSpec,
) -> Result<String, String> {
    let cp_req = session.connection
        .set_class_prepare(&spec.class_pattern, jdwp_client::SuspendPolicy::EventThread).await
        .map_err(|e| format!("Failed to register class-prepare watch: {e}"))?;

    let recheck = session.connection.classes_by_signature(&spec.signature).await.unwrap_or_default();
    if let Some(c) = recheck.first() {
        let ctid = c.type_id;
        let _ = session.connection.clear_class_prepare(cp_req).await;
        let (bp_id, line, method_name, _req) = arm_and_insert(session, ctid, spec).await?;
        return Ok(format!(
            "✅ {} set at {}:{} (class had just loaded)\n   Method: {}\n   Breakpoint ID: {}",
            if spec.trace { "Trace breakpoint" } else { "Breakpoint" },
            spec.class_pattern, line, method_name, bp_id
        ));
    }

    let bp_id = format!("bp_{cp_req}");
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
    });
    let where_ = match (spec.line_opt, spec.method_hint.as_deref()) {
        (Some(l), _) => format!("line {l}"),
        (None, Some(m)) => format!("method {m}"),
        _ => String::new(),
    };
    Ok(format!(
        "⏳ Deferred breakpoint for {0} ({where_}) — {0} is not loaded yet. It will arm automatically when the class loads (trigger the request that loads it), then hit normally.\n   Breakpoint ID: {bp_id}",
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
    let methods = conn.get_methods(class_type_id).await
        .map_err(|e| format!("Failed to get methods: {e}"))?;
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
        name, decode_signature(field_sig), value.format()
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
    let thread_id = thread_opt
        .ok_or_else(|| "No thread. Pass thread_id, or hit a breakpoint first.".to_string())?;
    let frames = conn.get_frames(thread_id, 0, -1).await
        .map_err(|e| format!("Failed to get frames: {e}"))?;
    let frame = frames.get(frame_index).cloned()
        .ok_or_else(|| format!("frame_index {frame_index} out of range"))?;
    let vars = conn.get_variable_table(frame.location.class_id, frame.location.method_id).await
        .map_err(|e| format!("Failed to read variable table: {e}"))?;
    let idx = frame.location.index;
    let var = vars.iter()
        .find(|v| &v.name == name && idx >= v.code_index && idx < v.code_index + u64::from(v.length))
        .or_else(|| vars.iter().find(|v| &v.name == name))
        .ok_or_else(|| format!("Unknown local variable '{name}' (for a static/instance field use Class.field or obj.field)"))?;
    let sig_byte = *var.signature.as_bytes().first().ok_or_else(|| "Bad signature".to_string())?;
    let value = literal_to_value(conn, value_str, sig_byte).await?;
    if !tag_compatible(sig_byte, value.tag) {
        return Err(type_mismatch_err(name, &var.signature, &value));
    }
    conn.set_frame_value(thread_id, frame.frame_id, i32::try_from(var.slot).unwrap_or(0), &value).await
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
        format!("Writing '{container_expr}[…]' needs a suspended thread — pause one or hit a breakpoint first")
    })?;
    let frames = conn.get_frames(tid, 0, -1).await
        .map_err(|e| format!("Failed to get frames (is the thread suspended?): {e}"))?;
    let frame = frames.get(frame_index).or_else(|| frames.first()).cloned();
    let container = resolve_expression(conn, Some(tid), frame.as_ref(), container_expr).await?;
    let id = as_object_id(&container)
        .ok_or_else(|| format!("'{container_expr}' is null or a primitive, so it has no elements"))?;

    if container.tag == 91 {
        return set_array_element(conn, id, container_expr, key, raw_value).await;
    }
    set_collection_element(conn, tid, frame.as_ref(), id, container_expr, key, raw_value).await
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
    frame: Option<&jdwp_client::thread::Frame>,
    id: u64,
    container_expr: &str,
    key: &ArgLit,
    raw_value: &str,
) -> Result<String, String> {
    let type_id = conn.get_object_reference_type(id).await
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
    let value_sig = params.get(1).map_or("Ljava/lang/Object;", String::as_str);
    let sig_byte = *value_sig.as_bytes().first().unwrap_or(&b'L');
    let new_value = literal_to_value(conn, raw_value, sig_byte).await?;
    let args = coerce_args(conn, tid, &m.signature, vec![key_value, new_value]).await?;

    let (ret, exc) = conn.invoke_method(id, tid, decl, m.method_id, args).await
        .map_err(|e| format!("{}() on '{container_expr}' failed: {e}", m.name))?;
    let displaced = invoke_result(conn, &m.name, ret, exc).await?;
    let old = render_value(conn, &displaced, Some(tid), 200).await;
    Ok(format!(
        "✅ Set {container_expr}[{}] = {raw_value} (was {old}) via {}()",
        render_arglit(key),
        m.name,
    ))
}

/// Write one array element via `ArrayReference.SetValues`, coercing the literal to the array's
/// component type. No invocation, so — unlike the collection path — it has no side effects.
async fn set_array_element(
    conn: &mut jdwp_client::JdwpConnection,
    id: u64,
    container_expr: &str,
    key: &ArgLit,
    raw_value: &str,
) -> Result<String, String> {
    let ArgLit::Int(i) = key else {
        return Err(format!("An array index must be an int, got {key:?} on '{container_expr}'"));
    };
    let len = conn.get_array_length(id).await
        .map_err(|e| format!("Failed to read length of '{container_expr}': {e}"))?;
    if *i < 0 || *i >= len {
        return Err(format!("Index {i} is out of bounds for '{container_expr}' (length {len})"));
    }
    // "[I" -> 'I', "[Ljava/lang/String;" -> 'L'. The component type is what the value must match:
    // ArrayReference.SetValues writes untagged, so a wrong width would corrupt the element silently.
    let type_id = conn.get_object_reference_type(id).await
        .map_err(|e| format!("Failed to resolve type of '{container_expr}': {e}"))?;
    let sig = conn.get_signature(type_id).await.unwrap_or_default();
    let component = sig.strip_prefix('[').unwrap_or(&sig).to_string();
    let sig_byte = *component.as_bytes().first().unwrap_or(&b'L');

    let old = conn.get_array_values(id, *i, 1).await.ok().and_then(|v| v.into_iter().next());
    let value = literal_to_value(conn, raw_value, sig_byte).await?;
    if !tag_compatible(sig_byte, value.tag) {
        return Err(format!(
            "'{container_expr}[{i}]' is {} — a {} literal can't be written to it",
            decode_signature(&component),
            decode_signature(&String::from_utf8_lossy(&[value.tag])),
        ));
    }
    conn.set_array_values(id, *i, std::slice::from_ref(&value)).await
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
        _ => return Ok(FieldWrite::Fallthrough(Some(format!("'{container_expr}' is a primitive, not an object")))),
    };
    let type_id = conn.get_object_reference_type(obj_id).await
        .map_err(|e| format!("Failed to resolve object type: {e}"))?;
    let (_, f) = find_field_info(conn, type_id, field_name, Some(false)).await?
        .ok_or_else(|| format!("No instance field '{field_name}' on the resolved object"))?;
    let sig_byte = *f.signature.as_bytes().first().ok_or_else(|| "Bad field signature".to_string())?;
    let value = literal_to_value(conn, value_str, sig_byte).await?;
    if !tag_compatible(sig_byte, value.tag) {
        return Err(type_mismatch_err(field_name, &f.signature, &value));
    }
    conn.set_object_values(obj_id, vec![(f.field_id, value)]).await
        .map_err(|e| format!("Failed to set instance field: {e}"))?;
    Ok(FieldWrite::Done(format!("✅ Set instance field {container_expr}.{field_name} = {value_str}")))
}

/// Static-field attempt: treat `container_expr` as a dotted class name and write its static field.
/// `Ok(None)` means the container isn't a loaded class (caller falls through to its final error).
async fn set_static_field(
    conn: &mut jdwp_client::JdwpConnection,
    container_expr: &str,
    field_name: &str,
    value_str: &str,
) -> Result<Option<String>, String> {
    let Some(class_id) = resolve_class_by_dotted(conn, container_expr).await? else {
        return Ok(None);
    };
    let (_, f) = find_field_info(conn, class_id, field_name, Some(true)).await?
        .ok_or_else(|| format!("class '{container_expr}' has no static field '{field_name}'"))?;
    let sig_byte = *f.signature.as_bytes().first().ok_or_else(|| "Bad field signature".to_string())?;
    let value = literal_to_value(conn, value_str, sig_byte).await?;
    if !tag_compatible(sig_byte, value.tag) {
        return Err(type_mismatch_err(field_name, &f.signature, &value));
    }
    conn.set_reference_values(class_id, vec![(f.field_id, value)]).await
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
        let obj = conn.get_this_object(thread_id, frame.frame_id).await
            .map_err(|e| format!("Failed to get 'this': {e}"))?;
        if obj == 0 {
            return Err("No 'this' in this frame (static method)".to_string());
        }
        return Ok(Value { tag: 76, data: ValueData::Object(obj) });
    }
    let vars = conn.get_variable_table(frame.location.class_id, frame.location.method_id).await
        .map_err(|e| format!("Failed to read local variable table (compiled without -g?): {e}"))?;
    let idx = frame.location.index;
    let var = vars.iter()
        .find(|v| v.name == seg.name && idx >= v.code_index && idx < v.code_index + u64::from(v.length))
        .or_else(|| vars.iter().find(|v| v.name == seg.name))
        .ok_or_else(|| format!("Unknown local variable '{}' in this frame", seg.name))?;
    let sig_byte = *var.signature.as_bytes().first().ok_or_else(|| "Bad variable signature".to_string())?;
    let slot = jdwp_client::stackframe::VariableSlot { slot: i32::try_from(var.slot).unwrap_or(0), sig_byte };
    let frame_values = conn.get_frame_values(thread_id, frame.frame_id, vec![slot]).await
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
    let id = as_object_id(base)
        .ok_or_else(|| format!("Cannot index '{label}' — it is null or a primitive"))?;

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
        if render_primitive(&key_value.data).is_some() {
            box_primitive(conn, tid, &key_value).await.ok_or_else(|| {
                format!("Could not box the key for '{label}[…]' — try a String key")
            })?
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
    let (decl, m) = find_method_for_args(conn, type_id, "valueOf", std::slice::from_ref(v), Some(true))
        .await
        .ok()??;
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

    let len = conn
        .get_array_length(arr)
        .await
        .map_err(|e| format!("Failed to read length of '{label}': {e}"))?;
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
    let Scan { values, len, name, .. } =
        scan_elements(conn, thread_id, base, label, MapScan::Refuse).await?;
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
    for (i, v) in values.into_iter().enumerate() {
        match eval_predicate_on(conn, thread_id, &v, &pred).await {
            Ok(true) => {
                if let Some(k) = keys.get(i) {
                    kept_keys.push(k.clone());
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

/// A filter predicate with everything that does not depend on the element already resolved.
enum Predicate {
    /// `lhs OP rhs`: `lhs` is re-resolved against each element, `rhs` was resolved once.
    Compare { lhs: String, op: String, rhs: PredRhs },
    /// A boolean chain evaluated against each element.
    Bool(String),
}

/// The right-hand side of a comparison: a literal, or a value already read from the frame.
enum PredRhs {
    Lit(ArgLit),
    Value(jdwp_client::types::Value),
}

/// Split a predicate and resolve its element-independent half.
///
/// The left side is deliberately kept as text: it is resolved *against each element*, which is what
/// lets `orders[?status == "OPEN"]` work without an element variable.
async fn prepare_predicate(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    frame: Option<&jdwp_client::thread::Frame>,
    predicate: &str,
) -> Result<Predicate, String> {
    let Some((lhs, op, rhs)) = split_comparison(predicate) else {
        return Ok(Predicate::Bool(predicate.to_string()));
    };
    let rhs = match parse_lit(rhs.trim())? {
        ArgLit::Expr(e) => PredRhs::Value(resolve_expression(conn, thread_id, frame, &e).await?),
        lit => PredRhs::Lit(lit),
    };
    Ok(Predicate::Compare { lhs, op, rhs })
}

/// Evaluate a prepared predicate against one element.
///
/// Takes no frame: by this point every frame-dependent part is already a value, and the element's own
/// fields and methods are reached through its object id, which invocation does not invalidate.
async fn eval_predicate_on(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: Option<u64>,
    element: &jdwp_client::types::Value,
    pred: &Predicate,
) -> Result<bool, String> {
    match pred {
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
        ValueData::Object(0) => {
            return Err(format!("Cannot access '.{}' on null", seg.name))
        }
        ValueData::Object(id) => *id,
        _ => return Err(format!("Cannot access '.{}' on a primitive value", seg.name)),
    };
    let type_id = conn.get_object_reference_type(obj_id).await
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
    let (decl, m) = find_method_for_args(conn, type_id, &seg.name, &argvals, None).await?
        .ok_or_else(|| {
            format!(
                "No method '{}' on the object accepts {} argument(s) of these types",
                seg.name,
                argvals.len()
            )
        })?;
    // Box any primitive the chosen overload declares as a reference (`f(Integer)` given `5`).
    let argvals = coerce_args(conn, tid, &m.signature, argvals).await?;
    let (ret, exc) = conn.invoke_method(obj_id, tid, decl, m.method_id, argvals).await
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
    let fid = find_field(conn, type_id, &seg.name).await?
        .ok_or_else(|| format!("No field '{}' found on the object", seg.name))?;
    let vals = conn.get_object_values(obj_id, vec![fid]).await
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
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<jdwp_client::types::Value, String>> + Send + 'a>> {
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
    resolve_expression_multi(conn, thread_id, frame, expr)
        .await?
        .single("This")
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

    let (mut current, start) = if let Some(Ok(v)) = head_result { (v, 1usize) } else {
        let (v, consumed) = resolve_static_head(conn, thread_id, frame, &segs).await.map_err(|static_err| {
            match &head_result {
                Some(Err(head_err)) => {
                    format!("{head_err} (also not a resolvable static member: {static_err})")
                }
                _ => format!(
                    "No suspended frame to read locals from, and not a resolvable static member: {static_err}"
                ),
            }
        })?;
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
            return if start < segs.len() {
                Err(multi_then_chain_error(&head_owner.name))
            } else {
                Ok(many)
            }
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
    let (decl, m) = find_method_for_args(conn, type_id, &member.name, &argvals, Some(true))
        .await?
        .ok_or_else(|| {
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
async fn resolve_class_by_dotted(
    conn: &mut jdwp_client::JdwpConnection,
    dotted: &str,
) -> Result<Option<u64>, String> {
    let sig = format!("L{};", dotted.replace('.', "/"));
    let classes = conn
        .classes_by_signature(&sig)
        .await
        .map_err(|e| format!("classes_by_signature failed: {e}"))?;
    if let Some(c) = classes.iter().find(|c| c.ref_type_tag == 1).or_else(|| classes.first()) {
        return Ok(Some(c.type_id));
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
                Ok(t) => format!("{} (id=0x{:x})", decode_signature(&conn.get_signature(t).await.unwrap_or_default()), id),
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
        other => render_primitive(other).unwrap_or_else(|| "(?)".to_string()),
    }
}

/// Render a primitive value; `None` for a reference (which needs a JVM round trip to describe).
fn render_primitive(data: &jdwp_client::types::ValueData) -> Option<String> {
    use jdwp_client::types::ValueData;
    Some(match data {
        ValueData::Byte(v) => format!("(byte) {v}"),
        ValueData::Char(v) => format!("(char) '{}'", char::from_u32(u32::from(*v)).unwrap_or('?')),
        ValueData::Float(v) => format!("(float) {v}"),
        ValueData::Double(v) => format!("(double) {v}"),
        ValueData::Int(v) => format!("(int) {v}"),
        ValueData::Long(v) => format!("(long) {v}"),
        ValueData::Short(v) => format!("(short) {v}"),
        ValueData::Boolean(v) => format!("(boolean) {v}"),
        ValueData::Void => "(void)".to_string(),
        ValueData::Object(_) => return None,
    })
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
    if let Some(p) = render_primitive(&value.data) {
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
        if let Some(rendered) =
            render_collection_deep(conn, id, type_id, name, tid, opts, state, depth).await
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
    render_primitive(&v.data)
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
        if let Some(rendered) = render_via_tostring(conn, id, type_id, tid, &name, max_len).await {
            return rendered;
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
) -> Option<String> {
    let (decl, m) = find_method_arity(conn, type_id, "toString", 0).await.ok()??;
    if m.signature != "()Ljava/lang/String;" {
        return None;
    }
    let (ret, exc) = conn.invoke_method(id, tid, decl, m.method_id, vec![]).await.ok()?;
    if exc != 0 {
        return None;
    }
    let jdwp_client::types::ValueData::Object(sid) = ret.data else {
        return None;
    };
    if sid == 0 {
        return None;
    }
    let s = conn.get_string_value(sid).await.ok()?;
    Some(format!("{} \"{}\"", name, truncate(&s, max_len)))
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
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        ArgLit::Int(n) => match sig_byte {
            b'J' => value_long(i64::from(n)),
            b'Z' => value_bool(n != 0),
            b'B' => jdwp_client::types::Value { tag: 66, data: jdwp_client::types::ValueData::Byte(n as i8) },
            b'S' => jdwp_client::types::Value { tag: 83, data: jdwp_client::types::ValueData::Short(n as i16) },
            b'C' => jdwp_client::types::Value { tag: 67, data: jdwp_client::types::ValueData::Char(n as u16) },
            b'F' => jdwp_client::types::Value { tag: 70, data: jdwp_client::types::ValueData::Float(n as f32) },
            b'D' => jdwp_client::types::Value { tag: 68, data: jdwp_client::types::ValueData::Double(f64::from(n)) },
            _ => value_int(n),
        },
    })
}

// ----- event / thread / location helpers -----

fn event_location(d: &EventKind) -> Option<(u64, Location)> {
    match d {
        EventKind::Breakpoint { thread, location }
        | EventKind::Step { thread, location }
        | EventKind::MethodEntry { thread, location }
        | EventKind::MethodExit { thread, location }
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
                    | EventKind::MethodEntry { .. }
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
        EventKind::MethodEntry { .. } => "method_entry",
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

/// Collect `(id, name, status label)` rows for `debug.list_threads`. One JVM round-trip per thread
/// for the name, plus one for the status only when `only_suspended` is set. With no filter we stop
/// scanning once we have `limit` rows so a 300-thread `WildFly` doesn't cost 300 round-trips for a peek.
async fn collect_thread_rows(
    conn: &mut jdwp_client::JdwpConnection,
    all: &[u64],
    filtering: bool,
    limit: usize,
    name_filter: Option<&str>,
    only_suspended: bool,
) -> Vec<(u64, String, Option<String>)> {
    let mut rows: Vec<(u64, String, Option<String>)> = Vec::new();
    for tid in all {
        if !filtering && rows.len() >= limit {
            break;
        }
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
                Err(_) => continue,
            }
        } else {
            None
        };
        rows.push((*tid, name, status));
    }
    rows
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
    lt.lines
        .iter()
        .filter(|e| e.line_code_index <= index)
        .max_by_key(|e| e.line_code_index)
        .map(|e| e.line_number)
}

/// Resolve (class name, method name, source line) for a location.
async fn describe_location(conn: &mut jdwp_client::JdwpConnection, loc: &Location) -> (String, String, Option<i32>) {
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

/// Snapshot a trace/logpoint hit: source location, in-scope locals/args, the kind-specific detail
/// (exception type + catch site, or a watched field's old → new pair), and any trace expression.
///
/// The hit thread is suspended (`EventThread` policy) while this runs; the caller resumes it right
/// after. Argument values are rendered WITHOUT invoking `toString()` (`thread_id` None), so tracing
/// stays side-effect free; the explicit `trace_expr` may invoke methods since the user asked for it.
///
/// The watchpoint detail must be captured HERE rather than at read time for the same reason
/// `get_last_event` reports it inline: the old value is only readable while the pending store has not
/// committed, which is exactly this window.
async fn capture_trace(
    conn: &mut jdwp_client::JdwpConnection,
    bp_id: &str,
    trace_expr: Option<&str>,
    thread: u64,
    loc: &Location,
    details: &EventKind,
) -> crate::session::TraceRecord {
    let (class, method, line) = describe_location(conn, loc).await;
    let mut args: Vec<(String, String)> = Vec::new();
    let mut expr: Option<(String, String)> = None;

    if let Ok(frames) = conn.get_frames(thread, 0, 1).await {
        if let Some(frame) = frames.first().cloned() {
            if let Ok(var_table) = conn.get_variable_table(loc.class_id, loc.method_id).await {
                let ci = loc.index;
                // Own each in-scope variable's (name, slot) so the names can be moved into `args`
                // below without cloning.
                let in_scope: Vec<(String, jdwp_client::stackframe::VariableSlot)> = var_table.into_iter()
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
    let detail = obj.into_iter().map(|(k, v)| (k, json_scalar_to_string(&v))).collect();

    crate::session::TraceRecord {
        seq: 0, bp_id: bp_id.to_string(), thread, class, method, line, args, expr, detail,
    }
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
    if let Some((lhs, op, rhs)) = split_comparison(condition) {
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
        let v = resolve_expression(conn, Some(thread_id), Some(frame), condition).await?;
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
    use jdwp_client::types::ValueData::{Int, Long, Short, Byte, Char, Float, Double};
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
            let t = conn.get_object_reference_type(id).await
                .map_err(|e| format!("Failed to resolve type: {e}"))?;
            if conn.get_signature(t).await.unwrap_or_default() == "Ljava/lang/String;" {
                let sv = conn.get_string_value(id).await
                    .map_err(|e| format!("Failed to read string: {e}"))?;
                match op {
                    "==" => Ok(&sv == s),
                    "!=" => Ok(&sv != s),
                    _ => Err("only == / != for strings".to_string()),
                }
            } else {
                Err("Left side is not a String".to_string())
            }
        }
        _ => Err("Unsupported comparison (numbers, booleans, null, or String value compares only)".to_string()),
    }
}
