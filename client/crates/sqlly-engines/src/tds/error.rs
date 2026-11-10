//! Error types plus the tiberius error mapping/formatting helpers used by the
//! execution engine.

use sqlly_core::StudioError;

/// Maps any tiberius error to a [`StudioError::QueryExecution`].
pub(crate) fn to_query_error(error: tiberius::error::Error) -> StudioError {
    StudioError::QueryExecution(error.to_string())
}

/// A server-returned SQL error (`Msg …` — syntax, constraint, permission). The
/// connection survives it, so the batch stream can be drained and later batches
/// can still run. Every other tiberius error is a transport/protocol failure
/// that leaves the client unusable.
pub(crate) fn is_recoverable_server_error(err: &tiberius::error::Error) -> bool {
    matches!(err, tiberius::error::Error::Server(_))
}

/// Formats a server error the way SSMS prints it into the Messages pane.
/// `line_shift` is added to the server-reported (batch-relative, 1-based) line
/// so the printed `Line` is relative to the whole script the app submitted.
pub(crate) fn format_server_error(err: &tiberius::error::Error, line_shift: i64) -> String {
    match err {
        tiberius::error::Error::Server(token) => format!(
            "Msg {}, Level {}, State {}, Line {}\n{}",
            token.code(),
            token.class(),
            token.state(),
            (i64::from(token.line()) + line_shift).max(0),
            token.message()
        ),
        other => other.to_string(),
    }
}

/// Lines to add to a server-reported (batch-relative, 1-based) line so it
/// becomes relative to the whole script the app submitted.
/// `script` is the full run text, `batch_start` the byte offset of the batch
/// inside it, `batch_text` the raw batch slice, and `policied` the text that
/// was actually sent (possibly wrapped by the rollback policy).
pub(crate) fn batch_line_shift(
    script: &str,
    batch_start: usize,
    batch_text: &str,
    policied: &str,
) -> i64 {
    let lines_before_batch = script[..batch_start.min(script.len())]
        .matches('\n')
        .count() as i64;
    let leading_ws = batch_text.len() - batch_text.trim_start().len();
    let trimmed_leading_lines = batch_text[..leading_ws].matches('\n').count() as i64;
    let trimmed = batch_text.trim();
    let preamble_lines = policied
        .find(trimmed)
        .map(|at| policied[..at].matches('\n').count() as i64)
        .unwrap_or(0);
    lines_before_batch + trimmed_leading_lines - preamble_lines
}

#[cfg(test)]
mod tests {
    use super::batch_line_shift;

    #[test]
    fn batch_line_shift_single_batch_no_wrap_is_zero() {
        let script = "SELECT 1";
        assert_eq!(batch_line_shift(script, 0, script, script), 0);
    }

    #[test]
    fn batch_line_shift_second_batch_after_go() {
        // Four newlines before the second batch (GO on line 4).
        let script = "SELECT 1\nGO\n\n\nSELECT 2";
        let start = script.find("SELECT 2").unwrap();
        assert_eq!(script[..start].matches('\n').count(), 4);
        assert_eq!(batch_line_shift(script, start, "SELECT 2", "SELECT 2"), 4);
    }

    #[test]
    fn batch_line_shift_rollback_wrap_subtracts_preamble() {
        let batch = "DELETE FROM t";
        let policied =
            "SET XACT_ABORT ON;\nBEGIN TRANSACTION;\nDELETE FROM t\nROLLBACK TRANSACTION;";
        assert_eq!(batch_line_shift(batch, 0, batch, policied), -2);
    }

    #[test]
    fn batch_line_shift_leading_blanks_and_wrap_cancel_out() {
        let batch = "\n\nDELETE FROM t";
        let policied =
            "SET XACT_ABORT ON;\nBEGIN TRANSACTION;\nDELETE FROM t\nROLLBACK TRANSACTION;";
        assert_eq!(batch_line_shift(batch, 0, batch, policied), 0);
    }
}

pub type TdsResult<T> = Result<T, TdsError>;

#[derive(Debug)]
pub enum TdsError {
    NotConnected,
    ConnectionFailed(String),
    AuthFailed(String),
    TlsError(String),
    QueryFailed(String),
    Timeout(String),
    Cancelled,
    NotImplemented(String),
    Protocol(String),
    Io(std::io::Error),
}

impl std::fmt::Display for TdsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TdsError::NotConnected => write!(f, "Not connected to server"),
            TdsError::ConnectionFailed(s) => write!(f, "Connection failed: {s}"),
            TdsError::AuthFailed(s) => write!(f, "Authentication failed: {s}"),
            TdsError::TlsError(s) => write!(f, "TLS error: {s}"),
            TdsError::QueryFailed(s) => write!(f, "Query execution failed: {s}"),
            TdsError::Timeout(s) => write!(f, "Timeout: {s}"),
            TdsError::Cancelled => write!(f, "Cancelled"),
            TdsError::NotImplemented(s) => write!(f, "Unsupported TDS capability: {s}"),
            TdsError::Protocol(s) => write!(f, "Protocol error: {s}"),
            TdsError::Io(e) => write!(f, "IO error: {e}"),
        }
    }
}

impl std::error::Error for TdsError {}

impl From<std::io::Error> for TdsError {
    fn from(e: std::io::Error) -> Self {
        TdsError::Io(e)
    }
}

impl From<TdsError> for StudioError {
    fn from(e: TdsError) -> Self {
        StudioError::Connection(e.to_string())
    }
}
