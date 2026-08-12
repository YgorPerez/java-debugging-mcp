// Lint policy — mirror the rust-doctor health gate (see `.github/workflows/`)
// locally so `cargo clippy` surfaces exactly what CI does. rust-doctor enables
// clippy's pedantic/nursery/cargo groups plus a curated set of restriction
// lints via command-line flags; declaring them here keeps the two in sync.
//
// The same policy is declared in `lib.rs`, which is the root the unit tests compile under. Both roots
// carry it because a crate attribute applies to one crate, and this package builds two.
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
// JDWP MCP Server — the stdio adapter.
//
// Everything that turns a request into a reply lives in the library beside this file
// (`jdwp_mcp::handle_message`), which performs no I/O. What stays here is transport and process
// lifecycle: the stdin read loop, the single stdout-owning writer task, and shutdown. ADR-0012 makes
// stdout ownership an invariant, and the invariant belongs with the task that holds it (CLEAN-3, #186).

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, info};

use jdwp_mcp::handle_message;
use jdwp_mcp::handlers::RequestHandler;
use jdwp_mcp::protocol::{Alerter, ALERT_CAPACITY};
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
                if let Some(response) = handle_message(&handler, line).await? {
                    send_message(&out_tx, response).await?;
                }
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
/// try-send path in `Alerter` instead, where dropping is the correct behaviour.
async fn send_message(out: &mpsc::Sender<String>, message: String) -> Result<()> {
    out.send(message).await.map_err(|_| anyhow::anyhow!("stdout writer task has gone away"))
}
