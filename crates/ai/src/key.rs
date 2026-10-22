//! A provider credential that never shows in `Debug` output.

use std::fmt;
use std::sync::Arc;

use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};

use crate::oauth::session::Session;

/// How a provider request proves who sends it.
///
/// The header is built once and marked sensitive, so a request clones two
/// reference counts and never copies the key. `Debug` prints a placeholder, so
/// a provider that derives `Debug` cannot leak the key into logs or errors.
#[derive(Clone)]
pub enum Credential {
    /// One header that carries the key.
    Key {
        /// The header name.
        name: HeaderName,
        /// The header value, marked sensitive.
        value: HeaderValue,
    },
    /// A saved ChatGPT login, which refreshes itself when the token is old.
    Session(Arc<Session>),
}

impl Credential {
    /// `authorization: Bearer <key>`.
    pub fn bearer(key: &str) -> Result<Self, String> {
        let mut text = String::with_capacity("Bearer ".len() + key.len());
        text.push_str("Bearer ");
        text.push_str(key);
        Self::key(AUTHORIZATION, &text)
    }

    /// `x-api-key: <key>`.
    pub fn x_api_key(key: &str) -> Result<Self, String> {
        Self::key(HeaderName::from_static("x-api-key"), key)
    }

    fn key(name: HeaderName, text: &str) -> Result<Self, String> {
        let mut value = HeaderValue::from_str(text)
            .map_err(|_| format!("the API key is not a valid {name} header value"))?;
        value.set_sensitive(true);
        Ok(Self::Key { name, value })
    }

    /// The header to add to a request.
    ///
    /// A key hands back its header at once, without allocating. A session
    /// hands back the cached access token, or refreshes it first when it is
    /// near its end.
    #[inline]
    pub async fn header(&self) -> Result<(HeaderName, HeaderValue), String> {
        match self {
            Self::Key { name, value } => Ok((name.clone(), value.clone())),
            Self::Session(session) => session.header().await,
        }
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Credential(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_debug_hides_the_key() {
        let key = Credential::bearer("sk-secret").expect("valid");
        assert_eq!(format!("{key:?}"), "Credential(<redacted>)");
    }

    #[tokio::test]
    async fn credential_header_is_bearer_or_x_api_key() {
        let (name, value) = Credential::bearer("sk-1")
            .expect("valid")
            .header()
            .await
            .expect("a header");
        assert_eq!(name, "authorization");
        assert_eq!(value.as_bytes(), b"Bearer sk-1");
        let (name, value) = Credential::x_api_key("sk-2")
            .expect("valid")
            .header()
            .await
            .expect("a header");
        assert_eq!(name, "x-api-key");
        assert_eq!(value.as_bytes(), b"sk-2");
    }

    #[tokio::test]
    async fn credential_header_is_sensitive() {
        assert!(
            Credential::bearer("sk-1")
                .unwrap()
                .header()
                .await
                .unwrap()
                .1
                .is_sensitive()
        );
        assert!(
            Credential::x_api_key("sk-1")
                .unwrap()
                .header()
                .await
                .unwrap()
                .1
                .is_sensitive()
        );
    }

    #[test]
    fn credential_rejects_a_bad_key_without_echoing_it() {
        for error in [
            Credential::bearer("sk-bad\nkey").unwrap_err(),
            Credential::x_api_key("sk-bad\nkey").unwrap_err(),
        ] {
            assert!(!error.contains("sk-bad"), "{error}");
        }
    }

    #[test]
    fn credential_a_provider_debug_hides_the_key() {
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

    #[tokio::test]
    async fn credential_a_bad_key_is_an_error_on_the_first_stream() {
        use futures_util::StreamExt;

        use crate::Provider;

        let provider = crate::anthropic::Anthropic::new("sk-bad\nkey", "http://x");
        let request = crate::Request {
            model: "m",
            max_tokens: 1,
            system: None,
            tools: &[],
            messages: &[],
            reasoning: None,
            cache_key: None,
        };
        let events: Vec<_> = provider.stream(&request).collect().await;
        assert_eq!(events.len(), 1);
        let error = events[0].as_ref().unwrap_err().to_string();
        assert!(!error.contains("sk-bad"), "{error}");
    }
}
