//! JSON-RPC 2.0 over stdio framing for the LSP wire protocol.
//!
//! Each message is a UTF-8 JSON object preceded by a `Content-Length: N\r\n\r\n`
//! header (per the LSP base protocol). [`read_message`] parses one message from
//! a blocking reader; [`write_message`] serializes and writes one message with
//! the header to a writer. Both are generic so tests can drive the server over
//! an in-memory pipe without touching real stdin/stdout.

use std::io::{self, BufRead, Write};

/// A parsed JSON-RPC message envelope.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub params: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<RpcError>,
}

/// JSON-RPC id — a number or a string (null ids are not used here).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Id {
    Int(i64),
    Str(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub data: Option<serde_json::Value>,
}

impl Message {
    /// Build a successful response for a request id.
    pub fn response(id: Id, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            method: None,
            params: None,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error response for a request id.
    pub fn error(id: Id, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            method: None,
            params: None,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    /// Build a notification (no id).
    pub fn notification(method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    /// Build a request.
    pub fn request(id: Id, method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    pub fn is_request(&self) -> bool {
        self.id.is_some() && self.method.is_some()
    }

    pub fn is_response(&self) -> bool {
        self.id.is_some() && self.method.is_none()
    }

    pub fn is_notification(&self) -> bool {
        self.id.is_none() && self.method.is_some()
    }
}

/// Standard JSON-RPC error codes.
pub mod error_code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
    /// LSP server-not-initialized (per LSP spec).
    pub const SERVER_NOT_INITIALIZED: i32 = -32002;
    /// LSP request cancelled.
    pub const REQUEST_CANCELLED: i32 = -32800;
}

/// Read one LSP message (header + body) from a buffered reader. Returns
/// `Ok(None)` on clean EOF (the client closed stdin). Returns an error on a
/// malformed header or a truncated body.
pub fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Message>> {
    // Accumulate header bytes until the delimiter, then parse Content-Length.
    let mut header = Vec::new();
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            // EOF before any header: clean shutdown.
            if header.is_empty() && content_length.is_none() {
                return Ok(None);
            }
            // EOF mid-header: truncated.
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected EOF in LSP header",
            ));
        }
        // An empty line (just CRLF) ends the header block.
        if line == "\r\n" {
            break;
        }
        header.extend_from_slice(line.as_bytes());
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            content_length = rest.trim().parse::<usize>().ok();
        }
    }
    let len = content_length.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "LSP header missing Content-Length",
        )
    })?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    let msg: Message = serde_json::from_slice(&body).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid JSON body: {e}"),
        )
    })?;
    Ok(Some(msg))
}

/// Write one LSP message (header + body) to a writer and flush.
pub fn write_message(writer: &mut impl Write, msg: &Message) -> io::Result<()> {
    let body =
        serde_json::to_vec(msg).map_err(|e| io::Error::other(format!("serialize message: {e}")))?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Parse an incoming request's params into a typed struct, returning an
/// `INVALID_PARAMS` `RpcError` on failure.
#[allow(clippy::result_large_err)]
pub fn parse_params<T: serde::de::DeserializeOwned>(msg: &Message) -> Result<T, (Id, RpcError)> {
    let id = msg.id.clone().unwrap_or(Id::Int(0));
    let params = msg.params.clone().unwrap_or(serde_json::Value::Null);
    serde_json::from_value::<T>(params).map_err(|e| {
        (
            id,
            RpcError {
                code: error_code::INVALID_PARAMS,
                message: format!("invalid params: {e}"),
                data: None,
            },
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trips_a_request() {
        let req = Message::request(
            Id::Int(1),
            "textDocument/completion",
            serde_json::json!({"foo": "bar"}),
        );
        let mut buf = Vec::new();
        write_message(&mut buf, &req).unwrap();
        let mut reader = Cursor::new(buf);
        let got = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(got.id, Some(Id::Int(1)));
        assert_eq!(got.method.as_deref(), Some("textDocument/completion"));
        assert_eq!(got.params, Some(serde_json::json!({"foo": "bar"})));
    }

    #[test]
    fn round_trips_a_response() {
        let resp = Message::response(Id::Int(2), serde_json::json!([1, 2, 3]));
        let mut buf = Vec::new();
        write_message(&mut buf, &resp).unwrap();
        let mut reader = Cursor::new(buf);
        let got = read_message(&mut reader).unwrap().unwrap();
        assert!(got.is_response());
        assert_eq!(got.result, Some(serde_json::json!([1, 2, 3])));
    }

    #[test]
    fn read_returns_none_on_clean_eof() {
        let mut reader = Cursor::new(Vec::<u8>::new());
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn read_two_messages_back_to_back() {
        let mut buf = Vec::new();
        write_message(
            &mut buf,
            &Message::request(Id::Int(1), "a", serde_json::json!({})),
        )
        .unwrap();
        write_message(
            &mut buf,
            &Message::request(Id::Int(2), "b", serde_json::json!({})),
        )
        .unwrap();
        let mut reader = Cursor::new(buf);
        let m1 = read_message(&mut reader).unwrap().unwrap();
        let m2 = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(m1.id, Some(Id::Int(1)));
        assert_eq!(m2.id, Some(Id::Int(2)));
    }

    #[test]
    fn notification_has_no_id() {
        let n = Message::notification("$/cancelRequest", serde_json::json!({}));
        assert!(n.is_notification());
        assert!(!n.is_request());
    }
}
