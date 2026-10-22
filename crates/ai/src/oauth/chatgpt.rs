//! OpenAI's "Sign in with ChatGPT" for a local tool: the authorize URL, the
//! token exchange, the refresh, the revoke and the account signing keys.
//!
//! No reply body ever reaches an error message, a log or a `Debug`: it carries
//! the account tokens. A failure names the HTTP status and, when the body holds
//! one, the `error` code alone.

use std::borrow::Cow;
use std::fmt;

use reagent_config::Secret;
use serde::Deserialize;

use super::jwt::Jwks;
use super::percent_encode;

/// The page the user signs in and agrees on.
pub const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";

/// The endpoint that trades the code for the tokens.
pub const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";

/// The endpoint that drops an account's tokens.
pub const REVOKE: &str = "https://auth.openai.com/api/accounts/oauth/revoke";

/// The account signing keys the ID token is checked against.
pub const JWKS: &str = "https://auth.openai.com/.well-known/jwks.json";

/// The issuer every ID token must name.
pub const ISSUER: &str = "https://auth.openai.com";

/// The API the issued token is allowed to call.
pub const RESOURCE: &str = "https://api.openai.com/v1";

/// The scopes a login asks for, space separated.
pub const SCOPE: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

/// The scope a ChatGPT plan must grant for a request to be allowed.
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";

/// The client id a first login asks for; the server then issues the saved one.
pub const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";

/// The local port the callback listens on.
pub const PORT: u16 = 1455;

/// The parameters of one authorize URL.
pub struct AuthorizeParams<'a> {
    /// The client id: the dynamic one on a first login, the issued one later.
    pub client_id: &'a str,
    /// The local callback URL the browser is sent back to.
    pub redirect_uri: &'a str,
    /// The fresh value the callback must echo.
    pub state: &'a str,
    /// The fresh value the ID token must carry.
    pub nonce: &'a str,
    /// The PKCE S256 challenge of the verifier.
    pub challenge: &'a str,
    /// The agent host id, saved with the first login.
    pub host_id: &'a str,
    /// Whether this is a first login, which asks for an agent name.
    pub first: bool,
    /// The saved ID token of a later login, when there is one.
    pub id_token_hint: Option<&'a str>,
}

impl fmt::Debug for AuthorizeParams<'_> {
    /// Prints the parameters with the one-time values hidden: the state and
    /// the nonce guard the callback, and the hint is a signed identity.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorizeParams")
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .field("state", &"***")
            .field("nonce", &"***")
            .field("challenge", &self.challenge)
            .field("host_id", &self.host_id)
            .field("first", &self.first)
            .field("id_token_hint", &self.id_token_hint.as_ref().map(|_| "***"))
            .finish()
    }
}

/// The URL to send the user to, with every login parameter.
pub fn authorize_url(params: &AuthorizeParams<'_>) -> String {
    let mut url = format!(
        "{AUTHORIZE}?response_type=code&client_id={}&redirect_uri={}&scope={}&resource={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256&ext_agent_host_id={}",
        percent_encode(params.client_id),
        percent_encode(params.redirect_uri),
        percent_encode(SCOPE),
        percent_encode(RESOURCE),
        percent_encode(params.state),
        percent_encode(params.nonce),
        percent_encode(params.challenge),
        percent_encode(params.host_id),
    );
    if params.first {
        // The name the account lists this tool under, on the first login only.
        url.push_str("&agent_name_hint=reagent");
    } else if let Some(id_token) = params.id_token_hint {
        url.push_str("&id_token_hint=");
        url.push_str(&percent_encode(id_token));
    }
    url
}

/// The tokens one exchange or refresh returned.
///
/// `Debug` prints a placeholder for each token, so a logged or panicked reply
/// never shows the account credentials.
pub struct TokenReply {
    /// The bearer of a provider request.
    pub access_token: Secret,
    /// The token that buys the next access token; it rotates on each refresh.
    pub refresh_token: Secret,
    /// The signed identity of the account, when the reply carried one.
    pub id_token: Option<Secret>,
    /// The token type, normally `Bearer`.
    pub token_type: Option<String>,
    /// How many seconds the access token lasts.
    pub expires_in: u64,
    /// The granted scopes, space separated.
    pub scope: Option<String>,
}

impl fmt::Debug for TokenReply {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenReply")
            .field("access_token", &"***")
            .field("refresh_token", &"***")
            .field("id_token", &self.id_token.as_ref().map(|_| "***"))
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .finish()
    }
}

/// Why a refresh failed.
#[derive(Debug)]
pub enum RefreshError {
    /// The saved login is no longer usable; the user must log in again.
    Expired,
    /// Anything else, with a message that holds no token.
    Other(String),
}

/// The reply codes that mean the saved login is gone.
const EXPIRED: [&str; 6] = [
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

/// The reply body, borrowed field by field; a value JSON writes with an
/// escape is a copy instead of a refused reply.
#[derive(Deserialize)]
struct Reply<'a> {
    #[serde(borrow)]
    access_token: Option<Cow<'a, str>>,
    #[serde(borrow)]
    refresh_token: Option<Cow<'a, str>>,
    #[serde(borrow)]
    id_token: Option<Cow<'a, str>>,
    #[serde(borrow)]
    token_type: Option<Cow<'a, str>>,
    expires_in: Option<u64>,
    #[serde(borrow)]
    scope: Option<Cow<'a, str>>,
}

/// One token reply parsed, or the field it lacks.
fn parse_reply(text: &str) -> Result<TokenReply, &'static str> {
    let raw: Reply<'_> = serde_json::from_str(text).map_err(|_| "the reply is not JSON")?;
    let access = raw
        .access_token
        .filter(|token| !token.is_empty())
        .ok_or("the reply has no access token")?;
    let refresh = raw
        .refresh_token
        .filter(|token| !token.is_empty())
        .ok_or("the reply has no refresh token")?;
    let expires_in = raw.expires_in.ok_or("the reply has no expiry")?;
    Ok(TokenReply {
        access_token: Secret::from(access.as_ref()),
        refresh_token: Secret::from(refresh.as_ref()),
        id_token: raw
            .id_token
            .filter(|token| !token.is_empty())
            .map(|token| Secret::from(token.as_ref())),
        token_type: raw.token_type.map(Cow::into_owned),
        expires_in,
        scope: raw.scope.map(Cow::into_owned),
    })
}

/// The `error` code of a JSON error body, which is the only part ever shown.
fn error_code(text: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Raw {
        error: Option<Error>,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Error {
        /// `{"error":"invalid_grant"}`.
        Text(String),
        /// `{"error":{"code":"invalid_grant"}}`.
        Object { code: String },
    }
    let raw: Raw = serde_json::from_str(text).ok()?;
    match raw.error? {
        Error::Text(text) => Some(text),
        Error::Object { code } => Some(code),
    }
}

/// Posts `form` and reads the reply body, or names the status and error code.
async fn form_post(url: &str, form: String, failed: &str) -> Result<String, String> {
    let response = crate::http::client()
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(bytes::Bytes::from(form))
        .send()
        .await
        .map_err(|error| format!("{failed}: {error}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(match error_code(&text) {
            Some(code) => format!("{failed}: HTTP {} ({code})", status.as_u16()),
            None => format!("{failed}: HTTP {}", status.as_u16()),
        });
    }
    Ok(text)
}

/// Trades `code` and `verifier` for the account tokens at `token_url`.
pub async fn exchange(
    token_url: &str,
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenReply, String> {
    let form = format!(
        "grant_type=authorization_code&client_id={}&code={}&code_verifier={}&redirect_uri={}&resource={}",
        percent_encode(client_id),
        percent_encode(code),
        percent_encode(verifier),
        percent_encode(redirect_uri),
        percent_encode(RESOURCE),
    );
    let text = form_post(token_url, form, "chatgpt login failed").await?;
    parse_reply(&text).map_err(|error| format!("chatgpt login failed: {error}"))
}

/// Trades `refresh_token` for a new access token at `token_url`.
pub async fn refresh(
    token_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenReply, RefreshError> {
    let form = format!(
        "grant_type=refresh_token&client_id={}&refresh_token={}&resource={}",
        percent_encode(client_id),
        percent_encode(refresh_token),
        percent_encode(RESOURCE),
    );
    let response = crate::http::client()
        .post(token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(bytes::Bytes::from(form))
        .send()
        .await
        .map_err(|error| RefreshError::Other(format!("chatgpt refresh failed: {error}")))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(match error_code(&text) {
            Some(code) if EXPIRED.contains(&code.as_str()) => RefreshError::Expired,
            Some(code) => RefreshError::Other(format!(
                "chatgpt refresh failed: HTTP {} ({code})",
                status.as_u16()
            )),
            None => {
                RefreshError::Other(format!("chatgpt refresh failed: HTTP {}", status.as_u16()))
            }
        });
    }
    parse_reply(&text)
        .map_err(|error| RefreshError::Other(format!("chatgpt refresh failed: {error}")))
}

/// Drops the account's tokens at `revoke_url`. An empty 200 is done.
pub async fn revoke(revoke_url: &str, client_id: &str, refresh_token: &str) -> Result<(), String> {
    let form = format!(
        "token={}&token_type_hint=refresh_token&client_id={}",
        percent_encode(refresh_token),
        percent_encode(client_id),
    );
    form_post(revoke_url, form, "chatgpt logout failed")
        .await
        .map(|_| ())
}

/// The account signing keys at `url`.
pub async fn jwks(url: &str) -> Result<Jwks, String> {
    let response = crate::http::client()
        .get(url)
        .send()
        .await
        .map_err(|error| format!("chatgpt login failed: {error}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "chatgpt login failed: the signing keys returned HTTP {}",
            status.as_u16()
        ));
    }
    Jwks::parse(&text).map_err(|error| format!("chatgpt login failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::test_server;

    /// The token reply with one access, refresh and ID token.
    const REPLY: &str = r#"{"access_token":"access-1","refresh_token":"refresh-1","id_token":"id-1","token_type":"Bearer","expires_in":3600,"scope":"openid chatgpt.tokens.use.direct"}"#;

    /// A fixed expiry, two in the afternoon of a fixed day.
    const EXPIRY: u64 = 1_800_000_000;

    /// The escapes of the form body or the query of `text`, as written.
    fn pairs(text: &str) -> Vec<(String, String)> {
        let query = text.split_once('?').map(|(_, query)| query).unwrap_or(text);
        query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                (name.to_string(), value.to_string())
            })
            .collect()
    }

    /// Checks that `name` is `wanted` once its escapes are read, so both the
    /// value and its encoding are checked.
    fn encoded(text: &str, name: &str, wanted: &str) {
        let got = pairs(text)
            .into_iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value);
        assert_eq!(
            got.as_deref(),
            Some(percent_encode(wanted).as_str()),
            "{name}"
        );
    }

    #[test]
    fn oauth_chatgpt_authorize_url_encodes_every_parameter() {
        let host = "urn:uuid:00000000-0000-4000-8000-000000000000";
        let url = authorize_url(&AuthorizeParams {
            client_id: DYNAMIC_CLIENT_ID,
            redirect_uri: "http://127.0.0.1:1455/auth/callback",
            state: "state 1",
            nonce: "nonce/1",
            challenge: "chal-1",
            host_id: host,
            first: true,
            id_token_hint: None,
        });
        assert!(url.starts_with(AUTHORIZE), "the authorize endpoint");
        for (name, wanted) in [
            ("response_type", "code"),
            ("client_id", DYNAMIC_CLIENT_ID),
            ("redirect_uri", "http://127.0.0.1:1455/auth/callback"),
            ("scope", SCOPE),
            ("resource", RESOURCE),
            ("state", "state 1"),
            ("nonce", "nonce/1"),
            ("code_challenge", "chal-1"),
            ("code_challenge_method", "S256"),
            ("ext_agent_host_id", host),
            ("agent_name_hint", "reagent"),
        ] {
            encoded(&url, name, wanted);
        }
        // A separator that is not escaped would split the value in two.
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback"));
        assert!(url.contains("state=state%201"), "the state is escaped");
        assert!(url.contains("nonce=nonce%2F1"), "the nonce is escaped");
        assert!(
            url.contains("ext_agent_host_id=urn%3Auuid%3A"),
            "the host is escaped"
        );
        assert!(
            !url.contains("id_token_hint"),
            "a first login sends no hint"
        );
    }

    #[test]
    fn oauth_chatgpt_a_later_authorize_url_names_the_saved_identity() {
        let hint = "head.body.signature";
        let url = authorize_url(&AuthorizeParams {
            client_id: "oaiapp_test",
            redirect_uri: "http://127.0.0.1:1455/auth/callback",
            state: "state-1",
            nonce: "nonce-1",
            challenge: "chal-1",
            host_id: "urn:uuid:00000000-0000-4000-8000-000000000000",
            first: false,
            id_token_hint: Some(hint),
        });
        encoded(&url, "client_id", "oaiapp_test");
        encoded(&url, "id_token_hint", hint);
        assert!(
            url.contains("id_token_hint=head.body.signature"),
            "the hint"
        );
        assert!(
            !url.contains("agent_name_hint"),
            "only a first login names the agent"
        );
    }

    #[tokio::test]
    async fn oauth_chatgpt_exchange_posts_the_form_and_reads_the_tokens() {
        let (base, recorded) = test_server::serve(200, REPLY).await;
        let token_url = format!("{base}/api/accounts/oauth/token");
        let reply = exchange(
            &token_url,
            "oaiapp_test",
            "the-code",
            "the-verifier",
            "http://127.0.0.1:1455/auth/callback",
        )
        .await
        .expect("the exchange succeeds");
        assert_eq!(reply.access_token.expose(), "access-1");
        assert_eq!(reply.refresh_token.expose(), "refresh-1");
        assert_eq!(reply.id_token.as_ref().map(Secret::expose), Some("id-1"));
        assert_eq!(reply.token_type.as_deref(), Some("Bearer"));
        assert_eq!(reply.expires_in, 3600);
        assert_eq!(
            reply.scope.as_deref(),
            Some("openid chatgpt.tokens.use.direct")
        );
        let request = recorded.await.expect("the server recorded the request");
        assert!(
            request
                .request_line
                .starts_with("POST /api/accounts/oauth/token HTTP/1.1"),
            "{}",
            request.request_line
        );
        for (name, wanted) in [
            ("grant_type", "authorization_code"),
            ("client_id", "oaiapp_test"),
            ("code", "the-code"),
            ("code_verifier", "the-verifier"),
            ("redirect_uri", "http://127.0.0.1:1455/auth/callback"),
            ("resource", RESOURCE),
        ] {
            encoded(&request.body, name, wanted);
        }
        // The reply's tokens never reach a Debug.
        let shown = format!("{reply:?}");
        assert!(!shown.contains("access-1"), "the access token is shown");
        assert!(!shown.contains("refresh-1"), "the refresh token is shown");
        assert!(!shown.contains("id-1"), "the ID token is shown");
    }

    #[tokio::test]
    async fn oauth_chatgpt_exchange_names_the_status_and_code_without_the_body() {
        let (base, _recorded) = test_server::serve(
            400,
            r#"{"error":{"code":"invalid_request"},"access_token":"secret-access"}"#,
        )
        .await;
        let error = exchange(&format!("{base}/token"), "oaiapp_test", "c", "v", "u")
            .await
            .unwrap_err();
        assert_eq!(error, "chatgpt login failed: HTTP 400 (invalid_request)");
        assert!(!error.contains("secret-access"), "the body is shown");
    }

    #[tokio::test]
    async fn oauth_chatgpt_exchange_refuses_a_reply_without_a_refresh_token() {
        let (base, _recorded) =
            test_server::serve(200, r#"{"access_token":"access-1","expires_in":3600}"#).await;
        let error = exchange(&format!("{base}/token"), "oaiapp_test", "c", "v", "u")
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "chatgpt login failed: the reply has no refresh token"
        );
        assert!(!error.contains("access-1"), "the token is shown");
    }

    #[tokio::test]
    async fn oauth_chatgpt_refresh_posts_the_form_and_reads_the_rotated_tokens() {
        let (base, recorded) = test_server::serve(200, REPLY).await;
        let reply = refresh(&format!("{base}/token"), "oaiapp_test", "refresh-1")
            .await
            .expect("the refresh succeeds");
        assert_eq!(reply.access_token.expose(), "access-1");
        assert_eq!(reply.refresh_token.expose(), "refresh-1");
        let request = recorded.await.expect("the server recorded the request");
        assert!(
            request.request_line.starts_with("POST /token HTTP/1.1"),
            "{}",
            request.request_line
        );
        for (name, wanted) in [
            ("grant_type", "refresh_token"),
            ("client_id", "oaiapp_test"),
            ("refresh_token", "refresh-1"),
            ("resource", RESOURCE),
        ] {
            encoded(&request.body, name, wanted);
        }
        assert!(
            !request.body.contains("scope"),
            "a refresh asks for no scope"
        );
    }

    #[tokio::test]
    async fn oauth_chatgpt_refresh_maps_a_dead_login_to_the_expired_error() {
        for body in [
            r#"{"error":"invalid_grant"}"#,
            r#"{"error":"invalid_refresh_token"}"#,
            r#"{"error":"token_expired"}"#,
            r#"{"error":"refresh_token_expired"}"#,
            r#"{"error":"refresh_token_invalidated"}"#,
            r#"{"error":"refresh_token_reused"}"#,
            r#"{"error":{"code":"refresh_token_reused"}}"#,
        ] {
            let (base, _recorded) = test_server::serve(400, body).await;
            let error = refresh(&format!("{base}/token"), "oaiapp_test", "refresh-1")
                .await
                .unwrap_err();
            assert!(
                matches!(error, RefreshError::Expired),
                "the refresh error is not the expired one"
            );
        }
    }

    #[tokio::test]
    async fn oauth_chatgpt_refresh_maps_another_error_to_other_without_the_body() {
        let (base, _recorded) = test_server::serve(
            503,
            r#"{"error":"server_error","refresh_token":"secret-refresh"}"#,
        )
        .await;
        match refresh(&format!("{base}/token"), "oaiapp_test", "refresh-1")
            .await
            .unwrap_err()
        {
            RefreshError::Other(message) => {
                assert_eq!(message, "chatgpt refresh failed: HTTP 503 (server_error)");
                assert!(!message.contains("secret-refresh"), "the body is shown");
            }
            RefreshError::Expired => panic!("a server error is no expired login"),
        }
        // A status with no error code shows the status alone.
        let (base, _recorded) =
            test_server::serve(500, r#"{"refresh_token":"secret-refresh"}"#).await;
        match refresh(&format!("{base}/token"), "oaiapp_test", "refresh-1")
            .await
            .unwrap_err()
        {
            RefreshError::Other(message) => {
                assert_eq!(message, "chatgpt refresh failed: HTTP 500");
                assert!(!message.contains("secret-refresh"), "the body is shown");
            }
            RefreshError::Expired => panic!("a broken reply is no expired login"),
        }
    }

    #[tokio::test]
    async fn oauth_chatgpt_revoke_posts_the_token_and_the_hint() {
        let (base, recorded) = test_server::serve(200, "").await;
        revoke(&format!("{base}/revoke"), "oaiapp_test", "refresh-1")
            .await
            .expect("the revoke succeeds");
        let request = recorded.await.expect("the server recorded the request");
        assert!(
            request.request_line.starts_with("POST /revoke HTTP/1.1"),
            "{}",
            request.request_line
        );
        assert_eq!(
            request.body,
            "token=refresh-1&token_type_hint=refresh_token&client_id=oaiapp_test"
        );
    }

    #[tokio::test]
    async fn oauth_chatgpt_revoke_reports_the_status_without_the_token() {
        let (base, _recorded) =
            test_server::serve(500, r#"{"token":"secret-refresh","error":"server_error"}"#).await;
        let error = revoke(&format!("{base}/revoke"), "oaiapp_test", "refresh-1")
            .await
            .unwrap_err();
        assert_eq!(error, "chatgpt logout failed: HTTP 500 (server_error)");
        assert!(!error.contains("secret-refresh"), "the body is shown");
    }

    #[tokio::test]
    async fn oauth_chatgpt_escaped_reply_is_read() {
        // A body JSON writes with `\/`: a borrowed `&str` field refuses it.
        let (base, _recorded) = test_server::serve(
            200,
            r#"{"access_token":"access\/1","refresh_token":"refresh\/1","token_type":"Bear\/er","expires_in":3600,"scope":"openid\/x"}"#,
        )
        .await;
        let reply = refresh(&format!("{base}/token"), "oaiapp_test", "refresh-1")
            .await
            .expect("the escaped reply is read");
        assert_eq!(reply.access_token.expose(), "access/1");
        assert_eq!(reply.refresh_token.expose(), "refresh/1");
        assert_eq!(reply.token_type.as_deref(), Some("Bear/er"));
        assert_eq!(reply.scope.as_deref(), Some("openid/x"));
        let shown = format!("{reply:?}");
        assert!(!shown.contains("access/1"), "the access token is shown");
    }

    #[tokio::test]
    async fn oauth_chatgpt_jwks_reads_the_signing_keys() {
        let (base, _recorded) = test_server::serve(200, super::super::jwt::testing::JWKS).await;
        let keys = jwks(&format!("{base}/.well-known/jwks.json"))
            .await
            .expect("the keys parse");
        // The keys really sign: the test key's token verifies against them.
        let claims = format!(
            "{{\"iss\":\"{ISSUER}\",\"aud\":\"oaiapp_test\",\"nonce\":\"n\",\"exp\":{},\"sub\":\"user-1\"}}",
            EXPIRY
        );
        let token = super::super::jwt::testing::sign(&claims).expect("signs");
        let claims = super::super::jwt::verify_id_token(
            &token,
            &keys,
            ISSUER,
            "oaiapp_test",
            "n",
            EXPIRY - 60,
        )
        .expect("the keys verify the token");
        assert_eq!(claims.sub, "user-1");
    }

    #[tokio::test]
    async fn oauth_chatgpt_jwks_reports_a_status() {
        let (base, _recorded) = test_server::serve(404, "{}").await;
        let error = jwks(&format!("{base}/jwks.json")).await.unwrap_err();
        assert_eq!(
            error,
            "chatgpt login failed: the signing keys returned HTTP 404"
        );
    }
}
