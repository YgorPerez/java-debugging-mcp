// Lint policy — mirror the rust-doctor health gate (see `.github/workflows/`)
// locally so `cargo clippy` surfaces exactly what CI does. rust-doctor enables
// clippy's pedantic/nursery/cargo groups plus a curated set of restriction
// lints via command-line flags; declaring them here keeps the two in sync.
#![warn(clippy::pedantic, clippy::nursery)]
#![warn(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::format_push_string,
    clippy::panic_in_result_fn,
    clippy::print_stdout,
    clippy::print_stderr
)]
// Restriction lints above target production code; unit tests may panic on failure, so `unwrap`,
// `expect`, indexing, and assertions are idiomatic there.
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic_in_result_fn)
)]
// JDWP MCP Server - Java debugging via Model Context Protocol
//
// Provides LLM-friendly debugging tools for JVM applications via JDWP

use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, info};

mod args;
mod classfile;
mod handlers;
mod protocol;
mod session;
mod tools;

use handlers::RequestHandler;
use protocol::{
    Alerter, JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, ALERT_CAPACITY,
    INVALID_REQUEST, PARSE_ERROR,
};
use tokio::sync::mpsc;

/// How long to let the writer task drain after stdin closes, before giving up on it (EVT-2).
///
/// Bounded rather than a plain join: the event pump and watchdog tasks hold `Alerter` clones and are
/// not guaranteed to have stopped, so waiting for the channel to close outright could hang a process
/// that is already shutting down.
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[tokio::main]
async fn main() -> Result<()> {
    // Tracing to stderr only - stdout is reserved for JSON-RPC protocol
    //
    // `jdwp_client=warn` is here because leaving it out silenced the one crate that witnesses transport
    // failure. The event loop logs a lost connection at `error!`, and with only a `jdwp_mcp` directive
    // that line went nowhere by default — so the operator saw neither the cause in the reply (fixed by
    // carrying it in `JdwpError::ConnectionClosed`) nor a log line naming it. `warn` and above is quiet
    // in a healthy session: the loop logs at `debug`/`info` per packet, which stays off.
    let env_filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive("jdwp_mcp=info".parse()?)
        .add_directive("jdwp_client=warn".parse()?);
    tracing_subscriber::fmt().with_env_filter(env_filter).with_writer(std::io::stderr).init();

    info!("Starting JDWP MCP Server...");

    // EVT-2: ONE task owns stdout, and every outbound line goes through this channel to reach it —
    // responses from the loop below, alerts from the JDWP event pump and the watchdog. That
    // single writer is the whole interleaving guarantee: a hit landing mid-response cannot split it,
    // because the pump does not write, it queues.
    //
    // The two producers use different disciplines on purpose. A response is sent with `.await`, so a
    // slow stdout applies backpressure and nothing is ever lost. An alert uses `try_send` and is
    // dropped (and counted) when the queue is full, because making the debuggee's event pump wait on
    // how fast an MCP client drains its pipe would be a far worse failure than a missed hint the
    // caller can still read with `debug.get_last_event`.
    let (out_tx, mut out_rx) = mpsc::channel::<String>(ALERT_CAPACITY);
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if let Err(e) = write_message(&mut stdout, &line).await {
                error!("Write error: {e}");
                break;
            }
        }
    });

    let alerter = Alerter::new(out_tx.clone());
    let handler = RequestHandler::new(alerter);

    let mut reader = BufReader::new(tokio::io::stdin());

    info!("JDWP MCP server ready, waiting for requests...");

    // Single-threaded message loop. Reuse one buffer across iterations rather
    // than allocating a fresh String per line.
    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        match reader.read_line(&mut line_buf).await {
            Ok(0) => {
                info!("Client disconnected");
                break;
            }
            Ok(_) => {
                let line = line_buf.trim();
                if line.is_empty() {
                    continue;
                }
                debug!("Received: {}", line);
                process_line(&handler, &out_tx, line).await?;
            }
            Err(e) => {
                error!("Read error: {}", e);
                break;
            }
        }
    }

    // Drop every sender we hold so the writer sees the channel close and flushes what is queued.
    drop(out_tx);
    drop(handler);
    if tokio::time::timeout(DRAIN_TIMEOUT, writer).await.is_err() {
        error!("writer task did not finish draining within {DRAIN_TIMEOUT:?}");
    }

    info!("JDWP MCP server shutting down");
    Ok(())
}

/// Build a JSON-RPC error response with a null id (used for messages we couldn't parse or route).
fn error_response(code: i32, message: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: serde_json::Value::Null,
        result: None,
        error: Some(JsonRpcError { code, message: message.to_string(), data: None }),
    }
}

/// Name a JSON value's kind for an error message — what arrived instead of an object.
const fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Write one framed JSON-RPC message (line + newline) to stdout and flush.
async fn write_message<W: AsyncWriteExt + Unpin>(stdout: &mut W, message: &str) -> Result<()> {
    debug!("Sending: {}", message);
    stdout.write_all(message.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

/// Queue one outbound line for the writer task.
///
/// `.await`s for capacity rather than dropping: this path carries **responses**, and a dropped
/// response leaves a client waiting on a reply that will never come. Alerts take the
/// try-send path in [`Alerter`] instead, where dropping is the correct behaviour.
async fn send_message(out: &mpsc::Sender<String>, message: String) -> Result<()> {
    out.send(message).await.map_err(|_| anyhow::anyhow!("stdout writer task has gone away"))
}

/// Parse and dispatch one incoming line: a request gets handled and answered; a notification is
/// handled without a reply; anything unparseable yields a JSON-RPC error response.
async fn process_line(handler: &RequestHandler, out: &mpsc::Sender<String>, line: &str) -> Result<()> {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            error!("Parse error: {}", e);
            let response = serde_json::to_string(&error_response(PARSE_ERROR, "Parse error"))?;
            return send_message(out, response).await;
        }
    };

    // A top-level value that is not an object is neither a request nor a notification, and the
    // distinction matters because of what the two silences mean. A notification is *supposed* to get no
    // reply, so the branch below stays quiet for anything object-shaped without an `id`. A bare scalar or
    // array has no `id` either, and used to fall into that same branch — parsed as a notification,
    // failed, logged to stderr, answered with nothing. So `42` or `"hello"` (both valid JSON, so not a
    // parse error) left a client waiting forever on a reply that was never coming, which is the one
    // outcome worse than an error. JSON-RPC 2.0's own example for a non-object is an Invalid Request with
    // a null id; found by TEST-9 (#25) while covering these arms.
    if !value.is_object() {
        error!("Not a JSON-RPC message: expected an object, got {}", kind_of(&value));
        let response = serde_json::to_string(&error_response(
            INVALID_REQUEST,
            "Invalid request: a JSON-RPC message must be an object",
        ))?;
        return send_message(out, response).await;
    }

    // Requests carry an id; notifications don't.
    if value.get("id").is_some() {
        match serde_json::from_value::<JsonRpcRequest>(value) {
            Ok(request) => {
                let response = handler.handle_request(request).await;
                send_message(out, serde_json::to_string(&response)?).await?;
            }
            Err(e) => {
                error!("Invalid request: {}", e);
                let response = serde_json::to_string(&error_response(INVALID_REQUEST, "Invalid request"))?;
                send_message(out, response).await?;
            }
        }
    } else {
        match serde_json::from_value::<JsonRpcNotification>(value) {
            Ok(notification) => handler.handle_notification(&notification),
            Err(e) => error!("Invalid notification: {}", e),
        }
    }
    Ok(())
}
