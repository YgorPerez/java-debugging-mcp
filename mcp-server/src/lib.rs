// Lint policy — mirror the rust-doctor health gate (see `.github/workflows/`)
// locally so `cargo clippy` surfaces exactly what CI does. rust-doctor enables
// clippy's pedantic/nursery/cargo groups plus a curated set of restriction
// lints via command-line flags; declaring them here keeps the two in sync.
//
// This is the crate root the unit tests compile under, so the policy lives here as well as in
// `main.rs` — two roots, one policy, and neither can be dropped without the other noticing.
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

//! JDWP MCP server — Java debugging via the Model Context Protocol.
//!
//! # This library is not a supported surface
//!
//! **The supported surface of this package is the MCP tool set** — the `debug.*` tools, their argument
//! names and their replies, as `docs/toolkit-contract.md` describes them. It is reached by running the
//! `jdwp-mcp` binary and speaking JSON-RPC over stdio.
//!
//! Everything this library exports is `#[doc(hidden)]` and exists for one reason: so the repository's own
//! tests can cross the request→reply seam in-process instead of through a pipe (CLEAN-3, #186). None of it
//! is stable, none of it follows semver, and any of it may change or vanish in a patch release. This is
//! ADR-0044's rule — the library supports the operations it implements and nothing under them — applied to
//! the second crate rather than a new decision.
//!
//! If you are building against this, build against the tool surface instead.
//!
//! # What lives here and what stays in the binary
//!
//! Everything that turns a request into a reply is here: message parsing, the parse-error and
//! invalid-request branches, routing, [`RequestHandler`] and the session manager under it. None of it
//! performs I/O.
//!
//! The stdio adapter stays in `main.rs`: the stdin read loop, the single stdout-owning writer task and the
//! channel that feeds it, and process lifecycle. ADR-0012 makes stdout ownership an invariant, and the
//! invariant belongs with the task that holds it.

use anyhow::Result;
use serde_json::Value;
use tracing::error;

mod args;
mod classfile;
mod generics;
#[doc(hidden)]
pub mod handlers;
#[doc(hidden)]
pub mod protocol;
/// The read seam under the renderers (CLEAN-7, ADR-0049).
///
/// `pub` rather than private for a mechanical reason rather than an intent to publish: the `StatedDebuggee`
/// half is exercised only from `#[cfg(test)]` code, so behind a private module every one of its items
/// is dead code in a non-test build and the gate says so. It is `#[doc(hidden)]` and unsupported like
/// everything else here — see the crate docs.
#[doc(hidden)]
pub mod reads;
mod reply;
mod session;
mod stop_point;
mod stop_point_set;
mod tools;
mod value_reads;

#[doc(hidden)]
pub use handlers::RequestHandler;

use protocol::{
    JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, INVALID_REQUEST, PARSE_ERROR,
};

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

/// Turn one incoming line into the line that answers it, or `None` where the protocol says to stay quiet.
///
/// This is the whole request→reply path and it performs no I/O: a request gets handled and answered; a
/// notification is handled without a reply; anything unparseable yields a JSON-RPC error response. The
/// caller owns the transport — see `main.rs` for the stdio one and the tests for the in-process one.
///
/// `None` means *and correctly so*: a notification is supposed to get no reply. Everything that is not a
/// notification gets a line back, which is the property the invalid-request branch below exists to hold.
#[doc(hidden)]
pub async fn handle_message(handler: &RequestHandler, line: &str) -> Result<Option<String>> {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            error!("Parse error: {}", e);
            return Ok(Some(serde_json::to_string(&error_response(PARSE_ERROR, "Parse error"))?));
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
        return Ok(Some(serde_json::to_string(&error_response(
            INVALID_REQUEST,
            "Invalid request: a JSON-RPC message must be an object",
        ))?));
    }

    // Requests carry an id; notifications don't.
    if value.get("id").is_some() {
        match serde_json::from_value::<JsonRpcRequest>(value) {
            Ok(request) => {
                let response = handler.handle_request(request).await;
                Ok(Some(serde_json::to_string(&response)?))
            }
            Err(e) => {
                error!("Invalid request: {}", e);
                Ok(Some(serde_json::to_string(&error_response(INVALID_REQUEST, "Invalid request"))?))
            }
        }
    } else {
        match serde_json::from_value::<JsonRpcNotification>(value) {
            Ok(notification) => handler.handle_notification(&notification),
            Err(e) => error!("Invalid notification: {}", e),
        }
        Ok(None)
    }
}
