use thiserror::Error;

pub type AdminResult<T> = Result<T, AdminError>;

#[derive(Debug, Error)]
pub enum AdminError {
    #[error("Backup failed: {0}")]
    BackupFailed(String),

    #[error("Restore failed: {0}")]
    RestoreFailed(String),

    #[error("Operation not supported: {0}")]
    NotSupported(String),

    #[error("{0}")]
    Other(String),
}

impl AdminError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self {
            Self::BackupFailed(detail) => DisplayMessage::new(
                "services.admin.errors.backup_failed",
                format!("Backup failed: {detail}"),
            )
            .with_arg("detail", detail),
            Self::RestoreFailed(detail) => DisplayMessage::new(
                "services.admin.errors.restore_failed",
                format!("Restore failed: {detail}"),
            )
            .with_arg("detail", detail),
            Self::NotSupported(detail) => DisplayMessage::new(
                "services.admin.errors.not_supported",
                format!("Operation not supported: {detail}"),
            )
            .with_arg("detail", detail),
            Self::Other(detail) => DisplayMessage::raw(detail),
        }
    }
}
