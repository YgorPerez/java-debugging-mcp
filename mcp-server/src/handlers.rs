// MCP request handlers
//
// Handles initialize, list tools, and debug tool execution

use crate::protocol::*;
use crate::session::SessionManager;
use crate::tools;
use serde_json::json;
use tracing::{debug, info, warn};

pub struct RequestHandler {
    session_manager: SessionManager,
}

impl RequestHandler {
    pub fn new() -> Self {
        Self {
            session_manager: SessionManager::new(),
        }
    }

    pub async fn handle_request(&self, request: JsonRpcRequest) -> JsonRpcResponse {
        let result = match request.method.as_str() {
            "initialize" => self.handle_initialize(request.params),
            "tools/list" => self.handle_list_tools(),
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

    pub async fn handle_notification(&self, notification: JsonRpcNotification) {
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

    fn handle_initialize(&self, params: Option<serde_json::Value>) -> Result<serde_json::Value, JsonRpcError> {
        let _params: InitializeParams = serde_json::from_value(params.unwrap_or(json!({})))
            .map_err(|e| JsonRpcError {
                code: INVALID_PARAMS,
                message: format!("Invalid initialize params: {}", e),
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

        Ok(serde_json::to_value(result).unwrap())
    }

    fn handle_list_tools(&self) -> Result<serde_json::Value, JsonRpcError> {
        let result = ListToolsResult {
            tools: tools::get_tools(),
        };

        Ok(serde_json::to_value(result).unwrap())
    }

    async fn handle_call_tool(&self, params: Option<serde_json::Value>) -> Result<serde_json::Value, JsonRpcError> {
        let call_params: CallToolParams = serde_json::from_value(params.unwrap_or(json!({})))
            .map_err(|e| JsonRpcError {
                code: INVALID_PARAMS,
                message: format!("Invalid tool call params: {}", e),
                data: None,
            })?;

        // Route to appropriate handler based on tool name
        let result = match call_params.name.as_str() {
            "debug.attach" => self.handle_attach(call_params.arguments).await,
            "debug.set_breakpoint" => self.handle_set_breakpoint(call_params.arguments).await,
            "debug.list_breakpoints" => self.handle_list_breakpoints(call_params.arguments).await,
            "debug.clear_breakpoint" => self.handle_clear_breakpoint(call_params.arguments).await,
            "debug.continue" => self.handle_continue(call_params.arguments).await,
            "debug.step_over" => self.handle_step_over(call_params.arguments).await,
            "debug.step_into" => self.handle_step_into(call_params.arguments).await,
            "debug.step_out" => self.handle_step_out(call_params.arguments).await,
            "debug.get_stack" => self.handle_get_stack(call_params.arguments).await,
            "debug.evaluate" => self.handle_evaluate(call_params.arguments).await,
            "debug.list_threads" => self.handle_list_threads(call_params.arguments).await,
            "debug.pause" => self.handle_pause(call_params.arguments).await,
            "debug.disconnect" => self.handle_disconnect(call_params.arguments).await,
            "debug.get_last_event" => self.handle_get_last_event(call_params.arguments).await,
            "debug.panic" => self.handle_panic(call_params.arguments).await,
            "debug.set_value" => self.handle_set_value(call_params.arguments).await,
            _ => Err(format!("Unknown tool: {}", call_params.name)),
        };

        match result {
            Ok(content) => {
                let call_result = CallToolResult {
                    content: vec![ContentBlock::Text { text: content }],
                    is_error: None,
                };
                Ok(serde_json::to_value(call_result).unwrap())
            }
            Err(error) => {
                let call_result = CallToolResult {
                    content: vec![ContentBlock::Text { text: error.clone() }],
                    is_error: Some(true),
                };
                Ok(serde_json::to_value(call_result).unwrap())
            }
        }
    }

    // Tool implementations (stubs for now)
    async fn handle_attach(&self, args: serde_json::Value) -> Result<String, String> {
        let host = args.get("host").and_then(|v| v.as_str()).unwrap_or("localhost");
        let port = args.get("port").and_then(|v| v.as_u64()).unwrap_or(5005) as u16;

        match jdwp_client::JdwpConnection::connect(host, port).await {
            Ok(connection) => {
                // Create session
                let session_id = self.session_manager.create_session(connection).await;

                // Get session guard once to prevent race between spawn and store
                let session_guard = self.session_manager.get_current_session().await
                    .ok_or_else(|| "Failed to get session after creation".to_string())?;

                // Clone connection, spawn task, and store handle in single critical section
                {
                    let mut session = session_guard.lock().await;
                    let connection_clone = session.connection.clone();

                    // Spawn event listener task
                    let session_manager = self.session_manager.clone();
                    let task_handle = tokio::spawn(async move {
                        loop {
                            // Receive event without holding any locks!
                            let event_opt = connection_clone.recv_event().await;

                            // Store event (brief lock acquisition)
                            if let Some(event_set) = event_opt {
                                if let Some(session_guard) = session_manager.get_current_session().await {
                                    let mut session = session_guard.lock().await;
                                    if let Some(tid) = event_thread(&event_set) {
                                        session.last_thread = Some(tid);
                                    }
                                    if event_suspends(&event_set) {
                                        session.suspended_since = Some(std::time::Instant::now());
                                    }
                                    session.last_event = Some(event_set);
                                } else {
                                    break; // Session gone
                                }
                            } else {
                                break; // Connection closed
                            }
                        }
                        info!("Event listener task stopped");
                    });

                    // Store task handle before releasing lock - prevents race with disconnect
                    session.event_listener_task = Some(task_handle);

                    // Watchdog: auto-resume if a breakpoint leaves the VM suspended too long,
                    // so a forgotten breakpoint can't freeze a request thread on a shared instance.
                    let wd_manager = self.session_manager.clone();
                    let watchdog = tokio::spawn(async move {
                        let secs: u64 = std::env::var("JDWP_WATCHDOG_SECS")
                            .ok()
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(120);
                        if secs == 0 {
                            return;
                        }
                        loop {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            match wd_manager.get_current_session().await {
                                Some(g) => {
                                    let mut s = g.lock().await;
                                    if let Some(since) = s.suspended_since {
                                        if since.elapsed().as_secs() >= secs {
                                            if let Some(req) = s.pending_step.take() {
                                                let _ = s.connection.clear_step(req).await;
                                            }
                                            let _ = s.connection.resume_all().await;
                                            s.suspended_since = None;
                                            info!("watchdog auto-resumed VM after {}s suspended", secs);
                                        }
                                    }
                                }
                                None => break,
                            }
                        }
                    });
                    session.watchdog_task = Some(watchdog);
                }

                Ok(format!("Connected to JVM at {}:{} (session: {})", host, port, session_id))
            }
            Err(e) => Err(format!("Failed to connect: {}", e)),
        }
    }

    async fn handle_set_breakpoint(&self, args: serde_json::Value) -> Result<String, String> {
        let class_pattern = args.get("class_pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing 'class_pattern' parameter".to_string())?;

        let line_opt = args.get("line").and_then(|v| v.as_i64()).map(|l| l as i32);
        let method_hint = args.get("method").and_then(|v| v.as_str());
        if line_opt.is_none() && method_hint.is_none() {
            return Err("Provide 'line' and/or 'method'".to_string());
        }
        let hit_count = args.get("hit_count").and_then(|v| v.as_i64()).map(|c| c as i32);
        let thread_filter = arg_thread(&args);

        let session_guard = self.session_manager.get_current_session().await
            .ok_or_else(|| "No active debug session. Use debug.attach first.".to_string())?;

        let mut session = session_guard.lock().await;

        let signature = if class_pattern.starts_with('L') && class_pattern.ends_with(';') {
            class_pattern.to_string()
        } else {
            format!("L{};", class_pattern.replace('.', "/"))
        };

        let classes = session.connection.classes_by_signature(&signature).await
            .map_err(|e| format!("Failed to find class: {}", e))?;
        if classes.is_empty() {
            return Err(format!("Class not found: {}", class_pattern));
        }
        let class_type_id = classes[0].type_id;

        let methods = session.connection.get_methods(class_type_id).await
            .map_err(|e| format!("Failed to get methods: {}", e))?;

        // Resolve (method, bytecode index, line): by explicit line, by method name
        // (first executable line), or a named method that also contains the line.
        let mut chosen: Option<(jdwp_client::reftype::MethodInfo, u64, i32)> = None;
        for method in &methods {
            if let Some(hint) = method_hint {
                if method.name != hint {
                    continue;
                }
            }
            let line_table = match session.connection.get_line_table(class_type_id, method.method_id).await {
                Ok(lt) => lt,
                Err(_) => continue,
            };
            if let Some(want) = line_opt {
                if let Some(e) = line_table.lines.iter().find(|e| e.line_number == want) {
                    chosen = Some((method.clone(), e.line_code_index, want));
                    break;
                }
                if method_hint.is_some() {
                    if let Some(e) = line_table.lines.iter().min_by_key(|e| e.line_code_index) {
                        chosen = Some((method.clone(), e.line_code_index, e.line_number));
                        break;
                    }
                }
            } else if let Some(e) = line_table.lines.iter().min_by_key(|e| e.line_code_index) {
                chosen = Some((method.clone(), e.line_code_index, e.line_number));
                break;
            }
        }
        let (method, index, line) = chosen.ok_or_else(|| match line_opt {
            Some(l) => format!("No method contains line {} in {}", l, class_pattern),
            None => format!("Method '{}' not found in {}", method_hint.unwrap_or(""), class_pattern),
        })?;

        let request_id = session.connection.set_breakpoint_ex(
            class_type_id,
            method.method_id,
            index,
            jdwp_client::SuspendPolicy::All,
            hit_count,
            thread_filter,
        ).await.map_err(|e| format!("Failed to set breakpoint: {}", e))?;

        let bp_id = format!("bp_{}", request_id);
        session.breakpoints.insert(bp_id.clone(), crate::session::BreakpointInfo {
            id: bp_id.clone(),
            request_id,
            class_pattern: class_pattern.to_string(),
            line: line as u32,
            method: Some(method.name.clone()),
            enabled: true,
            hit_count: 0,
        });

        let mut extra = String::new();
        if let Some(c) = hit_count {
            extra.push_str(&format!("\n   Stops on hit #{}", c));
        }
        if let Some(t) = thread_filter {
            extra.push_str(&format!("\n   Thread filter: 0x{:x}", t));
        }
        Ok(format!(
            "✅ Breakpoint set at {}:{}\n   Method: {}\n   Breakpoint ID: {}\n   JDWP Request ID: {}{}",
            class_pattern, line, method.name, bp_id, request_id, extra
        ))
    }

    async fn handle_list_breakpoints(&self, _args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.session_manager.get_current_session().await
            .ok_or_else(|| "No active debug session".to_string())?;

        let session = session_guard.lock().await;

        if session.breakpoints.is_empty() {
            return Ok("No breakpoints set".to_string());
        }

        let mut output = format!("📍 {} breakpoint(s):\n\n", session.breakpoints.len());

        for (_, bp) in session.breakpoints.iter() {
            output.push_str(&format!(
                "  {} [{}] {}:{}\n",
                if bp.enabled { "✓" } else { "✗" },
                bp.id,
                bp.class_pattern,
                bp.line
            ));
            if let Some(method) = &bp.method {
                output.push_str(&format!("     Method: {}\n", method));
            }
            if bp.hit_count > 0 {
                output.push_str(&format!("     Hits: {}\n", bp.hit_count));
            }
        }

        Ok(output)
    }

    async fn handle_clear_breakpoint(&self, args: serde_json::Value) -> Result<String, String> {
        let bp_id = args.get("breakpoint_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing 'breakpoint_id' parameter".to_string())?;

        let session_guard = self.session_manager.get_current_session().await
            .ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        // Find the breakpoint
        let bp_info = session.breakpoints.get(bp_id)
            .ok_or_else(|| format!("Breakpoint not found: {}", bp_id))?
            .clone();

        // Clear the breakpoint in the JVM
        session.connection.clear_breakpoint(bp_info.request_id).await
            .map_err(|e| format!("Failed to clear breakpoint: {}", e))?;

        // Remove from session
        session.breakpoints.remove(bp_id);

        Ok(format!(
            "✅ Breakpoint cleared: {} at {}:{}\n   JDWP Request ID: {}",
            bp_id, bp_info.class_pattern, bp_info.line, bp_info.request_id
        ))
    }

    async fn handle_continue(&self, _args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.session_manager.get_current_session().await
            .ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        // Drop any pending single-step request first, or it would re-fire on resume.
        if let Some(req) = session.pending_step.take() {
            let _ = session.connection.clear_step(req).await;
        }
        session.suspended_since = None;
        session.connection.resume_all().await
            .map_err(|e| format!("Failed to resume: {}", e))?;

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
        let session_guard = self.session_manager.get_current_session().await
            .ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;

        let thread_id = arg_thread(&args)
            .or(session.last_thread)
            .ok_or_else(|| "No thread to step. Pass thread_id, or hit a breakpoint first.".to_string())?;

        // One active step request at a time; clear the previous before setting a new one.
        if let Some(req) = session.pending_step.take() {
            let _ = session.connection.clear_step(req).await;
        }
        let req = session.connection.set_step(thread_id, depth).await
            .map_err(|e| format!("Failed to set step: {}", e))?;
        session.pending_step = Some(req);
        session.suspended_since = None;
        session.connection.resume_all().await
            .map_err(|e| format!("Failed to resume for step: {}", e))?;

        Ok(format!(
            "👣 Stepping {} on thread 0x{:x}. Call debug.get_last_event to see where it stopped.",
            label, thread_id
        ))
    }

    async fn handle_panic(&self, _args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.session_manager.get_current_session().await
            .ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;

        if let Some(req) = session.pending_step.take() {
            let _ = session.connection.clear_step(req).await;
        }
        let n = session.breakpoints.len();
        let _ = session.connection.clear_all_breakpoints().await;
        session.breakpoints.clear();
        session.suspended_since = None;
        session.connection.resume_all().await
            .map_err(|e| format!("Failed to resume: {}", e))?;

        Ok(format!("🧯 Panic: cleared {} breakpoint(s) and resumed all threads.", n))
    }

    async fn handle_get_stack(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.session_manager.get_current_session().await
            .ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        let thread_id = args.get("thread_id")
            .and_then(|v| v.as_str())
            .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok());

        let max_frames = args.get("max_frames")
            .and_then(|v| v.as_i64())
            .unwrap_or(20) as usize;

        let include_variables = args.get("include_variables")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Prefer the explicit thread, then the last thread that hit a breakpoint/step,
        // then fall back to the first thread.
        let target_thread = if let Some(tid) = thread_id {
            tid
        } else if let Some(t) = session.last_thread {
            t
        } else {
            let threads = session.connection.get_all_threads().await
                .map_err(|e| format!("Failed to get threads: {}", e))?;

            *threads.first().ok_or_else(|| "No threads found".to_string())?
        };

        // Get frames (-1 means all frames to avoid INVALID_LENGTH errors)
        let mut frames = session.connection.get_frames(target_thread, 0, -1).await
            .map_err(|e| format!("Failed to get frames: {}", e))?;

        // Truncate to max_frames
        frames.truncate(max_frames);

        if frames.is_empty() {
            return Ok(format!("Thread {:x} has no stack frames", target_thread));
        }

        let mut output = format!("🔍 Stack for thread {:x} ({} frames):\n\n", target_thread, frames.len());

        for (idx, frame) in frames.iter().enumerate() {
            output.push_str(&format!("Frame {}:\n", idx));
            output.push_str(&format!("  Location: class={:x}, method={:x}, index={}\n",
                frame.location.class_id, frame.location.method_id, frame.location.index));

            // Try to get method name
            if let Ok(methods) = session.connection.get_methods(frame.location.class_id).await {
                if let Some(method) = methods.iter().find(|m| m.method_id == frame.location.method_id) {
                    let method_name = method.name.clone();
                    let line = source_line(&mut session.connection, frame.location.class_id, frame.location.method_id, frame.location.index).await;
                    match line {
                        Some(l) => output.push_str(&format!("  Method: {} (line {})\n", method_name, l)),
                        None => output.push_str(&format!("  Method: {}\n", method_name)),
                    }

                    // Get variables if requested
                    if include_variables {
                        match session.connection.get_variable_table(frame.location.class_id, frame.location.method_id).await {
                            Ok(var_table) => {
                                let current_index = frame.location.index;
                                let active_vars: Vec<_> = var_table.iter()
                                    .filter(|v| current_index >= v.code_index && current_index < v.code_index + v.length as u64)
                                    .collect();

                                if !active_vars.is_empty() {
                                    output.push_str(&format!("  Variables ({}):\n", active_vars.len()));

                                    let slots: Vec<jdwp_client::stackframe::VariableSlot> = active_vars.iter()
                                        .map(|v| jdwp_client::stackframe::VariableSlot {
                                            slot: v.slot as i32,
                                            sig_byte: v.signature.as_bytes()[0],
                                        })
                                        .collect();

                                    if let Ok(values) = session.connection.get_frame_values(target_thread, frame.frame_id, slots).await {
                                        for (var, value) in active_vars.iter().zip(values.iter()) {
                                            // Render with type name + string contents (no method
                                            // invocation here — thread=None — to keep get_stack cheap).
                                            let formatted_value = render_value(&mut session.connection, value, None, 200).await;
                                            output.push_str(&format!("    {} = {}\n", var.name, formatted_value));
                                        }
                                    }
                                }
                            }
                            Err(_) => {}
                        }
                    }
                }
            }

            output.push_str("\n");
        }

        Ok(output)
    }

    async fn handle_evaluate(&self, args: serde_json::Value) -> Result<String, String> {
        let expression = args.get("expression").and_then(|v| v.as_str())
            .ok_or_else(|| "Missing 'expression' parameter".to_string())?;
        let frame_index = args.get("frame_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let max_len = args.get("max_result_length").and_then(|v| v.as_u64()).unwrap_or(4000) as usize;

        let session_guard = self.session_manager.get_current_session().await
            .ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;
        let thread_id = arg_thread(&args)
            .or(session.last_thread)
            .ok_or_else(|| "No thread to evaluate in. Pass thread_id, or hit a breakpoint first.".to_string())?;
        let conn = &mut session.connection;

        let frames = conn.get_frames(thread_id, 0, -1).await
            .map_err(|e| format!("Failed to get frames (is the thread suspended at a breakpoint?): {}", e))?;
        if frames.is_empty() {
            return Err("Thread has no stack frames (not suspended at a breakpoint?)".to_string());
        }
        let frame = frames.get(frame_index)
            .ok_or_else(|| format!("frame_index {} out of range ({} frames)", frame_index, frames.len()))?
            .clone();

        let value = resolve_expression(conn, thread_id, &frame, expression).await?;
        let rendered = render_value(conn, &value, Some(thread_id), max_len).await;
        Ok(format!("{} = {}", expression.trim(), rendered))
    }

    async fn handle_list_threads(&self, _args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.session_manager.get_current_session().await
            .ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        let threads = session.connection.get_all_threads().await
            .map_err(|e| format!("Failed to get threads: {}", e))?;

        let mut output = format!("🧵 {} thread(s):\n\n", threads.len());

        for (idx, thread_id) in threads.iter().enumerate() {
            output.push_str(&format!("  Thread {} (ID: 0x{:x})\n", idx + 1, thread_id));

            // Try to get frame count
            match session.connection.get_frames(*thread_id, 0, 1).await {
                Ok(frames) if !frames.is_empty() => {
                    output.push_str("     Status: Has frames (possibly suspended)\n");
                }
                Ok(_) => {
                    output.push_str("     Status: Running (no frames)\n");
                }
                Err(_) => {
                    output.push_str("     Status: Cannot inspect\n");
                }
            }
        }

        Ok(output)
    }

    async fn handle_pause(&self, _args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.session_manager.get_current_session().await
            .ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        session.connection.suspend_all().await
            .map_err(|e| format!("Failed to suspend: {}", e))?;

        Ok("⏸️  Execution paused (all threads suspended)".to_string())
    }

    async fn handle_disconnect(&self, _args: serde_json::Value) -> Result<String, String> {
        let current_session_id = self.session_manager.get_current_session_id().await;

        if let Some(session_id) = current_session_id {
            // Remove the session (this will also clear current session)
            self.session_manager.remove_session(&session_id).await;
            Ok(format!("✅ Disconnected from debug session: {}", session_id))
        } else {
            Err("No active debug session to disconnect".to_string())
        }
    }

    async fn handle_get_last_event(&self, _args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.session_manager.get_current_session().await
            .ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        let event_set = match session.last_event.clone() {
            Some(es) => es,
            None => return Ok("No events received yet. Set a breakpoint and trigger it.".to_string()),
        };

        let mut output = String::new();
        // Machine-readable summary of the first event, with source location resolved.
        if let Some(ev) = event_set.events.first() {
            if let Some((thread, loc)) = event_location(&ev.details) {
                let (cls, method, line) = describe_location(&mut session.connection, &loc).await;
                let summary = json!({
                    "event": event_type_name(&ev.details),
                    "thread": format!("0x{:x}", thread),
                    "class": cls,
                    "method": method,
                    "line": line,
                });
                output.push_str(&format!("[event] {}\n\n", summary));
            }
        }
        output.push_str(&format!("🎯 Last event (suspend_policy={})\n\n", event_set.suspend_policy));

        {
            for (idx, event) in event_set.events.iter().enumerate() {
                output.push_str(&format!("Event {}:\n", idx + 1));
                output.push_str(&format!("  Request ID: {}\n", event.request_id));

                match &event.details {
                    jdwp_client::events::EventKind::Breakpoint { thread, location } => {
                        output.push_str("  Type: Breakpoint\n");
                        output.push_str(&format!("  ⚡ Thread ID: 0x{:x}\n", thread));
                        output.push_str(&format!("  Location: class=0x{:x}, method=0x{:x}, index={}\n",
                            location.class_id, location.method_id, location.index));
                    }
                    jdwp_client::events::EventKind::Step { thread, location } => {
                        output.push_str("  Type: Step\n");
                        output.push_str(&format!("  Thread ID: 0x{:x}\n", thread));
                        output.push_str(&format!("  Location: class=0x{:x}, method=0x{:x}, index={}\n",
                            location.class_id, location.method_id, location.index));
                    }
                    jdwp_client::events::EventKind::VMStart { thread } => {
                        output.push_str("  Type: VM Start\n");
                        output.push_str(&format!("  Thread ID: 0x{:x}\n", thread));
                    }
                    jdwp_client::events::EventKind::VMDeath => {
                        output.push_str("  Type: VM Death\n");
                    }
                    jdwp_client::events::EventKind::ThreadStart { thread } => {
                        output.push_str("  Type: Thread Start\n");
                        output.push_str(&format!("  Thread ID: 0x{:x}\n", thread));
                    }
                    jdwp_client::events::EventKind::ThreadDeath { thread } => {
                        output.push_str("  Type: Thread Death\n");
                        output.push_str(&format!("  Thread ID: 0x{:x}\n", thread));
                    }
                    jdwp_client::events::EventKind::ClassPrepare { thread, ref_type, signature, .. } => {
                        output.push_str("  Type: Class Prepare\n");
                        output.push_str(&format!("  Thread ID: 0x{:x}\n", thread));
                        output.push_str(&format!("  Class: {} (0x{:x})\n", signature, ref_type));
                    }
                    _ => {
                        output.push_str("  Type: Other\n");
                    }
                }

                output.push_str("\n");
            }
        }

        Ok(output)
    }

    async fn handle_set_value(&self, args: serde_json::Value) -> Result<String, String> {
        let name = args.get("name").and_then(|v| v.as_str())
            .ok_or_else(|| "Missing 'name' (local variable)".to_string())?;
        let value_str = args.get("value").and_then(|v| v.as_str())
            .ok_or_else(|| "Missing 'value' (literal: int, true/false, null, or \"string\")".to_string())?;
        let frame_index = args.get("frame_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let session_guard = self.session_manager.get_current_session().await
            .ok_or_else(|| "No active debug session".to_string())?;
        let mut session = session_guard.lock().await;
        let thread_id = arg_thread(&args).or(session.last_thread)
            .ok_or_else(|| "No thread. Pass thread_id, or hit a breakpoint first.".to_string())?;
        let conn = &mut session.connection;

        let frames = conn.get_frames(thread_id, 0, -1).await
            .map_err(|e| format!("Failed to get frames: {}", e))?;
        let frame = frames.get(frame_index).cloned()
            .ok_or_else(|| format!("frame_index {} out of range", frame_index))?;

        let vars = conn.get_variable_table(frame.location.class_id, frame.location.method_id).await
            .map_err(|e| format!("Failed to read variable table: {}", e))?;
        let idx = frame.location.index;
        let var = vars.iter()
            .find(|v| v.name == name && idx >= v.code_index && idx < v.code_index + v.length as u64)
            .or_else(|| vars.iter().find(|v| v.name == name))
            .ok_or_else(|| format!("Unknown local variable '{}'", name))?;
        let sig_byte = *var.signature.as_bytes().first().ok_or_else(|| "Bad signature".to_string())?;

        let value = literal_to_value(conn, value_str, sig_byte).await?;
        conn.set_frame_value(thread_id, frame.frame_id, var.slot as i32, &value).await
            .map_err(|e| format!("Failed to set value: {}", e))?;
        Ok(format!("✅ Set {} = {}", name, value_str))
    }
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

#[derive(Debug, Clone)]
enum ArgLit {
    Int(i32),
    Long(i64),
    Bool(bool),
    Null,
    Str(String),
}

struct Seg {
    name: String,
    /// None = field access; Some = method call with these argument literals.
    args: Option<Vec<ArgLit>>,
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_' || c == '$').unwrap_or(false)
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Split an expression into `.`-separated segments, ignoring dots inside () or "".
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
            '(' if !in_str => {
                depth += 1;
                cur.push(c);
            }
            ')' if !in_str => {
                depth -= 1;
                cur.push(c);
            }
            '.' if !in_str && depth == 0 => {
                segs.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if depth != 0 || in_str {
        return Err("Unbalanced parentheses or quotes".to_string());
    }
    if !cur.trim().is_empty() {
        segs.push(cur.trim().to_string());
    }
    Ok(segs)
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
    Err(format!(
        "Unsupported argument literal: '{}' (supported: int, long like 123L, true/false, null, \"string\")",
        t
    ))
}

fn parse_args(inside: &str) -> Result<Vec<ArgLit>, String> {
    let s = inside.trim();
    if s.is_empty() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_str = !in_str;
                cur.push(c);
            }
            ',' if !in_str => {
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
    if let Some(open) = raw.find('(') {
        if !raw.ends_with(')') {
            return Err(format!("Malformed method call: '{}'", raw));
        }
        let name = raw[..open].trim();
        if !is_ident(name) {
            return Err(format!("Bad method name: '{}'", name));
        }
        let args = parse_args(&raw[open + 1..raw.len() - 1])?;
        Ok(Seg { name: name.to_string(), args: Some(args) })
    } else {
        if !is_ident(raw) {
            return Err(format!("Unsupported token: '{}'", raw));
        }
        Ok(Seg { name: raw.to_string(), args: None })
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
    while i < bytes.len() && bytes[i] == b'[' {
        dims += 1;
        i += 1;
    }
    let base = match bytes.get(i) {
        Some(b'L') => {
            let end = if sig.ends_with(';') { sig.len() - 1 } else { sig.len() };
            sig[i + 1..end].replace('/', ".")
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
    let mut chars = sig[a + 1..b].chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '[' => continue, // array prefix; the following base type is the arg
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
        let methods = conn.get_methods(tid).await.map_err(|e| format!("Failed to get methods: {}", e))?;
        if let Some(m) = methods.into_iter().find(|m| m.name == name && sig_arg_count(&m.signature) == argc) {
            return Ok(Some((tid, m)));
        }
        current = conn.get_superclass(tid).await.unwrap_or(None);
    }
    Ok(None)
}

/// Find a field by name, walking the superclass chain.
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
        let fields = conn.get_fields(tid).await.map_err(|e| format!("Failed to get fields: {}", e))?;
        if let Some(f) = fields.into_iter().find(|f| f.name == name) {
            return Ok(Some(f.field_id));
        }
        current = conn.get_superclass(tid).await.unwrap_or(None);
    }
    Ok(None)
}

async fn arglit_to_value(
    conn: &mut jdwp_client::JdwpConnection,
    a: &ArgLit,
) -> Result<jdwp_client::types::Value, String> {
    Ok(match a {
        ArgLit::Int(n) => value_int(*n),
        ArgLit::Long(n) => value_long(*n),
        ArgLit::Bool(b) => value_bool(*b),
        ArgLit::Null => value_null(),
        ArgLit::Str(s) => {
            let id = conn.create_string(s).await.map_err(|e| format!("Failed to create string arg: {}", e))?;
            value_object(id)
        }
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
            .map_err(|e| format!("Failed to get 'this': {}", e))?;
        if obj == 0 {
            return Err("No 'this' in this frame (static method)".to_string());
        }
        return Ok(Value { tag: 76, data: ValueData::Object(obj) });
    }
    let vars = conn.get_variable_table(frame.location.class_id, frame.location.method_id).await
        .map_err(|e| format!("Failed to read local variable table (compiled without -g?): {}", e))?;
    let idx = frame.location.index;
    let var = vars.iter()
        .find(|v| v.name == seg.name && idx >= v.code_index && idx < v.code_index + v.length as u64)
        .or_else(|| vars.iter().find(|v| v.name == seg.name))
        .ok_or_else(|| format!("Unknown local variable '{}' in this frame", seg.name))?;
    let sig_byte = *var.signature.as_bytes().first().ok_or_else(|| "Bad variable signature".to_string())?;
    let slot = jdwp_client::stackframe::VariableSlot { slot: var.slot as i32, sig_byte };
    let vals = conn.get_frame_values(thread_id, frame.frame_id, vec![slot]).await
        .map_err(|e| format!("Failed to read variable value: {}", e))?;
    vals.into_iter().next().ok_or_else(|| "No value returned for variable".to_string())
}

async fn resolve_segment(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: u64,
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
        .map_err(|e| format!("Failed to resolve object type: {}", e))?;

    match &seg.args {
        Some(arglits) => {
            let mut argvals = Vec::with_capacity(arglits.len());
            for a in arglits {
                argvals.push(arglit_to_value(conn, a).await?);
            }
            let (decl, m) = find_method_arity(conn, type_id, &seg.name, argvals.len()).await?
                .ok_or_else(|| format!("No method '{}' with {} argument(s) on the object", seg.name, argvals.len()))?;
            let (ret, exc) = conn.invoke_method(obj_id, thread_id, decl, m.method_id, argvals).await
                .map_err(|e| format!("invoke {}() failed: {}", seg.name, e))?;
            if exc != 0 {
                let tn = match conn.get_object_reference_type(exc).await {
                    Ok(t) => decode_signature(&conn.get_signature(t).await.unwrap_or_default()),
                    Err(_) => "an exception".to_string(),
                };
                return Err(format!("{}() threw {}", seg.name, tn));
            }
            Ok(ret)
        }
        None => {
            let fid = find_field(conn, type_id, &seg.name).await?
                .ok_or_else(|| format!("No field '{}' found on the object", seg.name))?;
            let vals = conn.get_object_values(obj_id, vec![fid]).await
                .map_err(|e| format!("Failed to read field '{}': {}", seg.name, e))?;
            vals.into_iter().next().ok_or_else(|| "No value returned for field".to_string())
        }
    }
}

async fn resolve_expression(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: u64,
    frame: &jdwp_client::thread::Frame,
    expr: &str,
) -> Result<jdwp_client::types::Value, String> {
    let segs = parse_expr(expr)?;
    let mut current = resolve_head(conn, thread_id, frame, &segs[0]).await?;
    for seg in &segs[1..] {
        current = resolve_segment(conn, thread_id, &current, seg).await?;
    }
    Ok(current)
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
                Err(_) => format!("(object) @{:x}", id),
            }
        }
        _ => value.format(),
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
        ValueData::Byte(v) => format!("(byte) {}", v),
        ValueData::Char(v) => format!("(char) '{}'", char::from_u32(*v as u32).unwrap_or('?')),
        ValueData::Float(v) => format!("(float) {}", v),
        ValueData::Double(v) => format!("(double) {}", v),
        ValueData::Int(v) => format!("(int) {}", v),
        ValueData::Long(v) => format!("(long) {}", v),
        ValueData::Short(v) => format!("(short) {}", v),
        ValueData::Boolean(v) => format!("(boolean) {}", v),
        ValueData::Void => "(void)".to_string(),
        ValueData::Object(0) => "null".to_string(),
        ValueData::Object(id) => {
            let id = *id;
            if value.tag == 115 {
                if let Ok(s) = conn.get_string_value(id).await {
                    return format!("\"{}\"", truncate(&s, max_len));
                }
            }
            let type_id = match conn.get_object_reference_type(id).await {
                Ok(t) => t,
                Err(_) => return format!("(object) @{:x}", id),
            };
            let name = decode_signature(&conn.get_signature(type_id).await.unwrap_or_default());
            if name == "java.lang.String" {
                if let Ok(s) = conn.get_string_value(id).await {
                    return format!("\"{}\"", truncate(&s, max_len));
                }
            }
            // Array contents
            if value.tag == 91 {
                if let Ok(len) = conn.get_array_length(id).await {
                    let take = len.min(16);
                    if let Ok(elems) = conn.get_array_values(id, 0, take).await {
                        let mut parts = Vec::with_capacity(elems.len());
                        for e in &elems {
                            parts.push(render_element(conn, e).await);
                        }
                        let more = if len > take { format!(", … +{} more", len - take) } else { String::new() };
                        let base = name.strip_suffix("[]").unwrap_or(name.as_str());
                        return format!("{}[{}]{{{}{}}}", base, len, parts.join(", "), more);
                    }
                }
            }
            // best-effort toString() when we have a thread to run it on
            if let Some(tid) = thread_id {
                if let Ok(Some((decl, m))) = find_method_arity(conn, type_id, "toString", 0).await {
                    if m.signature == "()Ljava/lang/String;" {
                        if let Ok((ret, exc)) = conn.invoke_method(id, tid, decl, m.method_id, vec![]).await {
                            if exc == 0 {
                                if let ValueData::Object(sid) = ret.data {
                                    if sid != 0 {
                                        if let Ok(s) = conn.get_string_value(sid).await {
                                            return format!("{} \"{}\"", name, truncate(&s, max_len));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            format!("{} (id=0x{:x})", name, id)
        }
    }
}

/// Convert a literal string to a Value, coercing int literals to the slot's primitive type.
async fn literal_to_value(
    conn: &mut jdwp_client::JdwpConnection,
    s: &str,
    sig_byte: u8,
) -> Result<jdwp_client::types::Value, String> {
    Ok(match parse_lit(s)? {
        ArgLit::Str(st) => {
            let id = conn.create_string(&st).await.map_err(|e| format!("Failed to create string: {}", e))?;
            value_object(id)
        }
        ArgLit::Null => value_null(),
        ArgLit::Bool(b) => value_bool(b),
        ArgLit::Long(n) => value_long(n),
        ArgLit::Int(n) => match sig_byte {
            b'J' => value_long(n as i64),
            b'Z' => value_bool(n != 0),
            _ => value_int(n),
        },
    })
}

// ----- event / thread / location helpers -----

fn arg_thread(args: &serde_json::Value) -> Option<u64> {
    args.get("thread_id")
        .and_then(|v| v.as_str())
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
}

fn event_location(d: &EventKind) -> Option<(u64, Location)> {
    match d {
        EventKind::Breakpoint { thread, location }
        | EventKind::Step { thread, location }
        | EventKind::MethodEntry { thread, location }
        | EventKind::MethodExit { thread, location } => Some((*thread, location.clone())),
        EventKind::Exception { thread, location, .. } => Some((*thread, location.clone())),
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
            )
        })
}

fn event_type_name(d: &EventKind) -> &'static str {
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
        EventKind::Unknown { .. } => "unknown",
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
