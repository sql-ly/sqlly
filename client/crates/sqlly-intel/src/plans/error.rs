use thiserror::Error;

pub type PlanResult<T> = Result<T, PlanError>;

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("Empty execution plan")]
    EmptyPlan,

    #[error("XML parse error: {0}")]
    XmlParse(String),

    #[error("Invalid plan structure: {0}")]
    InvalidStructure(String),

    #[error("{0}")]
    Other(String),
}

impl PlanError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self {
            Self::EmptyPlan => {
                DisplayMessage::new("services.plans.errors.empty_plan", "Empty execution plan")
            }
            Self::XmlParse(detail) => DisplayMessage::new(
                "services.plans.errors.xml_parse",
                format!("XML parse error: {detail}"),
            )
            .with_arg("detail", detail),
            Self::InvalidStructure(detail) => DisplayMessage::new(
                "services.plans.errors.invalid_structure",
                format!("Invalid plan structure: {detail}"),
            )
            .with_arg("detail", detail),
            Self::Other(detail) => DisplayMessage::raw(detail),
        }
    }
}
