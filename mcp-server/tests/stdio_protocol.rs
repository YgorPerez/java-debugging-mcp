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
const INVALID_PARAMS: i64 = -32602;
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// The version whose per-request metadata this server serves statelessly (MCP-1).
const MODERN: &str = "2026-07-28";
/// The revision `initialize` negotiates, for clients that predate the stateless model.
const LEGACY: &str = "2024-11-05";

/// The `params` a stateless request carries: its own protocol version and capabilities, every time.
///
/// Written out here rather than borrowed from the crate, on purpose. These are **wire** constants — a
/// client on the other side of a pipe has no access to our types — so a test that imported them could
/// not catch a rename, which is the one failure the downstream toolkit cannot see either
/// (`docs/toolkit-contract.md`).
fn stateless(version: &str, extra: &serde_json::Value) -> serde_json::Value {
    let mut meta = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": version,
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { "name": "stdio_protocol.rs", "version": "0" },
    });
    if let (Some(m), Some(e)) = (meta.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            m.insert(k.clone(), v.clone());
        }
    }
    serde_json::json!({ "_meta": meta })
}

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

    let status =
        server.close_stdin_and_wait(Duration::from_secs(10)).expect("server did not exit after EOF on stdin");
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

// ---------------------------------------------------------------------------
// MCP-1: the 2026-07-28 stateless surface, alongside the `initialize` handshake.
//
// The property under test throughout is DUAL-ERA: every case below asserts the new behaviour without
// asserting away the old one, because the old one is what every client using this server today speaks —
// including the pinned downstream toolkit, whose failure modes are mostly silent
// (`docs/toolkit-contract.md`). A revision bump that quietly stopped answering `initialize` would look
// exactly like success from in here.
// ---------------------------------------------------------------------------

/// `server/discover` is the one RPC a 2026-07-28 server MUST implement, and on stdio it is also the
/// probe a dual-era client uses to decide whether to fall back to `initialize`.
///
/// So the assertion that matters is not merely "it answers" but that it answers with a **`DiscoverResult`
/// rather than an error**: those are the two outcomes the client's fallback rule turns on, and any error
/// here — including a well-meaning one — is read as "this server is legacy".
#[test]
fn the_discovery_probe_answers_with_a_result_and_not_an_error() {
    let mut server = Server::start().expect("start server");
    let reply =
        server.request("server/discover", stateless(MODERN, &serde_json::json!({}))).expect("discover");

    assert!(reply.get("error").is_none(), "a probe answered with an error reads as a LEGACY server: {reply}");
    let result = &reply["result"];
    assert_eq!(result["resultType"], "complete", "every result carries its type: {reply}");
    let versions = result["supportedVersions"].as_array().expect("supportedVersions must be an array");
    assert!(
        versions.iter().any(|v| v == MODERN),
        "the probe must name the version it serves, or a client has nothing to select: {reply}"
    );
    assert!(result["capabilities"]["tools"].is_object(), "tools capability must be declared: {reply}");
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "jdwp-mcp",
        "the server identifies itself in the result, there being no handshake to do it in: {reply}"
    );
    assert!(result["ttlMs"].is_u64() && result["cacheScope"] == "public", "CacheableResult: {reply}");

    // NOT declared, and this is a claim about behaviour rather than tidiness: a stateless client's
    // alerts go to stderr because the protocol has nowhere legal to put them, so declaring `logging`
    // here would promise notifications that can never arrive.
    assert!(
        result["capabilities"]["logging"].is_null(),
        "logging must not be declared to a client that cannot legally be sent any: {reply}"
    );
}

/// A version this server does not serve is answered with the list to retry from — the client's entire
/// recovery path is to pick from it, so an error that only said "no" would strand it.
#[test]
fn an_unserved_version_is_told_what_to_retry_with() {
    let mut server = Server::start().expect("start server");
    let reply = server
        .request("tools/list", stateless("1900-01-01", &serde_json::json!({})))
        .expect("versioned call");

    assert_eq!(
        error_code(&reply, "an unsupported version"),
        UNSUPPORTED_PROTOCOL_VERSION,
        "must be the spec's own code, since a dual-era client uses it to tell modern from legacy: {reply}"
    );
    let data = &reply["error"]["data"];
    assert_eq!(data["requested"], "1900-01-01", "the error must name what was asked for: {reply}");
    assert!(
        data["supported"].as_array().is_some_and(|s| s.iter().any(|v| v == MODERN)),
        "and what to use instead: {reply}"
    );
    assert_still_serving(&mut server, "an unsupported protocol version");
}

/// The dual-era rule, in one test: a missing protocol version is a **legacy** request everywhere it
/// could be one, and malformed only where it could not.
///
/// `tools/list` with no `_meta` is exactly what every client that works today sends, so refusing it
/// would be the regression this whole change has to avoid. `server/discover` exists only in the modern
/// era, so there is nothing else a missing field there could mean.
#[test]
fn a_missing_protocol_version_is_legacy_except_where_it_cannot_be() {
    let mut server = Server::start().expect("start server");

    let bare = server.request("tools/list", serde_json::json!({})).expect("bare tools/list");
    assert!(
        bare["result"]["tools"].as_array().is_some_and(|t| !t.is_empty()),
        "a request with no _meta is a legacy request and must be served: {bare}"
    );

    let probe = server.request("server/discover", serde_json::json!({})).expect("bare discover");
    assert_eq!(
        error_code(&probe, "discover without _meta"),
        INVALID_PARAMS,
        "a modern-only method with no version is malformed, not legacy: {probe}"
    );
}

/// Capabilities are required on a stateless request, and "declared none" has to be distinguishable from
/// "did not say" for the rule that a server MUST NOT rely on an undeclared capability to mean anything.
#[test]
fn a_stateless_request_must_declare_its_capabilities() {
    let mut server = Server::start().expect("start server");
    let reply = server
        .request(
            "tools/list",
            serde_json::json!({ "_meta": { "io.modelcontextprotocol/protocolVersion": MODERN } }),
        )
        .expect("call with no capabilities");

    assert_eq!(
        error_code(&reply, "no clientCapabilities"),
        INVALID_PARAMS,
        "a required field that is absent is malformed: {reply}"
    );
    assert_still_serving(&mut server, "a request with no declared capabilities");
}

/// An unrecognised log level is refused by name rather than accepted and ignored. Accepting it would be
/// a silent promise to filter by something this server cannot honour.
#[test]
fn an_unrecognised_log_level_is_refused_rather_than_ignored() {
    let mut server = Server::start().expect("start server");
    let extra = serde_json::json!({ "io.modelcontextprotocol/logLevel": "chatty" });
    let reply = server.request("tools/list", stateless(MODERN, &extra)).expect("call with a bad log level");

    assert_eq!(error_code(&reply, "a bogus log level"), INVALID_PARAMS, "{reply}");

    // And a real one is accepted, so the check above is discriminating rather than blanket.
    let good = serde_json::json!({ "io.modelcontextprotocol/logLevel": "warning" });
    let ok = server.request("tools/list", stateless(MODERN, &good)).expect("call with a real log level");
    assert!(ok["result"]["tools"].is_array(), "a valid RFC 5424 level must be accepted: {ok}");
}

/// `resultType` and `serverInfo` ride on **every** result, in both eras.
///
/// Both eras deliberately: the fields are inert to a client that predates them (a result is an open
/// object in every revision), and one stamped path cannot drift from another the way two would.
#[test]
fn every_result_carries_its_type_and_the_servers_identity() {
    let mut server = Server::start().expect("start server");
    let stateless_list =
        server.request("tools/list", stateless(MODERN, &serde_json::json!({}))).expect("modern");
    let legacy_list = server.request("tools/list", serde_json::json!({})).expect("legacy");
    let handshake = server
        .request("initialize", serde_json::json!({"protocolVersion": LEGACY, "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}))
        .expect("initialize");

    for (what, reply) in
        [("stateless", &stateless_list), ("legacy", &legacy_list), ("initialize", &handshake)]
    {
        assert_eq!(reply["result"]["resultType"], "complete", "{what} result needs a resultType: {reply}");
        assert_eq!(
            reply["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "jdwp-mcp",
            "{what} result needs serverInfo: {reply}"
        );
    }

    // The list is cacheable and its order is fixed, which is what lets a client cache it at all.
    assert!(stateless_list["result"]["ttlMs"].is_u64(), "tools/list must be cacheable: {stateless_list}");
    assert_eq!(
        stateless_list["result"]["tools"], legacy_list["result"]["tools"],
        "the tool list MUST NOT vary by era or by connection state"
    );
}

/// The legacy handshake still works, still negotiates its own revision, and still declares `logging` —
/// the capability is true for this era, where the notifications really are sent.
#[test]
fn the_legacy_handshake_is_untouched() {
    let mut server = Server::start().expect("start server");
    let reply = server
        .request("initialize", serde_json::json!({"protocolVersion": LEGACY, "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}))
        .expect("initialize");

    let result = &reply["result"];
    assert_eq!(result["protocolVersion"], LEGACY, "the handshake must still negotiate its revision: {reply}");
    assert!(result["capabilities"]["tools"].is_object(), "{reply}");
    assert!(
        result["capabilities"]["logging"].is_object(),
        "a legacy client is still pushed notifications, so the capability is still true: {reply}"
    );
    assert!(result["instructions"].as_str().is_some_and(|s| s.contains("debug.attach")), "{reply}");
    assert_still_serving(&mut server, "the legacy handshake");
}

/// `subscriptions/listen` is answered, not refused — and the acknowledgment comes **first**.
///
/// Ordering is the assertion that carries the weight: the spec says the acknowledgment MUST be the first
/// message on a subscription, and on stdio everything shares one channel, so "first" is a property of the
/// line order and nothing else. A server that queued the response first would look correct in every field
/// and still be wrong.
///
/// The honoured set is empty because none of the four filter types can ever fire here: no resources, no
/// prompts, and a compiled-in tool list. So the subscription is opened and closed in one exchange, which
/// is what the spec's graceful-closure result is for.
#[test]
fn a_subscription_is_acknowledged_first_and_then_closed_gracefully() {
    let mut server = Server::start().expect("start server");
    server
        .send_raw(&format!(
            r#"{{"jsonrpc":"2.0","id":31,"method":"subscriptions/listen","params":{}}}"#,
            serde_json::json!({
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": MODERN,
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "notifications": { "toolsListChanged": true, "resourcesListChanged": true },
            })
        ))
        .expect("write subscriptions/listen");

    let ack = server.read_reply().expect("an acknowledgment");
    assert_eq!(
        ack["method"], "notifications/subscriptions/acknowledged",
        "the acknowledgment must be the FIRST message on the subscription, before the response: {ack}"
    );
    assert!(ack.get("id").is_none(), "an acknowledgment is a notification and carries no id: {ack}");
    assert_eq!(
        ack["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"], 31,
        "it must carry the subscription id, which is the listen request's own id: {ack}"
    );
    assert_eq!(
        ack["params"]["notifications"],
        serde_json::json!({}),
        "nothing here can ever fire, so the honoured set is empty rather than echoed back: {ack}"
    );

    let closed = server.read_reply().expect("the graceful-closure result");
    assert_eq!(closed["id"], 31, "the close is the response to the listen request: {closed}");
    assert_eq!(closed["result"]["resultType"], "complete", "{closed}");
    assert_eq!(
        closed["result"]["_meta"]["io.modelcontextprotocol/subscriptionId"], 31,
        "an empty result correlated by id is how a clean end is told from a dropped transport: {closed}"
    );
    assert!(
        closed["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"] == "jdwp-mcp",
        "and the central stamp still applies to it: {closed}"
    );
    assert_still_serving(&mut server, "a subscription");
}

/// A cursor this server never minted is refused rather than ignored (MCP-1).
///
/// `tools/list` returns every tool in one page and never issues a `nextCursor`, so any cursor is one the
/// client did not get from here. Ignoring it would leave a client that believes it is paginating reading
/// page one forever with no way to find out — the silence-as-answer failure this codebase is built
/// against. An empty string is included on purpose: the spec is explicit that `""` is a valid cursor and
/// must not be read as the end of results, so it is refused like any other.
#[test]
fn a_cursor_this_server_never_issued_is_refused() {
    let mut server = Server::start().expect("start server");
    for cursor in [serde_json::json!("eyJwYWdlIjogMn0="), serde_json::json!("")] {
        let mut params = stateless(MODERN, &serde_json::json!({}));
        if let Some(o) = params.as_object_mut() {
            o.insert("cursor".to_string(), cursor.clone());
        }
        let reply = server.request("tools/list", params).expect("paginated tools/list");
        assert_eq!(
            error_code(&reply, &format!("cursor {cursor}")),
            INVALID_PARAMS,
            "an unknown cursor is -32602, not a silently ignored argument: {reply}"
        );
    }

    // A null cursor is an ABSENT cursor, not an invalid one — otherwise a client that serialises its
    // optional fields as null could never list tools at all.
    let mut params = stateless(MODERN, &serde_json::json!({}));
    if let Some(o) = params.as_object_mut() {
        o.insert("cursor".to_string(), serde_json::Value::Null);
    }
    let reply = server.request("tools/list", params).expect("null-cursor tools/list");
    assert!(reply["result"]["tools"].is_array(), "a null cursor must read as absent: {reply}");
    assert!(reply["result"].get("nextCursor").is_none(), "one page means no nextCursor: {reply}");
}
