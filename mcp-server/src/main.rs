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
#![cfg_attr(test, allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic_in_result_fn
))]
// JDWP MCP Server - Java debugging via Model Context Protocol
//
// Provides LLM-friendly debugging tools for JVM applications via JDWP

use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, info};

mod args;
mod handlers;
mod protocol;
mod session;
mod tools;

use handlers::RequestHandler;
use protocol::{JsonRpcRequest, JsonRpcResponse, JsonRpcError, INVALID_REQUEST, JsonRpcNotification, PARSE_ERROR};

#[tokio::main]
async fn main() -> Result<()> {
    // Tracing to stderr only - stdout is reserved for JSON-RPC protocol
    let env_filter =
        tracing_subscriber::EnvFilter::from_default_env().add_directive("jdwp_mcp=info".parse()?);
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();

    info!("Starting JDWP MCP Server...");

    let handler = RequestHandler::new();

    // Stdio transport - no network, no files
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut stdout = stdout;

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
                process_line(&handler, &mut stdout, line).await?;
            }
            Err(e) => {
                error!("Read error: {}", e);
                break;
            }
        }
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
        error: Some(JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        }),
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

/// Parse and dispatch one incoming line: a request gets handled and answered; a notification is
/// handled without a reply; anything unparseable yields a JSON-RPC error response.
async fn process_line<W: AsyncWriteExt + Unpin>(
    handler: &RequestHandler,
    stdout: &mut W,
    line: &str,
) -> Result<()> {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            error!("Parse error: {}", e);
            let response = serde_json::to_string(&error_response(PARSE_ERROR, "Parse error"))?;
            return write_message(stdout, &response).await;
        }
    };

    // Requests carry an id; notifications don't.
    if value.get("id").is_some() {
        match serde_json::from_value::<JsonRpcRequest>(value) {
            Ok(request) => {
                let response = handler.handle_request(request).await;
                let response_str = serde_json::to_string(&response)?;
                write_message(stdout, &response_str).await?;
            }
            Err(e) => {
                error!("Invalid request: {}", e);
                let response = serde_json::to_string(&error_response(INVALID_REQUEST, "Invalid request"))?;
                write_message(stdout, &response).await?;
            }
        }
    } else {
        match serde_json::from_value::<JsonRpcNotification>(value) {
            Ok(notification) => RequestHandler::handle_notification(&notification),
            Err(e) => error!("Invalid notification: {}", e),
        }
    }
    Ok(())
}
