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

    /// A pasted or opened plan document could not be read. Carries what the
    /// document looks like, where reading stopped, and how to capture a
    /// plan that can be read.
    #[error("{0}")]
    Import(Box<PlanImportError>),
}

/// Why [`import_plan_document`](crate::plans::import::import_plan_document)
/// could not read a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanImportError {
    /// Id of the format the document looks like (a
    /// [`PlanFormat::id`](crate::plans::import::PlanFormat::id)), when one
    /// was recognized.
    pub format_id: Option<&'static str>,
    /// What the document looks like, in words: a plan format's label, or
    /// the name of something that is not a plan (`SQLite bytecode`).
    pub format_label: Option<String>,
    /// 1-based line of the cleaned document where reading stopped.
    pub line: Option<usize>,
    /// 1-based column on that line.
    pub column: Option<usize>,
    /// What went wrong, as a clause that can follow "but" (`it holds no
    /// operators`).
    pub detail: String,
    /// How to capture a plan the importer reads.
    pub hint: Option<String>,
}

impl std::fmt::Display for PlanImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let detail = self.detail.trim().trim_end_matches('.');
        match &self.format_label {
            Some(label) => write!(f, "This looks like {label}, but {detail}")?,
            None => {
                let mut chars = detail.chars();
                match chars.next() {
                    Some(first) => write!(f, "{}{}", first.to_uppercase(), chars.as_str())?,
                    None => write!(f, "This plan could not be read")?,
                }
            }
        }
        match (self.line, self.column) {
            (Some(line), Some(column)) => write!(f, " (line {line}, column {column})")?,
            (Some(line), None) => write!(f, " (line {line})")?,
            _ => {}
        }
        write!(f, ".")?;
        if let Some(hint) = &self.hint {
            write!(f, " {hint}")?;
        }
        Ok(())
    }
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
            Self::Import(error) => DisplayMessage::raw(error.to_string()),
        }
    }
}
