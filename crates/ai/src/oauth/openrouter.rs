//! The OpenRouter login flow: an authorize URL and the key exchange.

use serde::Deserialize;

use super::percent_encode;

/// The page the user picks a login on.
pub const AUTHORIZE: &str = "https://openrouter.ai/auth";

/// The endpoint that turns a code into a key.
pub const KEYS: &str = "https://openrouter.ai/api/v1/auth/keys";

/// The URL to send the user to, with the callback and the challenge.
pub fn authorize_url(callback: &str, challenge: &str) -> String {
    format!(
        "{AUTHORIZE}?callback_url={}&code_challenge={challenge}&code_challenge_method=S256",
        percent_encode(callback)
    )
}

/// The exchange request body.
#[derive(serde::Serialize)]
struct Body<'a> {
    code: &'a str,
    code_verifier: &'a str,
    code_challenge_method: &'a str,
}

/// The exchange reply, of which only the key matters.
#[derive(Deserialize)]
struct Reply {
    key: Option<String>,
}

/// Trades `code` and `verifier` for the API key at `keys_url`.
///
/// The reply body never reaches an error message: it may echo the code.
pub async fn exchange(keys_url: &str, code: &str, verifier: &str) -> Result<String, String> {
    let pay = Body {
        code,
        code_verifier: verifier,
        code_challenge_method: "S256",
    };
    let bytes = serde_json::to_vec(&pay)
        .map_err(|_| "openrouter login failed: cannot build the request".to_string())?;
    let response = crate::http::client()
        .post(keys_url)
        .header("content-type", "application/json")
        .body(bytes::Bytes::from(bytes))
        .send()
        .await
        .map_err(|error| format!("openrouter login failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("openrouter login failed: HTTP {}", status.as_u16()));
    }
    let text = response
        .text()
        .await
        .map_err(|_| "openrouter login failed: no key in reply".to_string())?;
    let reply: Reply = serde_json::from_str(&text)
        .map_err(|_| "openrouter login failed: no key in reply".to_string())?;
    reply
        .key
        .filter(|key| !key.is_empty())
        .ok_or_else(|| "openrouter login failed: no key in reply".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::test_server;

    #[test]
    fn oauth_authorize_url_carries_the_callback_and_the_challenge() {
        let url = authorize_url("http://127.0.0.1:5000/oauth/callback/abc", "chal-1");
        assert!(url.starts_with(AUTHORIZE), "{url}");
        assert!(
            url.contains("callback_url=http%3A%2F%2F127.0.0.1%3A5000%2Foauth%2Fcallback%2Fabc"),
            "{url}"
        );
        assert!(url.contains("code_challenge=chal-1"), "{url}");
        assert!(url.contains("code_challenge_method=S256"), "{url}");
    }

    #[tokio::test]
    async fn oauth_exchange_posts_the_code_and_verifier_and_reads_the_key() {
        let (base, recorded) = test_server::serve(200, r#"{"key":"sk-or-test"}"#).await;
        let key = exchange(
            &format!("{base}/api/v1/auth/keys"),
            "the-code",
            "the-verifier",
        )
        .await
        .expect("the exchange succeeds");
        assert_eq!(key, "sk-or-test");
        let request = recorded.await.expect("the server recorded the request");
        assert!(
            request.request_line.starts_with("POST /api/v1/auth/keys"),
            "{request:?}"
        );
        assert!(request.body.contains(r#""code":"the-code""#), "{request:?}");
        assert!(
            request.body.contains(r#""code_verifier":"the-verifier""#),
            "{request:?}"
        );
        assert!(request.body.contains("S256"), "{request:?}");
    }

    #[tokio::test]
    async fn oauth_exchange_reports_a_status_without_the_code() {
        let (base, _recorded) = test_server::serve(401, r#"{"code":"the-code"}"#).await;
        let error = exchange(&format!("{base}/api/v1/auth/keys"), "the-code", "v")
            .await
            .unwrap_err();
        assert_eq!(error, "openrouter login failed: HTTP 401");
        assert!(!error.contains("the-code"), "{error}");
    }

    #[tokio::test]
    async fn oauth_exchange_refuses_a_reply_without_a_key() {
        let (base, _recorded) = test_server::serve(200, r#"{"key":""}"#).await;
        let error = exchange(&format!("{base}/api/v1/auth/keys"), "c", "v")
            .await
            .unwrap_err();
        assert_eq!(error, "openrouter login failed: no key in reply");
    }
}
