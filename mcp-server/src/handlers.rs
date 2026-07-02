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
                let session_guard = self.resolve_session(&args).await
                    .ok_or_else(|| "Failed to get session after creation".to_string())?;

                // Clone connection, spawn task, and store handle in single critical section
                {
                    let mut session = session_guard.lock().await;
                    let connection_clone = session.connection.clone();

                    // Spawn event listener task (bound to THIS session, not "current").
                    let session_manager = self.session_manager.clone();
                    let listener_sid = session_id.clone();
                    let task_handle = tokio::spawn(async move {
                        loop {
                            // Receive event without holding any locks!
                            let event_opt = connection_clone.recv_event().await;

                            // Store event (brief lock acquisition)
                            if let Some(event_set) = event_opt {
                                if let Some(session_guard) = session_manager.get_session_by_id(&listener_sid).await {
                                    let mut session = session_guard.lock().await;
                                    // Conditional breakpoint: evaluate the condition on the hit thread
                                    // and auto-resume (skip reporting) when it is not true.
                                    let mut skip = false;
                                    if let (Some((thread, _)), Some(req_id)) = (
                                        event_set.events.first().and_then(|e| event_location(&e.details)),
                                        event_set.events.first().map(|e| e.request_id),
                                    ) {
                                        let cond = session.breakpoints.values()
                                            .find(|b| b.request_id == req_id)
                                            .and_then(|b| b.condition.clone());
                                        if let Some(cond) = cond {
                                            let keep = evaluate_condition_on_thread(&mut session.connection, thread, &cond).await;
                                            if !keep {
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
                                        session.last_event = Some(event_set);
                                    }
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
                    let wd_sid = session_id.clone();
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
                            match wd_manager.get_session_by_id(&wd_sid).await {
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
        let condition = args.get("condition").and_then(|v| v.as_str()).map(|s| s.to_string());

        let session_guard = self.resolve_session(&args).await
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
            condition: condition.clone(),
        });

        let mut extra = String::new();
        if let Some(c) = hit_count {
            extra.push_str(&format!("\n   Stops on hit #{}", c));
        }
        if let Some(t) = thread_filter {
            extra.push_str(&format!("\n   Thread filter: 0x{:x}", t));
        }
        if let Some(c) = &condition {
            extra.push_str(&format!("\n   Condition: {}", c));
        }
        Ok(format!(
            "✅ Breakpoint set at {}:{}\n   Method: {}\n   Breakpoint ID: {}\n   JDWP Request ID: {}{}",
            class_pattern, line, method.name, bp_id, request_id, extra
        ))
    }

    async fn handle_list_breakpoints(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.resolve_session(&args).await
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

        let session_guard = self.resolve_session(&args).await
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
        let session_guard = self.resolve_session(&args).await
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

    async fn handle_panic(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.resolve_session(&args).await
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
        let session_guard = self.resolve_session(&args).await
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

        // Compact format: one line per frame `#idx class.method:line`, variables indented
        // beneath. Raw JDWP class/method ids are omitted — they're noise to the caller.
        // `package_filter` collapses frames whose class doesn't match (a JVM like WildFly buries a
        // few app frames under dozens of framework ones) into `… N frame(s) hidden` markers, and
        // skips the expensive method/variable round-trips for those hidden frames.
        let package_filter = args.get("package_filter")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase());

        let mut output = match &package_filter {
            Some(f) => format!("Stack (thread 0x{:x}, {} frames, filter \"{}\"):\n", target_thread, frames.len(), f),
            None => format!("Stack (thread 0x{:x}, {} frames):\n", target_thread, frames.len()),
        };

        // Cache class-name resolution across frames (recursion / same-class frames are common).
        let mut class_names: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
        let mut hidden = 0usize;

        for (idx, frame) in frames.iter().enumerate() {
            let class_id = frame.location.class_id;
            let class_name = match class_names.get(&class_id) {
                Some(n) => n.clone(),
                None => {
                    let n = session.connection.get_signature(class_id).await
                        .ok()
                        .map(|s| decode_signature(&s))
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| format!("class@{:x}", class_id));
                    class_names.insert(class_id, n.clone());
                    n
                }
            };

            // Collapse frames whose class doesn't match the filter (and skip their lookups).
            if let Some(f) = &package_filter {
                if !class_name.to_lowercase().contains(f.as_str()) {
                    hidden += 1;
                    continue;
                }
            }
            flush_hidden(&mut output, &mut hidden);

            // Method name + source line, and the variable slots live at this bytecode index.
            let mut method_name = format!("method@{:x}", frame.location.method_id);
            let mut line: Option<i32> = None;
            let mut active: Vec<(String, jdwp_client::stackframe::VariableSlot)> = Vec::new();
            if let Ok(methods) = session.connection.get_methods(class_id).await {
                if let Some(method) = methods.iter().find(|m| m.method_id == frame.location.method_id) {
                    method_name = method.name.clone();
                    line = source_line(&mut session.connection, class_id, frame.location.method_id, frame.location.index).await;
                    if include_variables {
                        if let Ok(var_table) = session.connection.get_variable_table(class_id, frame.location.method_id).await {
                            let ci = frame.location.index;
                            for v in var_table.iter()
                                .filter(|v| ci >= v.code_index && ci < v.code_index + v.length as u64)
                            {
                                active.push((v.name.clone(), jdwp_client::stackframe::VariableSlot {
                                    slot: v.slot as i32,
                                    sig_byte: v.signature.as_bytes()[0],
                                }));
                            }
                        }
                    }
                }
            }

            match line {
                Some(l) => output.push_str(&format!("#{} {}.{}:{}\n", idx, class_name, method_name, l)),
                None => output.push_str(&format!("#{} {}.{}\n", idx, class_name, method_name)),
            }

            if include_variables && !active.is_empty() {
                let slots: Vec<jdwp_client::stackframe::VariableSlot> =
                    active.iter().map(|(_, s)| s.clone()).collect();
                if let Ok(values) = session.connection.get_frame_values(target_thread, frame.frame_id, slots).await {
                    for ((name, _), value) in active.iter().zip(values.iter()) {
                        // Render with type name + string contents (no method invocation here —
                        // thread=None — to keep get_stack cheap).
                        let formatted_value = render_value(&mut session.connection, value, None, 200).await;
                        output.push_str(&format!("     {} = {}\n", name, formatted_value));
                    }
                }
            }
        }
        flush_hidden(&mut output, &mut hidden);

        Ok(output)
    }

    async fn handle_evaluate(&self, args: serde_json::Value) -> Result<String, String> {
        let expression = args.get("expression").and_then(|v| v.as_str())
            .ok_or_else(|| "Missing 'expression' parameter".to_string())?;
        let frame_index = args.get("frame_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        // Default must match the schema in tools.rs (they used to drift: schema 500 vs code 4000).
        let max_len = args.get("max_result_length").and_then(|v| v.as_u64()).unwrap_or(2000) as usize;

        let session_guard = self.resolve_session(&args).await
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

    async fn handle_list_threads(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        let name_filter = args.get("name_filter")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase());
        let only_suspended = args.get("only_suspended").and_then(|v| v.as_bool()).unwrap_or(false);
        let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(40).max(1) as usize;
        let filtering = name_filter.is_some() || only_suspended;

        let all = session.connection.get_all_threads().await
            .map_err(|e| format!("Failed to get threads: {}", e))?;
        let total = all.len();

        // (id, name, status label). One JVM round-trip per thread for the name, plus one for the
        // status only when only_suspended is set. With no filter we stop scanning once we have
        // `limit` rows so a 300-thread WildFly doesn't cost 300 round-trips for a peek.
        let mut rows: Vec<(u64, String, Option<String>)> = Vec::new();
        for tid in &all {
            if !filtering && rows.len() >= limit {
                break;
            }
            let name = session.connection.get_thread_name(*tid).await.unwrap_or_default();
            if let Some(f) = &name_filter {
                if !name.to_lowercase().contains(f.as_str()) {
                    continue;
                }
            }
            let status = if only_suspended {
                match session.connection.get_thread_status(*tid).await {
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

        let shown = rows.len().min(limit);
        let hidden = if filtering {
            rows.len().saturating_sub(shown)
        } else {
            total.saturating_sub(rows.len())
        };

        let mut note = String::new();
        if let Some(f) = &name_filter {
            note.push_str(&format!(" name~\"{}\"", f));
        }
        if only_suspended {
            note.push_str(" suspended-only");
        }

        let mut output = format!("{}/{} thread(s){}:\n", shown, total, note);
        for (tid, name, status) in rows.iter().take(limit) {
            match status {
                Some(s) => output.push_str(&format!("0x{:x} {} [{}]\n", tid, name, s)),
                None => output.push_str(&format!("0x{:x} {}\n", tid, name)),
            }
        }
        if hidden > 0 {
            output.push_str(&format!("… +{} more (raise limit or use name_filter)\n", hidden));
        }

        Ok(output)
    }

    async fn handle_pause(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        session.connection.suspend_all().await
            .map_err(|e| format!("Failed to suspend: {}", e))?;

        Ok("⏸️  Execution paused (all threads suspended)".to_string())
    }

    async fn handle_disconnect(&self, args: serde_json::Value) -> Result<String, String> {
        let target = match args.get("session_id").and_then(|v| v.as_str()) {
            Some(s) => Some(s.to_string()),
            None => self.session_manager.get_current_session_id().await,
        };

        if let Some(session_id) = target {
            self.session_manager.remove_session(&session_id).await;
            Ok(format!("✅ Disconnected from debug session: {}", session_id))
        } else {
            Err("No active debug session to disconnect".to_string())
        }
    }

    async fn handle_get_last_event(&self, args: serde_json::Value) -> Result<String, String> {
        let session_guard = self.resolve_session(&args).await
            .ok_or_else(|| "No active debug session".to_string())?;

        let mut session = session_guard.lock().await;

        let event_set = match session.last_event.clone() {
            Some(es) => es,
            None => return Ok("No events received yet. Set a breakpoint and trigger it.".to_string()),
        };

        // Compact, machine-readable summary only — one [event] line per event with the source
        // location resolved. Raw JDWP ids and the human-readable decoration are intentionally
        // omitted; they cost tokens and the caller never uses them.
        let mut lines: Vec<String> = Vec::with_capacity(event_set.events.len() + 1);
        for ev in &event_set.events {
            let mut obj = serde_json::Map::new();
            obj.insert("event".to_string(), json!(event_type_name(&ev.details)));
            if let Some((thread, loc)) = event_location(&ev.details) {
                let (cls, method, line) = describe_location(&mut session.connection, &loc).await;
                obj.insert("thread".to_string(), json!(format!("0x{:x}", thread)));
                obj.insert("class".to_string(), json!(cls));
                obj.insert("method".to_string(), json!(method));
                obj.insert("line".to_string(), json!(line));
            } else {
                match &ev.details {
                    jdwp_client::events::EventKind::VMStart { thread }
                    | jdwp_client::events::EventKind::ThreadStart { thread }
                    | jdwp_client::events::EventKind::ThreadDeath { thread } => {
                        obj.insert("thread".to_string(), json!(format!("0x{:x}", thread)));
                    }
                    jdwp_client::events::EventKind::ClassPrepare { thread, signature, .. } => {
                        obj.insert("thread".to_string(), json!(format!("0x{:x}", thread)));
                        obj.insert("class".to_string(), json!(signature));
                    }
                    _ => {}
                }
            }
            lines.push(format!("[event] {}", serde_json::Value::Object(obj)));
        }
        lines.push(format!("[suspended] {}", event_suspends(&event_set)));
        Ok(lines.join("\n"))
    }

    async fn handle_set_value(&self, args: serde_json::Value) -> Result<String, String> {
        let name = args.get("name").and_then(|v| v.as_str())
            .ok_or_else(|| "Missing 'name' (local variable)".to_string())?;
        let value_str = args.get("value").and_then(|v| v.as_str())
            .ok_or_else(|| "Missing 'value' (literal: int, true/false, null, or \"string\")".to_string())?;
        let frame_index = args.get("frame_index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        let session_guard = self.resolve_session(&args).await
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

/// JDWP value tags for each method-descriptor parameter (objects/arrays collapse to 'L'=76).
fn sig_param_tags(sig: &str) -> Vec<u8> {
    let (a, b) = match (sig.find('('), sig.find(')')) {
        (Some(a), Some(b)) if b > a => (a, b),
        _ => return vec![],
    };
    let mut tags = Vec::new();
    let mut chars = sig[a + 1..b].chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '[' => {
                while let Some(&'[') = chars.peek() {
                    chars.next();
                }
                if let Some('L') = chars.next() {
                    for x in chars.by_ref() {
                        if x == ';' {
                            break;
                        }
                    }
                }
                tags.push(76);
            }
            'L' => {
                for x in chars.by_ref() {
                    if x == ';' {
                        break;
                    }
                }
                tags.push(76);
            }
            'Z' => tags.push(90),
            'B' => tags.push(66),
            'C' => tags.push(67),
            'S' => tags.push(83),
            'I' => tags.push(73),
            'J' => tags.push(74),
            'F' => tags.push(70),
            'D' => tags.push(68),
            _ => {}
        }
    }
    tags
}

/// Is a provided argument value tag acceptable for a parameter tag?
fn tag_compatible(param: u8, arg: u8) -> bool {
    let is_obj = |t: u8| matches!(t, 76 | 115 | 116 | 103 | 108 | 99 | 91);
    let is_num = |t: u8| matches!(t, 66 | 67 | 68 | 70 | 73 | 74 | 83);
    param == arg || (is_obj(param) && is_obj(arg)) || (is_num(param) && is_num(arg))
}

/// Find a method by name + argument types (preferring a type-compatible overload, falling
/// back to the first arity match), walking the superclass chain.
async fn find_method_for_args(
    conn: &mut jdwp_client::JdwpConnection,
    type_id: u64,
    name: &str,
    arg_tags: &[u8],
) -> Result<Option<(u64, jdwp_client::reftype::MethodInfo)>, String> {
    let argc = arg_tags.len();
    let mut current = Some(type_id);
    let mut guard = 0;
    let mut fallback: Option<(u64, jdwp_client::reftype::MethodInfo)> = None;
    while let Some(tid) = current {
        guard += 1;
        if guard > 50 {
            break;
        }
        let methods = conn.get_methods(tid).await.map_err(|e| format!("Failed to get methods: {}", e))?;
        for m in methods {
            if m.name != name {
                continue;
            }
            let ptags = sig_param_tags(&m.signature);
            if ptags.len() != argc {
                continue;
            }
            if ptags.iter().zip(arg_tags).all(|(p, a)| tag_compatible(*p, *a)) {
                return Ok(Some((tid, m)));
            }
            if fallback.is_none() {
                fallback = Some((tid, m));
            }
        }
        current = conn.get_superclass(tid).await.unwrap_or(None);
    }
    Ok(fallback)
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
            let arg_tags: Vec<u8> = argvals.iter().map(|v| v.tag).collect();
            let (decl, m) = find_method_for_args(conn, type_id, &seg.name, &arg_tags).await?
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

/// Emit get_stack's collapsed "hidden frames" marker (from package_filter) and reset the counter.
fn flush_hidden(output: &mut String, hidden: &mut usize) {
    if *hidden > 0 {
        output.push_str(&format!("   … {} frame(s) hidden\n", *hidden));
        *hidden = 0;
    }
}

/// JDWP threadStatus code -> short label (see types::ThreadStatus).
fn thread_status_name(ts: i32) -> &'static str {
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

// ----- conditional breakpoints -----

/// Evaluate a breakpoint condition on a thread's top frame. Returns true to KEEP the VM
/// suspended (condition true, or it couldn't be evaluated), false to auto-resume.
async fn evaluate_condition_on_thread(
    conn: &mut jdwp_client::JdwpConnection,
    thread_id: u64,
    cond: &str,
) -> bool {
    let frame = match conn.get_frames(thread_id, 0, 1).await {
        Ok(f) => match f.into_iter().next() {
            Some(fr) => fr,
            None => return true,
        },
        Err(_) => return true,
    };
    match eval_condition(conn, thread_id, &frame, cond).await {
        Ok(b) => b,
        Err(_) => true, // can't evaluate -> don't silently skip; keep suspended
    }
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
    cond: &str,
) -> Result<bool, String> {
    if let Some((lhs, op, rhs)) = split_comparison(cond) {
        let lv = resolve_expression(conn, thread_id, frame, &lhs).await?;
        let rlit = parse_lit(rhs.trim())?;
        compare_values(conn, &lv, &op, &rlit).await
    } else {
        let v = resolve_expression(conn, thread_id, frame, cond).await?;
        match v.data {
            jdwp_client::types::ValueData::Boolean(b) => Ok(b),
            _ => Err("Condition did not evaluate to a boolean".to_string()),
        }
    }
}

async fn compare_values(
    conn: &mut jdwp_client::JdwpConnection,
    lv: &jdwp_client::types::Value,
    op: &str,
    rlit: &ArgLit,
) -> Result<bool, String> {
    use jdwp_client::types::ValueData::*;
    let lnum: Option<f64> = match &lv.data {
        Int(v) => Some(*v as f64),
        Long(v) => Some(*v as f64),
        Short(v) => Some(*v as f64),
        Byte(v) => Some(*v as f64),
        Char(v) => Some(*v as f64),
        Float(v) => Some(*v as f64),
        Double(v) => Some(*v),
        _ => None,
    };
    let rnum: Option<f64> = match rlit {
        ArgLit::Int(v) => Some(*v as f64),
        ArgLit::Long(v) => Some(*v as f64),
        _ => None,
    };
    if let (Some(l), Some(r)) = (lnum, rnum) {
        return Ok(match op {
            "==" => l == r,
            "!=" => l != r,
            "<" => l < r,
            ">" => l > r,
            "<=" => l <= r,
            ">=" => l >= r,
            _ => return Err("bad operator".to_string()),
        });
    }
    if let (Boolean(l), ArgLit::Bool(r)) = (&lv.data, rlit) {
        return match op {
            "==" => Ok(l == r),
            "!=" => Ok(l != r),
            _ => Err("only == / != for booleans".to_string()),
        };
    }
    if let Object(id) = &lv.data {
        match rlit {
            ArgLit::Null => {
                return match op {
                    "==" => Ok(*id == 0),
                    "!=" => Ok(*id != 0),
                    _ => Err("only == / != with null".to_string()),
                }
            }
            ArgLit::Str(s) => {
                if *id == 0 {
                    return Ok(op == "!=");
                }
                let t = conn.get_object_reference_type(*id).await
                    .map_err(|e| format!("Failed to resolve type: {}", e))?;
                if conn.get_signature(t).await.unwrap_or_default() == "Ljava/lang/String;" {
                    let sv = conn.get_string_value(*id).await
                        .map_err(|e| format!("Failed to read string: {}", e))?;
                    return match op {
                        "==" => Ok(&sv == s),
                        "!=" => Ok(&sv != s),
                        _ => Err("only == / != for strings".to_string()),
                    };
                }
                return Err("Left side is not a String".to_string());
            }
            _ => {}
        }
    }
    Err("Unsupported comparison (numbers, booleans, null, or String value compares only)".to_string())
}
