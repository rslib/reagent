//! An API key that never shows in `Debug` output.

use std::fmt;

/// Holds a provider API key. `Debug` prints a placeholder, so a provider
/// that derives `Debug` cannot leak the key into logs or errors.
#[derive(Clone)]
pub(crate) struct ApiKey(String);

impl ApiKey {
    pub(crate) fn new(key: String) -> Self {
        Self(key)
    }

    /// The key itself, for the request header only.
    pub(crate) fn reveal(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_the_key() {
        let key = ApiKey::new("sk-secret".into());
        assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
        assert_eq!(key.reveal(), "sk-secret");
    }

    #[test]
    fn a_provider_debug_hides_the_key() {
        let text = format!(
            "{:?}",
            crate::anthropic::Anthropic::new("sk-secret", "http://x")
        );
        assert!(!text.contains("sk-secret"), "{text}");
        let text = format!(
            "{:?}",
            crate::openai::responses::OpenAiResponses::new("sk-secret", "http://x")
        );
        assert!(!text.contains("sk-secret"), "{text}");
    }
}
