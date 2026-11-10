use thiserror::Error;

pub type StudioResult<T> = Result<T, StudioError>;

#[derive(Debug, Error)]
pub enum StudioError {
    #[error("Connection error: {0}")]
    Connection(String),

    #[error("{0}")]
    ConnectionMessage(crate::DisplayMessage),

    #[error("Authentication failed: {0}")]
    Authentication(String),

    #[error("{0}")]
    AuthenticationMessage(crate::DisplayMessage),

    #[error("Query execution error: {0}")]
    QueryExecution(String),

    #[error("{0}")]
    QueryExecutionMessage(crate::DisplayMessage),

    #[error("Query timeout after {0:?}")]
    QueryTimeout(std::time::Duration),

    #[error("Query cancelled")]
    QueryCancelled,

    #[error("Object not found: {0}")]
    ObjectNotFound(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    /// A keyed permission refusal whose message is the whole sentence
    /// (including the "Permission denied" framing).
    #[error("{0}")]
    PermissionDeniedMessage(crate::DisplayMessage),

    #[error("Server capability not supported: {0}")]
    CapabilityNotSupported(String),

    #[error("Server capability not supported: {0}")]
    CapabilityNotSupportedMessage(crate::DisplayMessage),

    #[error("Invalid connection profile: {0}")]
    InvalidProfile(String),

    #[error("{0}")]
    InvalidProfileMessage(crate::DisplayMessage),

    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("Invalid input: {0}")]
    InvalidInputMessage(crate::DisplayMessage),

    #[error("Unsupported capability: {0}")]
    NotImplemented(&'static str),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),

    #[error("{0}")]
    Message(crate::DisplayMessage),
}

impl StudioError {
    /// Presentation copy for a UI render boundary. `Display` remains the stable
    /// English diagnostic used by logs and the existing wire protocol.
    pub fn message(&self) -> crate::DisplayMessage {
        use crate::DisplayMessage;
        match self {
            Self::Connection(detail) => DisplayMessage::new(
                "services.errors.connection",
                format!("Connection error: {detail}"),
            )
            .with_arg("detail", detail),
            Self::ConnectionMessage(message) => message.clone(),
            Self::Authentication(detail) => DisplayMessage::new(
                "services.errors.authentication",
                format!("Authentication failed: {detail}"),
            )
            .with_arg("detail", detail),
            Self::AuthenticationMessage(message) => message.clone(),
            Self::QueryExecution(detail) => DisplayMessage::new(
                "services.errors.query_execution",
                format!("Query execution error: {detail}"),
            )
            .with_arg("detail", detail),
            Self::QueryExecutionMessage(message) => message.clone(),
            Self::QueryTimeout(duration) => DisplayMessage::new(
                "services.errors.query_timeout",
                format!("Query timeout after {duration:?}"),
            )
            .with_arg("duration", format!("{duration:?}")),
            Self::QueryCancelled => {
                DisplayMessage::new("services.errors.query_cancelled", "Query cancelled")
            }
            Self::ObjectNotFound(object) => DisplayMessage::new(
                "services.errors.object_not_found",
                format!("Object not found: {object}"),
            )
            .with_arg("object_name", object),
            Self::PermissionDenied(detail) => DisplayMessage::new(
                "services.errors.permission_denied",
                format!("Permission denied: {detail}"),
            )
            .with_arg("detail", detail),
            Self::PermissionDeniedMessage(message) => message.clone(),
            Self::CapabilityNotSupported(detail) => DisplayMessage::new(
                "services.errors.capability_not_supported",
                format!("Server capability not supported: {detail}"),
            )
            .with_arg("detail", detail),
            Self::CapabilityNotSupportedMessage(message) => message.clone(),
            Self::InvalidProfile(detail) => DisplayMessage::new(
                "services.errors.invalid_profile",
                format!("Invalid connection profile: {detail}"),
            )
            .with_arg("detail", detail),
            Self::InvalidProfileMessage(message) => message.clone(),
            Self::InvalidInput(detail) => DisplayMessage::new(
                "services.errors.invalid_input",
                format!("Invalid input: {detail}"),
            )
            .with_arg("detail", detail),
            Self::InvalidInputMessage(message) => message.clone(),
            Self::NotImplemented(capability) => DisplayMessage::new(
                "services.errors.unsupported_capability",
                format!("Unsupported capability: {capability}"),
            )
            .with_arg("capability", *capability),
            Self::Serialization(detail) => DisplayMessage::new(
                "services.errors.serialization",
                format!("Serialization error: {detail}"),
            )
            .with_arg("detail", detail),
            Self::Io(error) => {
                DisplayMessage::new("services.errors.io", format!("IO error: {error}"))
                    .with_arg("detail", error.to_string())
            }
            Self::Other(detail) => DisplayMessage::raw(detail),
            Self::Message(message) => message.clone(),
        }
    }
}

impl StudioError {
    /// The error's text without the kind prefix `Display` puts in front of
    /// it (`Connection error: `, `Query execution error: `, …), for callers
    /// that re-frame the text — a diagnostic stage headline, a trace row, a
    /// wire frame whose receiver rebuilds the same kind. A keyed `*Message`
    /// variant's fallback is the whole sentence for some kinds, so a leading
    /// kind prefix is stripped from it too; the text is never re-prefixed
    /// twice. Kinds without a detail (timeout, cancel, I/O, …) return their
    /// full `Display`.
    pub fn detail(&self) -> String {
        fn strip(message: &crate::DisplayMessage, prefix: &str) -> String {
            let text = message.fallback();
            text.strip_prefix(prefix).unwrap_or(text).to_string()
        }
        match self {
            Self::Connection(detail)
            | Self::Authentication(detail)
            | Self::QueryExecution(detail)
            | Self::PermissionDenied(detail)
            | Self::CapabilityNotSupported(detail)
            | Self::InvalidProfile(detail)
            | Self::InvalidInput(detail)
            | Self::Other(detail) => detail.clone(),
            Self::ConnectionMessage(message) => strip(message, CONNECTION_PREFIX),
            Self::AuthenticationMessage(message) => strip(message, AUTHENTICATION_PREFIX),
            Self::QueryExecutionMessage(message) => strip(message, QUERY_EXECUTION_PREFIX),
            Self::PermissionDeniedMessage(message) => strip(message, PERMISSION_DENIED_PREFIX),
            Self::CapabilityNotSupportedMessage(message) => strip(message, CAPABILITY_PREFIX),
            Self::InvalidProfileMessage(message) => strip(message, INVALID_PROFILE_PREFIX),
            Self::InvalidInputMessage(message) => strip(message, INVALID_INPUT_PREFIX),
            Self::Message(message) => message.fallback().to_string(),
            other => other.to_string(),
        }
    }
}

const CONNECTION_PREFIX: &str = "Connection error: ";
const AUTHENTICATION_PREFIX: &str = "Authentication failed: ";
const QUERY_EXECUTION_PREFIX: &str = "Query execution error: ";
const PERMISSION_DENIED_PREFIX: &str = "Permission denied: ";
const CAPABILITY_PREFIX: &str = "Server capability not supported: ";
const INVALID_PROFILE_PREFIX: &str = "Invalid connection profile: ";
const INVALID_INPUT_PREFIX: &str = "Invalid input: ";

impl serde::Serialize for StudioError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DisplayMessage;

    #[test]
    fn capability_message_display_has_exactly_one_prefix() {
        let keyed = StudioError::CapabilityNotSupportedMessage(DisplayMessage::new(
            "services.query.errors.manual_commit_unavailable",
            "Manual commit mode is not available on this port",
        ));
        assert_eq!(
            keyed.to_string(),
            "Server capability not supported: Manual commit mode is not available on this port"
        );
        assert_eq!(
            keyed.to_string(),
            StudioError::CapabilityNotSupported(
                "Manual commit mode is not available on this port".into()
            )
            .to_string()
        );
        assert_eq!(
            keyed.detail(),
            "Manual commit mode is not available on this port"
        );
    }

    #[test]
    fn query_execution_message_display_keeps_the_kind_prefix() {
        let keyed = StudioError::QueryExecutionMessage(DisplayMessage::new(
            "services.tds.errors.spid_no_row",
            "Query execution error: spid probe returned no row",
        ));
        assert_eq!(
            keyed.to_string(),
            StudioError::QueryExecution("spid probe returned no row".into()).to_string()
        );
        assert_eq!(keyed.detail(), "spid probe returned no row");
    }

    #[test]
    fn detail_strips_the_kind_prefix_once_from_message_fallbacks() {
        let cases = [
            (
                StudioError::ConnectionMessage(DisplayMessage::new(
                    "services.redis.errors.handshakes_refused",
                    "Connection error: the server refused both RESP3 and RESP2 handshakes",
                )),
                "the server refused both RESP3 and RESP2 handshakes",
            ),
            (
                StudioError::ConnectionMessage(DisplayMessage::raw("no route to host")),
                "no route to host",
            ),
            (
                StudioError::AuthenticationMessage(DisplayMessage::raw(
                    "Authentication failed: bad password",
                )),
                "bad password",
            ),
            (
                StudioError::PermissionDeniedMessage(DisplayMessage::raw(
                    "Permission denied: read-only connection",
                )),
                "read-only connection",
            ),
            (
                StudioError::InvalidInputMessage(DisplayMessage::raw("empty key")),
                "empty key",
            ),
            (StudioError::Connection("refused".into()), "refused"),
            (StudioError::QueryCancelled, "Query cancelled"),
        ];
        for (error, expected) in cases {
            assert_eq!(error.detail(), expected, "{error:?}");
        }
    }
}
