use sqlly_core::ObjectKind;
use thiserror::Error;

pub type ScriptingResult<T> = Result<T, ScriptingError>;

#[derive(Debug, Error)]
pub enum ScriptingError {
    #[error("Object type not supported for scripting: {0:?}")]
    UnsupportedType(ObjectKind),

    #[error("Script generation failed: {0}")]
    GenerationFailed(String),

    #[error("Template not found: {0}")]
    TemplateNotFound(String),

    #[error("Missing template parameter: {0}")]
    MissingParameter(String),

    #[error("{0}")]
    Other(String),
}

impl ScriptingError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self {
            Self::UnsupportedType(kind) => DisplayMessage::new(
                "services.scripting.errors.unsupported_type",
                format!("Object type not supported for scripting: {kind:?}"),
            )
            .with_arg("object_type", format!("{kind:?}")),
            Self::GenerationFailed(detail) => DisplayMessage::new(
                "services.scripting.errors.generation_failed",
                format!("Script generation failed: {detail}"),
            )
            .with_arg("detail", detail),
            Self::TemplateNotFound(name) => DisplayMessage::new(
                "services.scripting.errors.template_not_found",
                format!("Template not found: {name}"),
            )
            .with_arg("template_name", name),
            Self::MissingParameter(name) => DisplayMessage::new(
                "services.scripting.errors.missing_parameter",
                format!("Missing template parameter: {name}"),
            )
            .with_arg("parameter_name", name),
            Self::Other(detail) => DisplayMessage::raw(detail),
        }
    }
}
