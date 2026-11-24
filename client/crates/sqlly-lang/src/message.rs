use std::fmt;

/// Translation-neutral presentation copy produced outside the UI.
///
/// A keyed message carries an English fallback for logging, protocol compatibility,
/// and non-localized frontends. The UI resolves `key` and `arguments` at its render
/// boundary instead of asking a worker thread for the active language. Raw messages
/// are reserved for SQL and for text supplied by users, servers, or other libraries.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DisplayMessage {
    key: Option<&'static str>,
    arguments: Vec<(String, String)>,
    /// Arguments that are messages themselves, translated before they are
    /// placed; `arguments` holds their English text under the same name.
    nested: Vec<(String, DisplayMessage)>,
    fallback: String,
}

impl DisplayMessage {
    pub fn new(key: &'static str, fallback: impl Into<String>) -> Self {
        Self {
            key: Some(key),
            arguments: Vec::new(),
            nested: Vec::new(),
            fallback: fallback.into(),
        }
    }

    pub fn raw(fallback: impl Into<String>) -> Self {
        Self {
            key: None,
            arguments: Vec::new(),
            nested: Vec::new(),
            fallback: fallback.into(),
        }
    }

    pub fn with_arg(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.arguments.push((name.into(), value.into()));
        self
    }

    /// An argument that is itself a message, so the UI translates it too
    /// instead of placing its English text inside a translated sentence.
    pub fn with_message_arg(mut self, name: impl Into<String>, message: DisplayMessage) -> Self {
        let name = name.into();
        self.arguments
            .push((name.clone(), message.fallback().to_string()));
        self.nested.push((name, message));
        self
    }

    pub fn key(&self) -> Option<&'static str> {
        self.key
    }

    pub fn fallback(&self) -> &str {
        &self.fallback
    }

    pub fn arguments(&self) -> impl Iterator<Item = (&str, &str)> {
        self.arguments
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    pub fn nested_arguments(&self) -> impl Iterator<Item = (&str, &DisplayMessage)> {
        self.nested
            .iter()
            .map(|(name, message)| (name.as_str(), message))
    }

    pub fn into_fallback(self) -> String {
        self.fallback
    }
}

impl fmt::Display for DisplayMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.fallback)
    }
}

impl std::ops::Deref for DisplayMessage {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.fallback()
    }
}

impl AsRef<str> for DisplayMessage {
    fn as_ref(&self) -> &str {
        self.fallback()
    }
}

impl PartialEq<&str> for DisplayMessage {
    fn eq(&self, other: &&str) -> bool {
        self.fallback() == *other
    }
}

impl PartialEq<DisplayMessage> for &str {
    fn eq(&self, other: &DisplayMessage) -> bool {
        *self == other.fallback()
    }
}

impl From<String> for DisplayMessage {
    fn from(value: String) -> Self {
        Self::raw(value)
    }
}

impl From<&str> for DisplayMessage {
    fn from(value: &str) -> Self {
        Self::raw(value)
    }
}

#[cfg(test)]
mod tests {
    use super::DisplayMessage;

    #[test]
    fn keyed_messages_expose_stable_key_arguments_and_fallback() {
        let message = DisplayMessage::new("services.example.count", "Read {count} rows")
            .with_arg("count", "12");
        assert_eq!(message.key(), Some("services.example.count"));
        assert_eq!(
            message.arguments().collect::<Vec<_>>(),
            vec![("count", "12")]
        );
        assert_eq!(message.to_string(), "Read {count} rows");
    }

    #[test]
    fn message_arguments_keep_the_inner_message_and_its_english() {
        let inner = DisplayMessage::new("services.example.inner", "too long").with_arg("max", "3");
        let message = DisplayMessage::new("services.example.outer", "a: too long")
            .with_arg("name", "a")
            .with_message_arg("error", inner.clone());
        assert_eq!(
            message.arguments().collect::<Vec<_>>(),
            vec![("name", "a"), ("error", "too long")]
        );
        assert_eq!(
            message.nested_arguments().collect::<Vec<_>>(),
            vec![("error", &inner)]
        );
    }

    #[test]
    fn raw_messages_never_claim_a_translation_key() {
        let message = DisplayMessage::raw("server supplied detail");
        assert_eq!(message.key(), None);
        assert_eq!(message.arguments().next(), None);
    }
}
