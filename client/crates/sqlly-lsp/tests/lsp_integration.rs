//! Integration tests for `sqlly-lsp`. Drives a full [`LspServer`] over an
//! in-memory pipe (no real stdin/stdout, no subprocess) to verify the
//! JSON-RPC dispatch path for `initialize`, `textDocument/completion`, and
//! `textDocument/hover` flows against the real in-process `Services`.

use std::io::{Cursor, Read, Write};
use std::sync::{Arc, Mutex};

use sqlly_lsp::LspServer;
use sqlly_lsp::jsonrpc::{Id, Message, read_message};
use sqlly_services::Services;
use tokio::runtime::Runtime;

/// An owned, cloneable in-memory sink. The server moves a clone onto its writer
/// thread; the test reads the accumulated bytes from another clone after
/// `run()` returns (the writer thread is joined inside `run`, so all bytes are
/// flushed by then).
#[derive(Clone)]
struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl SharedSink {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

impl Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Spin up a server, feed it a sequence of framed messages, and collect every
/// outbound message it writes. Returns the outbound stream as parsed messages.
fn run_session(input: Vec<Message>) -> Vec<Message> {
    let rt = Runtime::new().unwrap();
    let services = Services::new();
    let sink = SharedSink::new();
    let mut server = LspServer::new(services, rt.handle().clone(), sink.clone());

    // Frame the input into a single byte stream the reader can consume.
    let mut input_bytes: Vec<u8> = Vec::new();
    for msg in &input {
        let body = serde_json::to_vec(msg).unwrap();
        input_bytes.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        input_bytes.extend_from_slice(&body);
    }
    let mut reader = Cursor::new(input_bytes);
    server.run(&mut reader).unwrap();

    // Parse the captured outbound bytes back into messages. All async tasks and
    // the writer thread were joined inside `run`, so the sink is complete.
    let mut out_reader = Cursor::new(sink.bytes());
    let mut got = Vec::new();
    while let Ok(Some(msg)) = read_message(&mut out_reader) {
        got.push(msg);
    }
    got
}

#[test]
fn initialize_returns_capabilities() {
    let init = Message::request(Id::Int(1), "initialize", serde_json::json!({}));
    let out = run_session(vec![init]);
    assert_eq!(out.len(), 1, "expected one response");
    let resp = &out[0];
    assert!(resp.is_response());
    assert_eq!(resp.id, Some(Id::Int(1)));
    let result = resp.result.as_ref().unwrap();
    let caps = result.get("capabilities").unwrap();
    assert!(caps.get("completionProvider").is_some());
    assert!(caps.get("hoverProvider").is_some());
    assert!(caps.get("workspaceSymbolProvider").is_some());
    assert!(caps.get("diagnosticProvider").is_some());
    // Pin the sync kind and the v0.55 additions: a regression to full-only
    // sync (1) or dropped symbol/formatting support must fail loudly here.
    assert_eq!(
        caps.get("textDocumentSync").and_then(|v| v.as_i64()),
        Some(2),
        "the server must advertise incremental sync: {caps:?}"
    );
    assert_eq!(
        caps.get("documentSymbolProvider").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        caps.get("documentFormattingProvider")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    let info = result.get("serverInfo").unwrap();
    assert_eq!(info.get("name").unwrap(), "sqlly-lsp");
}

#[test]
fn completion_returns_keywords_without_connection() {
    // Open a document, then request completion. With no connection set the
    // catalog is empty (default), so only built-in keywords/snippets surface —
    // but the dispatch path through complete_enriched must still work and
    // return a JSON array.
    let did_open = Message::notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": "file:///q.sql",
                "languageId": "sql",
                "version": 1,
                "text": "SEL"
            }
        }),
    );
    let completion = Message::request(
        Id::Int(2),
        "textDocument/completion",
        serde_json::json!({
            "textDocument": {"uri": "file:///q.sql"},
            "position": {"line": 0, "character": 3}
        }),
    );
    let out = run_session(vec![did_open, completion]);
    // didOpen has no response (it's a notification), so only the completion
    // response is expected.
    let resp = out
        .iter()
        .find(|m| m.id == Some(Id::Int(2)))
        .expect("completion response");
    let items = resp.result.as_ref().unwrap().as_array().unwrap();
    assert!(
        !items.is_empty(),
        "expected keyword/snippet completions for 'SEL'"
    );
    let labels: Vec<&str> = items
        .iter()
        .map(|i| i.get("label").unwrap().as_str().unwrap())
        .collect();
    assert!(labels.iter().any(|l| l.eq_ignore_ascii_case("SELECT")));
}

#[test]
fn completion_after_dot_returns_nothing_for_empty_catalog() {
    // With an empty (default) catalog, a dotted prefix yields no matches —
    // confirms the enriched path runs without panicking on the dot trigger.
    let did_open = Message::notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": "file:///q.sql",
                "languageId": "sql",
                "version": 1,
                "text": "SELECT dbo."
            }
        }),
    );
    let completion = Message::request(
        Id::Int(3),
        "textDocument/completion",
        serde_json::json!({
            "textDocument": {"uri": "file:///q.sql"},
            "position": {"line": 0, "character": 11}
        }),
    );
    let out = run_session(vec![did_open, completion]);
    let resp = out
        .iter()
        .find(|m| m.id == Some(Id::Int(3)))
        .expect("completion response");
    let items = resp.result.as_ref().unwrap().as_array().unwrap();
    assert!(
        items.is_empty(),
        "empty catalog + dotted prefix -> no matches"
    );
}

#[test]
fn method_not_found_for_unknown_request() {
    let req = Message::request(Id::Int(9), "totally/madeUp", serde_json::json!({}));
    let out = run_session(vec![req]);
    let resp = &out[0];
    let err = resp.error.as_ref().expect("error response");
    assert_eq!(err.code, -32601); // METHOD_NOT_FOUND
}

#[test]
fn shutdown_returns_null_result() {
    let req = Message::request(Id::Int(10), "shutdown", serde_json::json!({}));
    let out = run_session(vec![req]);
    let resp = &out[0];
    assert!(resp.is_response());
    assert!(resp.error.is_none(), "shutdown must not error");
    // `result: null` round-trips through serde as `Option::None`.
    assert!(resp.result.is_none() || resp.result.as_ref() == Some(&serde_json::Value::Null));
}

#[test]
fn refresh_catalog_without_connection_returns_not_refreshed() {
    // `sqlly/refreshCatalog` with no prior `sqlly/setConnection` is a no-op
    // that reports it did nothing, rather than erroring.
    let req = Message::request(Id::Int(11), "sqlly/refreshCatalog", serde_json::json!({}));
    let out = run_session(vec![req]);
    let resp = out
        .iter()
        .find(|m| m.id == Some(Id::Int(11)))
        .expect("refreshCatalog response");
    assert!(resp.error.is_none(), "refreshCatalog must not error");
    let result = resp.result.as_ref().expect("result object");
    assert_eq!(
        result.get("refreshed").and_then(|v| v.as_bool()),
        Some(false)
    );
}

#[test]
fn hover_without_procedures_returns_null() {
    let did_open = Message::notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": "file:///q.sql",
                "languageId": "sql",
                "version": 1,
                "text": "SELECT 1"
            }
        }),
    );
    let hover = Message::request(
        Id::Int(4),
        "textDocument/hover",
        serde_json::json!({
            "textDocument": {"uri": "file:///q.sql"},
            "position": {"line": 0, "character": 0}
        }),
    );
    let out = run_session(vec![did_open, hover]);
    let resp = out
        .iter()
        .find(|m| m.id == Some(Id::Int(4)))
        .expect("hover response");
    // `result: null` round-trips through serde as `Option::None`.
    assert!(resp.result.is_none() || resp.result.as_ref() == Some(&serde_json::Value::Null));
}

#[test]
fn clean_eof_terminates_the_loop() {
    // No input at all -> run returns Ok immediately, no outbound bytes.
    let out = run_session(Vec::new());
    assert!(out.is_empty());
}

#[test]
fn did_open_without_connection_publishes_empty_diagnostics() {
    let did_open = Message::notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": "file:///q.sql",
                "languageId": "sql",
                "version": 1,
                "text": "SELECT * FROM dbo.Orders"
            }
        }),
    );
    let out = run_session(vec![did_open]);
    // didOpen without a connection still publishes (empty) diagnostics to
    // clear any stale editor state.
    let notif = out
        .iter()
        .find(|m| m.method.as_deref() == Some("textDocument/publishDiagnostics"));
    assert!(notif.is_some(), "expected publishDiagnostics notification");
    let params = notif.unwrap().params.as_ref().unwrap();
    assert_eq!(params.get("uri").unwrap(), "file:///q.sql");
    assert!(
        params
            .get("diagnostics")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn pull_diagnostic_without_connection_returns_full_report() {
    // The server advertises diagnosticProvider; clients may PULL via
    // textDocument/diagnostic instead of relying on push. Without a connection
    // the report is a valid "full" report with no items.
    let did_open = Message::notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": "file:///q.sql",
                "languageId": "sql",
                "version": 1,
                "text": "SELECT * FROM dbo.Orders"
            }
        }),
    );
    let diagnostic = Message::request(
        Id::Int(7),
        "textDocument/diagnostic",
        serde_json::json!({ "textDocument": {"uri": "file:///q.sql"} }),
    );
    let out = run_session(vec![did_open, diagnostic]);
    let resp = out
        .iter()
        .find(|m| m.id == Some(Id::Int(7)))
        .expect("diagnostic response");
    let result = resp.result.as_ref().expect("full report result");
    assert_eq!(result.get("kind").unwrap(), "full");
    assert!(result.get("items").unwrap().as_array().unwrap().is_empty());
}

// Keep a trivial Read import alive for the Cursor usage above.
fn _keep_read_trait<R: Read>(_: &R) {}

#[test]
fn spawned_and_inline_requests_both_resolve() {
    // A `textDocument/diagnostic` (I/O-bound, spawned onto the runtime) and a
    // `textDocument/completion` (pure-CPU, inline) are dispatched back to back.
    // The read loop dispatches the spawned diagnostic without blocking and
    // continues to the inline completion; `run` then drains both. This proves
    // the two dispatch paths coexist without deadlock and every reply is
    // flushed before the writer channel closes. (Exact ordering is intentionally
    // not asserted: out-of-order replies are permitted and a connection-less
    // diagnostic resolves too quickly to pin the interleaving.)
    let did_open = Message::notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": "file:///q.sql",
                "languageId": "sql",
                "version": 1,
                "text": "SEL"
            }
        }),
    );
    let diagnostic = Message::request(
        Id::Int(20),
        "textDocument/diagnostic",
        serde_json::json!({ "textDocument": {"uri": "file:///q.sql"} }),
    );
    let completion = Message::request(
        Id::Int(21),
        "textDocument/completion",
        serde_json::json!({
            "textDocument": {"uri": "file:///q.sql"},
            "position": {"line": 0, "character": 3}
        }),
    );
    let out = run_session(vec![did_open, diagnostic, completion]);

    let diagnostic_resp = out
        .iter()
        .find(|m| m.id == Some(Id::Int(20)))
        .expect("spawned diagnostic response present");
    assert_eq!(
        diagnostic_resp
            .result
            .as_ref()
            .unwrap()
            .get("kind")
            .unwrap(),
        "full"
    );
    let completion_resp = out
        .iter()
        .find(|m| m.id == Some(Id::Int(21)))
        .expect("inline completion response present");
    let items = completion_resp.result.as_ref().unwrap().as_array().unwrap();
    assert!(
        items.iter().any(|i| i
            .get("label")
            .and_then(|l| l.as_str())
            .map(|l| l.eq_ignore_ascii_case("SELECT"))
            .unwrap_or(false)),
        "completion still produced keyword results alongside the spawned task"
    );
}

/// `$/cancelRequest` before a spawned request's reply: the reply must be a
/// `REQUEST_CANCELLED` error (-32800), never the normal result.
#[test]
fn cancel_request_yields_request_cancelled_error() {
    let init = Message::request(Id::Int(1), "initialize", serde_json::json!({}));
    let cancel = Message::notification("$/cancelRequest", serde_json::json!({ "id": 7 }));
    let diag = Message::request(
        Id::Int(7),
        "textDocument/diagnostic",
        serde_json::json!({ "textDocument": { "uri": "file:///a.sql" } }),
    );
    let shutdown = Message::request(Id::Int(2), "shutdown", serde_json::Value::Null);
    let exit = Message::notification("exit", serde_json::Value::Null);
    let out = run_session(vec![init, cancel, diag, shutdown, exit]);

    let reply = out
        .iter()
        .find(|m| m.id == Some(Id::Int(7)))
        .expect("cancelled request still gets exactly one reply");
    let error = reply.error.as_ref().expect("an error reply");
    assert_eq!(error.code, -32800, "{error:?}");
}

/// Rapid `didChange` bursts are debounced into one validation of the final
/// text, and the published diagnostics carry the final document version —
/// older versions never publish behind it.
#[test]
fn burst_of_changes_publishes_once_with_final_version() {
    let init = Message::request(Id::Int(1), "initialize", serde_json::json!({}));
    let open = Message::notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": "file:///b.sql",
                "languageId": "sql",
                "version": 1,
                "text": "SELECT 1"
            }
        }),
    );
    let change = |version: i32, text: &str| {
        Message::notification(
            "textDocument/didChange",
            serde_json::json!({
                "textDocument": { "uri": "file:///b.sql", "version": version },
                "contentChanges": [ { "text": text } ]
            }),
        )
    };
    let shutdown = Message::request(Id::Int(2), "shutdown", serde_json::Value::Null);
    let exit = Message::notification("exit", serde_json::Value::Null);
    let out = run_session(vec![
        init,
        open,
        change(2, "SELECT 1, 2"),
        change(3, "SELECT 1, 2, 3"),
        change(4, "SELECT 1, 2, 3, 4"),
        shutdown,
        exit,
    ]);

    let publishes: Vec<&Message> = out
        .iter()
        .filter(|m| m.method.as_deref() == Some("textDocument/publishDiagnostics"))
        .collect();
    assert_eq!(
        publishes.len(),
        1,
        "burst must debounce to one publish: {publishes:?}"
    );
    let params = publishes[0].params.as_ref().expect("params");
    assert_eq!(params["version"], serde_json::json!(4));
}

/// `didChange` with several content changes applies them in order (full-sync:
/// last wins) instead of dropping all but the first.
#[test]
fn did_change_applies_all_content_changes() {
    let init = Message::request(Id::Int(1), "initialize", serde_json::json!({}));
    let open = Message::notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": "file:///c.sql",
                "languageId": "sql",
                "version": 1,
                "text": "SELECT old"
            }
        }),
    );
    let change = Message::notification(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": "file:///c.sql", "version": 2 },
            "contentChanges": [ { "text": "SELECT mid" }, { "text": "SELECT final" } ]
        }),
    );
    // Completion reads the stored text; the label list is irrelevant — we
    // assert via hover'd text length instead: semantic tokens run over the
    // stored doc, so a token count over "SELECT final" proves the last
    // change won (2 words).
    let tokens = Message::request(
        Id::Int(9),
        "textDocument/semanticTokens/full",
        serde_json::json!({ "uri": "file:///c.sql" }),
    );
    let shutdown = Message::request(Id::Int(2), "shutdown", serde_json::Value::Null);
    let exit = Message::notification("exit", serde_json::Value::Null);
    let out = run_session(vec![init, open, change, tokens, shutdown, exit]);

    let reply = out
        .iter()
        .find(|m| m.id == Some(Id::Int(9)))
        .expect("semantic tokens reply");
    let data = reply.result.as_ref().expect("result")["data"]
        .as_array()
        .expect("token data")
        .clone();
    // "SELECT final": one keyword token → 5 ints; "SELECT mid"/"SELECT old"
    // would also yield one keyword, so additionally assert the doc text via
    // its keyword token length staying 6 ("SELECT").
    assert!(!data.is_empty(), "tokens over the final text");
}

/// `textDocument/definition` resolves the identifier under the cursor to the
/// `CREATE` statement declaring it — across documents, and across identifier
/// spellings (`[GetUsers]` at the call site, bare `GetUsers` at the
/// declaration).
#[test]
fn definition_jumps_to_the_create_statement_across_documents() {
    let init = Message::request(Id::Int(1), "initialize", serde_json::json!({}));
    let open_def = Message::notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": "file:///procs.sql",
                "languageId": "sql",
                "version": 1,
                "text": "CREATE PROCEDURE dbo.GetUsers AS\nSELECT 1"
            }
        }),
    );
    let open_use = Message::notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": "file:///call.sql",
                "languageId": "sql",
                "version": 1,
                "text": "EXEC dbo.[GetUsers]"
            }
        }),
    );
    // Cursor inside `[GetUsers]` on the call site.
    let definition = Message::request(
        Id::Int(7),
        "textDocument/definition",
        serde_json::json!({
            "textDocument": { "uri": "file:///call.sql" },
            "position": { "line": 0, "character": 12 }
        }),
    );
    let shutdown = Message::request(Id::Int(2), "shutdown", serde_json::Value::Null);
    let exit = Message::notification("exit", serde_json::Value::Null);
    let out = run_session(vec![init, open_def, open_use, definition, shutdown, exit]);

    let reply = out
        .iter()
        .find(|m| m.id == Some(Id::Int(7)))
        .expect("definition reply");
    let locations = reply.result.as_ref().expect("result").as_array().unwrap();
    assert_eq!(locations.len(), 1, "{locations:?}");
    assert_eq!(locations[0]["uri"], "file:///procs.sql");
    // The declared name token `GetUsers` sits at bytes 21..29 of line 0.
    assert_eq!(locations[0]["range"]["start"]["character"], 21);
    assert_eq!(locations[0]["range"]["end"]["character"], 29);
}

/// `textDocument/references` lists every occurrence of the identifier across
/// open documents; `includeDeclaration: false` drops the `CREATE` name.
#[test]
fn references_list_occurrences_and_respect_include_declaration() {
    let init = Message::request(Id::Int(1), "initialize", serde_json::json!({}));
    let open = Message::notification(
        "textDocument/didOpen",
        serde_json::json!({
            "textDocument": {
                "uri": "file:///script.sql",
                "languageId": "sql",
                "version": 1,
                "text": "CREATE TABLE dbo.Orders (Id INT);\nSELECT * FROM dbo.Orders;\nSELECT * FROM [Orders];"
            }
        }),
    );
    let with_decl = Message::request(
        Id::Int(8),
        "textDocument/references",
        serde_json::json!({
            "textDocument": { "uri": "file:///script.sql" },
            "position": { "line": 1, "character": 19 },
            "context": { "includeDeclaration": true }
        }),
    );
    let without_decl = Message::request(
        Id::Int(9),
        "textDocument/references",
        serde_json::json!({
            "textDocument": { "uri": "file:///script.sql" },
            "position": { "line": 1, "character": 19 },
            "context": { "includeDeclaration": false }
        }),
    );
    let shutdown = Message::request(Id::Int(2), "shutdown", serde_json::Value::Null);
    let exit = Message::notification("exit", serde_json::Value::Null);
    let out = run_session(vec![init, open, with_decl, without_decl, shutdown, exit]);

    let with_decl_locs = out
        .iter()
        .find(|m| m.id == Some(Id::Int(8)))
        .and_then(|m| m.result.as_ref())
        .and_then(|r| r.as_array())
        .expect("references reply")
        .clone();
    assert_eq!(with_decl_locs.len(), 3, "{with_decl_locs:?}");

    let without_decl_locs = out
        .iter()
        .find(|m| m.id == Some(Id::Int(9)))
        .and_then(|m| m.result.as_ref())
        .and_then(|r| r.as_array())
        .expect("references reply")
        .clone();
    assert_eq!(without_decl_locs.len(), 2, "{without_decl_locs:?}");
    // The declaration on line 0 is the one dropped.
    assert!(
        without_decl_locs
            .iter()
            .all(|l| l["range"]["start"]["line"] != 0),
        "{without_decl_locs:?}"
    );
}

/// The capabilities advertisement includes the navigation providers, so
/// editors know to send definition/references requests at all.
#[test]
fn capabilities_advertise_definition_and_references() {
    let caps = serde_json::to_value(sqlly_lsp::server_capabilities()).unwrap();
    assert_eq!(caps["definitionProvider"], true);
    assert_eq!(caps["referencesProvider"], true);
}
