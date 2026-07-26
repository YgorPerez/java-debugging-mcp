// The process's JSON-RPC front door, driven with input a client should never send (TEST-9, #25).
//
// Every other test in this harness constructs a *well-formed* request, so the whole suite only ever
// exercised the happy path of the stdio read loop — `main.rs` sat at 65% region coverage with its parse
// and validation arms unexecuted. The part standing between a buggy or hostile client and the debugger
// was the part nothing drove.
//
// These need **no JDK and no JVM**, only the server binary, so unlike `mcp_integration.rs` they are not
// `#[ignore]`d and run in the default `cargo test`.
//
// The property under test is not "an error comes back" but "an error comes back AND the server is still
// serving": one bad line from a client must not end the session. So each case follows its malformed
// input with a real request and asserts that still works.

mod common;

use common::Server;
use std::time::Duration;

/// JSON-RPC error codes, from `mcp-server/src/protocol.rs`.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;

/// The `error.code` of a reply, or a panic naming what came back instead.
fn error_code(reply: &serde_json::Value, what: &str) -> i64 {
    reply["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("{what}: expected a JSON-RPC error object, got {reply}"))
}

/// Assert the server still answers a real request — the half that matters. A reply to the junk is worth
/// nothing if the process died writing it.
fn assert_still_serving(server: &mut Server, after: &str) {
    let tools = server
        .request("tools/list", serde_json::json!({}))
        .unwrap_or_else(|e| panic!("server stopped serving after {after}: {e}"));
    let count = tools["result"]["tools"].as_array().map_or(0, Vec::len);
    assert!(count > 0, "server answered after {after} but listed no tools: {tools}");
}

/// A line that is not JSON at all: parse error, and the session survives it.
#[test]
fn a_line_that_is_not_json_is_a_parse_error_and_the_server_keeps_serving() {
    let mut server = Server::start().expect("start server");

    for junk in ["{not json", "]", "{\"unterminated\": ", "{\"a\": 1} trailing"] {
        let reply = server.raw(junk).unwrap_or_else(|e| panic!("no reply to {junk:?}: {e}"));
        assert_eq!(error_code(&reply, junk), PARSE_ERROR, "wrong code for {junk:?}: {reply}");
        // The id is null because there is nothing to read one from — the line never parsed.
        assert_eq!(reply["id"], serde_json::Value::Null, "unparseable input must answer with a null id");
        assert_eq!(reply["jsonrpc"], "2.0", "the error must itself be well-formed JSON-RPC: {reply}");
    }

    assert_still_serving(&mut server, "four unparseable lines");
}

/// Valid JSON that is not a JSON-RPC message at all, because it is not an object.
///
/// The case this test was written for was a hang, not a wrong code: a bare scalar has no `id`, so it fell
/// into the notification branch, failed to parse as one, and was answered with **nothing** — a client
/// waiting on a reply waits forever. A notification's silence is correct; this one was not (TEST-9, #25).
#[test]
fn valid_json_that_is_not_an_object_gets_an_error_rather_than_silence() {
    let mut server = Server::start().expect("start server");

    for not_an_object in ["42", "\"a bare string\"", "true", "null", r#"[{"jsonrpc": "2.0"}]"#] {
        let reply = server
            .raw(not_an_object)
            .unwrap_or_else(|e| panic!("no reply to {not_an_object:?} — silence is the bug: {e}"));
        assert_eq!(error_code(&reply, not_an_object), INVALID_REQUEST, "wrong code: {reply}");
        assert_eq!(reply["id"], serde_json::Value::Null, "there is no id to echo: {reply}");
    }

    assert_still_serving(&mut server, "five non-object messages");
}

/// Valid JSON, valid enough to look like a request (it has an `id`), but not a JSON-RPC request: no
/// `method`. Must be refused rather than panicking on the missing field.
#[test]
fn a_json_object_that_is_not_a_request_is_refused_without_killing_the_server() {
    let mut server = Server::start().expect("start server");

    // No `method` at all.
    let reply = server.raw(r#"{"jsonrpc": "2.0", "id": 41, "params": {}}"#).expect("reply to id-only");
    assert_eq!(error_code(&reply, "missing method"), INVALID_REQUEST, "{reply}");

    // `method` present but not a string, so deserialization fails on the type rather than the absence.
    let reply = server.raw(r#"{"jsonrpc": "2.0", "id": 42, "method": 7}"#).expect("reply to numeric method");
    assert_eq!(error_code(&reply, "non-string method"), INVALID_REQUEST, "{reply}");

    // An `id` of an unexpected shape. `JsonRpcRequest::id` is a bare `Value`, so an object id is
    // accepted and echoed — the spec says a client picks the id, and JSON-RPC only asks that it come
    // back unchanged. Asserted because "accepted deliberately" and "accepted by accident" look the same
    // from outside, and this is the arm a strict server would reject instead.
    let reply = server
        .raw(r#"{"jsonrpc": "2.0", "id": {"weird": true}, "method": "tools/list"}"#)
        .expect("reply to object id");
    assert_eq!(reply["id"], serde_json::json!({"weird": true}), "an id must come back exactly: {reply}");
    assert!(reply["result"]["tools"].is_array(), "an odd id must not stop the call: {reply}");

    assert_still_serving(&mut server, "three malformed requests");
}

/// An unknown `method` is a routing failure, not a parse failure — it must say so with the right code.
#[test]
fn an_unknown_method_is_method_not_found() {
    let mut server = Server::start().expect("start server");

    let reply = server.request("tools/nope", serde_json::json!({})).expect("reply to unknown method");
    assert_eq!(error_code(&reply, "unknown method"), METHOD_NOT_FOUND, "{reply}");
    assert!(
        reply["error"]["message"].as_str().unwrap_or_default().contains("tools/nope"),
        "the error should name the method that wasn't found: {reply}"
    );

    assert_still_serving(&mut server, "an unknown method");
}

/// A message with no `id` is a notification: handled, never answered. Blank lines are skipped the same
/// way. Both are silent paths, so the test proves the silence rather than assuming it — it sends a real
/// request afterwards and insists the *next* line out is that request's reply.
#[test]
fn notifications_and_blank_lines_are_answered_with_nothing_at_all() {
    let mut server = Server::start().expect("start server");

    for quiet in [
        r#"{"jsonrpc": "2.0", "method": "notifications/initialized"}"#, // a real MCP notification
        r#"{"jsonrpc": "2.0", "method": "notifications/unheard-of"}"#,  // unknown: logged, not answered
        r#"{"jsonrpc": "2.0", "params": {}}"#,                          // no id AND no method: invalid
        "",
        "   ",
    ] {
        server.send_raw(quiet).expect("write a quiet line");
    }

    // Ordering is what makes this an assertion: the server handles lines in sequence, so if any of the
    // five had produced output, it would be sitting in front of this reply.
    server.send_raw(r#"{"jsonrpc": "2.0", "id": 99, "method": "tools/list"}"#).expect("write request");
    let next = server.read_reply().expect("reply to the request after the quiet lines");
    assert_eq!(next["id"], 99, "something replied to a notification or a blank line: {next}");
    assert!(next["result"]["tools"].is_array(), "the request after the quiet lines must succeed: {next}");
}

/// EOF on stdin ends the process, cleanly and promptly.
///
/// This is the shutdown path the harness itself depends on: `Drop` closes stdin rather than `kill()`ing,
/// because coverage counters flush in an `atexit` handler that SIGKILL skips. A server that hung on EOF
/// would leak a process per session and take the coverage numbers with it.
#[test]
fn eof_on_stdin_exits_cleanly() {
    let mut server = Server::start().expect("start server");
    assert_still_serving(&mut server, "startup");

    let status = server
        .close_stdin_and_wait(Duration::from_secs(10))
        .expect("server did not exit after EOF on stdin");
    assert!(status.success(), "EOF should be a clean exit, got {status:?}");
}

/// A stream that ends mid-message: the last request has no trailing newline.
///
/// `read_line` blocks on a partial line until the newline **or EOF**, so nothing is answered while the
/// pipe is open — closing stdin is what delivers it. The server must then answer it before exiting,
/// rather than discarding a complete-but-unterminated request, and must not treat the missing newline as
/// a parse failure.
#[test]
fn a_final_request_without_a_trailing_newline_is_answered_at_eof() {
    let mut server = Server::start().expect("start server");
    server
        .send_raw_unterminated(r#"{"jsonrpc": "2.0", "id": 7, "method": "tools/list"}"#)
        .expect("write an unterminated request");

    // Closing stdin both delivers the partial line and ends the loop, in that order.
    let status = server.close_stdin_and_wait(Duration::from_secs(10)).expect("exit after EOF");
    assert!(status.success(), "expected a clean exit, got {status:?}");

    let reply = server.read_reply().expect("reply to an unterminated request");
    assert_eq!(reply["id"], 7, "an unterminated final line must still be answered: {reply}");
    assert!(reply["result"]["tools"].is_array(), "and answered properly, not with an error: {reply}");
}
