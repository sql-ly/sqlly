use thiserror::Error;

pub type MetadataResult<T> = Result<T, MetadataError>;

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("Node not found: {0}")]
    NodeNotFound(String),

    #[error("Permission denied for object: {0}")]
    PermissionDenied(String),

    #[error("Query failed: {0}")]
    QueryFailed(String),

    #[error("{0}")]
    Other(String),
}

impl MetadataError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self {
            Self::NodeNotFound(node) => {
                DisplayMessage::new("services.metadata.errors.node_not_found", self.to_string())
                    .with_arg("node_name", node)
            }
            Self::PermissionDenied(object) => DisplayMessage::new(
                "services.metadata.errors.permission_denied",
                self.to_string(),
            )
            .with_arg("object_name", object),
            Self::QueryFailed(detail) => {
                DisplayMessage::new("services.metadata.errors.query_failed", self.to_string())
                    .with_arg("detail", detail)
            }
            Self::Other(detail) => DisplayMessage::raw(detail),
        }
    }
}
