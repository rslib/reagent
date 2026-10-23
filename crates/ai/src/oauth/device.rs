//! The device-code login of RFC 8628: ask for a code, show it, poll the token
//! endpoint until the user approves, and refresh the tokens later.
//!
//! No reply body ever reaches an error message, a log or a `Debug`: it carries
//! the account tokens. A failure names the HTTP status and, when the body holds
//! one, the `error` code alone.

use std::borrow::Cow;
use std::fmt;
use std::time::Duration;

use reagent_config::Secret;
use serde::Deserialize;

use super::percent_encode;
use super::session::Sleeper;

/// The Radius gateway's endpoints and client.
pub mod radius {
    /// The endpoint that hands out a device code.
    pub const DEVICE: &str = "https://radius.pi.dev/v1/oauth/device";
    /// The endpoint polled for the tokens, and the refresh endpoint.
    pub const TOKEN: &str = "https://radius.pi.dev/v1/oauth/token";
    /// The client id pi and openclaw sign in with.
    pub const CLIENT_ID: &str = "pi-gateway";
    /// The scopes a login asks for, space separated.
    pub const SCOPE: &str = "gateway offline_access";
    /// The only origin a page the user is sent to may have.
    pub const PAGE_ORIGIN: &str = "https://radius.earendil.com";
}

/// The grant a device-code poll names.
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// The poll interval when the server names none, from RFC 8628.
const DEFAULT_INTERVAL: u64 = 5;

/// What a `slow_down` adds to the interval, from RFC 8628.
const SLOW_DOWN: u64 = 5;

/// The endpoints and the client of one provider's device login.
#[derive(Clone, Debug)]
pub struct Endpoints {
    /// The device authorization endpoint.
    pub device: String,
    /// The token endpoint.
    pub token: String,
    /// The client id.
    pub client_id: String,
    /// The scopes, space separated.
    pub scope: String,
    /// The origin the page the user opens must have.
    pub page_origin: String,
}

impl Endpoints {
    /// The Radius gateway's.
    pub fn radius() -> Self {
        Self {
            device: radius::DEVICE.into(),
            token: radius::TOKEN.into(),
            client_id: radius::CLIENT_ID.into(),
            scope: radius::SCOPE.into(),
            page_origin: radius::PAGE_ORIGIN.into(),
        }
    }
}

/// The code a user types and the page they type it on.
pub struct Code {
    /// The code to show.
    pub user_code: String,
    /// The page to open.
    pub page: String,
    device_code: Secret,
    expires_in: u64,
    interval: u64,
}

impl fmt::Debug for Code {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Code")
            .field("user_code", &self.user_code)
            .field("page", &self.page)
            .field("device_code", &"***")
            .finish_non_exhaustive()
    }
}

/// The tokens a login or a refresh returned.
///
/// `Debug` prints a placeholder for each token.
pub struct Tokens {
    /// The bearer of a provider request.
    pub access: Secret,
    /// The token that buys the next access token, when the server sent one.
    pub refresh: Option<Secret>,
    /// How many seconds the access token lasts.
    pub expires_in: u64,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Tokens")
            .field("access", &"***")
            .field("refresh", &self.refresh.as_ref().map(|_| "***"))
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// Why a refresh failed.
#[derive(Debug)]
pub enum RefreshError {
    /// The saved login is no longer usable; the user must log in again.
    Expired,
    /// Any other failure, which names the status and the error code only.
    Other(String),
}

#[derive(Deserialize)]
struct Reply<'a> {
    #[serde(borrow)]
    access_token: Option<Cow<'a, str>>,
    #[serde(borrow)]
    refresh_token: Option<Cow<'a, str>>,
    expires_in: Option<f64>,
    #[serde(borrow)]
    error: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct DeviceReply<'a> {
    #[serde(borrow)]
    device_code: Option<Cow<'a, str>>,
    #[serde(borrow)]
    user_code: Option<Cow<'a, str>>,
    #[serde(borrow)]
    verification_uri: Option<Cow<'a, str>>,
    #[serde(borrow)]
    verification_uri_complete: Option<Cow<'a, str>>,
    // Radius sends fractions, such as an interval of 0.1.
    expires_in: Option<f64>,
    interval: Option<f64>,
}

/// Whole seconds of a reply's `seconds`, rounded up, at least `floor`.
fn whole_seconds(seconds: f64, floor: u64) -> u64 {
    if seconds.is_finite() && seconds > 0.0 {
        (seconds.ceil() as u64).max(floor)
    } else {
        floor
    }
}

/// Posts `form` to `url` and returns the status and the body.
async fn post(url: &str, form: String, failed: &str) -> Result<(u16, String), String> {
    let response = crate::http::client()
        .post(url)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(bytes::Bytes::from(form))
        .send()
        .await
        .map_err(|error| format!("{failed}: {error}"))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    Ok((status, text))
}

/// The tokens of a reply, or the field it lacks.
fn tokens(reply: &Reply<'_>, previous: bool) -> Result<Tokens, &'static str> {
    let access = reply
        .access_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .ok_or("the reply has no access token")?;
    let refresh = reply
        .refresh_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .map(Secret::from);
    // A first login needs a refresh token; a refresh may keep the old one.
    if refresh.is_none() && !previous {
        return Err("the reply has no refresh token");
    }
    let expires_in = reply
        .expires_in
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(|seconds| whole_seconds(seconds, 1))
        .ok_or("the reply has no expiry")?;
    Ok(Tokens {
        access: Secret::from(access),
        refresh,
        expires_in,
    })
}

/// Whether `page` is a plain URL on `origin`, with no user or password, so a
/// rogue reply cannot send the user to another site.
fn on_origin(page: &str, origin: &str) -> bool {
    let Some(rest) = page.strip_prefix(origin) else {
        return false;
    };
    let authority_end = rest.is_empty() || rest.starts_with(['/', '?', '#']);
    // `origin` has no `@`, so one before the path means a user part.
    authority_end && !page[..page.len() - rest.len()].contains('@')
}

/// Asks for a device code, and checks the page it names.
pub async fn start(endpoints: &Endpoints) -> Result<Code, String> {
    const FAILED: &str = "the login could not start";
    let form = format!(
        "client_id={}&scope={}",
        percent_encode(&endpoints.client_id),
        percent_encode(&endpoints.scope),
    );
    let (status, text) = post(&endpoints.device, form, FAILED).await?;
    if !(200..300).contains(&status) {
        return Err(failure(FAILED, status, &text));
    }
    let reply: DeviceReply<'_> =
        serde_json::from_str(&text).map_err(|_| format!("{FAILED}: the reply is not JSON"))?;
    let (Some(device_code), Some(user_code), Some(page), Some(expires_in)) = (
        reply.device_code.filter(|code| !code.is_empty()),
        reply.user_code.filter(|code| !code.is_empty()),
        reply.verification_uri,
        reply.expires_in,
    ) else {
        return Err(format!("{FAILED}: the reply lacks a field"));
    };
    if !on_origin(&page, &endpoints.page_origin) {
        return Err(format!(
            "{FAILED}: the login page is not on {}",
            endpoints.page_origin
        ));
    }
    // The page with the code filled in saves typing it, when it is on the
    // same site.
    let page = reply
        .verification_uri_complete
        .filter(|complete| on_origin(complete, &endpoints.page_origin))
        .unwrap_or(page);
    Ok(Code {
        user_code: user_code.into_owned(),
        page: page.into_owned(),
        device_code: Secret::from(device_code.as_ref()),
        expires_in: whole_seconds(expires_in, 1),
        interval: reply
            .interval
            .map_or(DEFAULT_INTERVAL, |seconds| whole_seconds(seconds, 1)),
    })
}

/// Polls until the user approves `code`, waiting on `sleep` between polls.
pub async fn wait(endpoints: &Endpoints, code: &Code, sleep: &Sleeper) -> Result<Tokens, String> {
    const FAILED: &str = "the login failed";
    let form = format!(
        "client_id={}&grant_type={}&device_code={}",
        percent_encode(&endpoints.client_id),
        percent_encode(DEVICE_GRANT),
        percent_encode(code.device_code.expose()),
    );
    let mut interval = code.interval;
    let mut waited = 0;
    loop {
        if waited >= code.expires_in {
            return Err("the login code expired; run the login again".into());
        }
        sleep(Duration::from_secs(interval)).await;
        waited += interval;
        let (status, text) = post(&endpoints.token, form.clone(), FAILED).await?;
        let reply: Option<Reply<'_>> = serde_json::from_str(&text).ok();
        let error = reply.as_ref().and_then(|reply| reply.error.as_deref());
        match error {
            None if (200..300).contains(&status) => {
                let reply = reply.ok_or_else(|| format!("{FAILED}: the reply is not JSON"))?;
                return tokens(&reply, false).map_err(|error| format!("{FAILED}: {error}"));
            }
            Some("authorization_pending") => {}
            Some("slow_down") => interval += SLOW_DOWN,
            Some("access_denied") => return Err("the login was denied".into()),
            Some("expired_token") => {
                return Err("the login code expired; run the login again".into());
            }
            _ => return Err(failure(FAILED, status, &text)),
        }
    }
}

/// Trades `refresh` for a new access token. A reply without a refresh token
/// keeps the old one.
pub async fn refresh(
    token_url: &str,
    client_id: &str,
    refresh: &str,
) -> Result<Tokens, RefreshError> {
    const FAILED: &str = "the token refresh failed";
    let form = format!(
        "client_id={}&grant_type=refresh_token&refresh_token={}",
        percent_encode(client_id),
        percent_encode(refresh),
    );
    let (status, text) = post(token_url, form, FAILED)
        .await
        .map_err(RefreshError::Other)?;
    let reply: Option<Reply<'_>> = serde_json::from_str(&text).ok();
    if let Some(code) = reply.as_ref().and_then(|reply| reply.error.as_deref()) {
        return Err(match code {
            "invalid_grant" | "expired_token" | "access_denied" => RefreshError::Expired,
            _ => RefreshError::Other(failure(FAILED, status, &text)),
        });
    }
    if !(200..300).contains(&status) {
        return Err(RefreshError::Other(failure(FAILED, status, &text)));
    }
    let reply =
        reply.ok_or_else(|| RefreshError::Other(format!("{FAILED}: the reply is not JSON")))?;
    tokens(&reply, true).map_err(|error| RefreshError::Other(format!("{FAILED}: {error}")))
}

/// `failed` with the status and, when the body names one, the error code.
#[cold]
fn failure(failed: &str, status: u16, text: &str) -> String {
    let code = serde_json::from_str::<Reply<'_>>(text)
        .ok()
        .and_then(|reply| reply.error.map(Cow::into_owned));
    match code {
        Some(code) => format!("{failed}: HTTP {status} ({code})"),
        None => format!("{failed}: HTTP {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serves one `(status, body)` reply per request, in order, and records
    /// each request body.
    async fn serve(replies: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&bodies);
        tokio::spawn(async move {
            for (status, reply) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 2048];
                loop {
                    let read = socket.read(&mut chunk).await.unwrap_or(0);
                    buffer.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&buffer).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if buffer.len() >= end + 4 + length || read == 0 {
                            seen.lock().unwrap().push(text[end + 4..].to_string());
                            break;
                        }
                    }
                    if read == 0 {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{address}"), bodies)
    }

    fn local(base: &str) -> Endpoints {
        Endpoints {
            device: format!("{base}/device"),
            token: format!("{base}/token"),
            client_id: "pi-gateway".into(),
            scope: "gateway offline_access".into(),
            page_origin: "https://radius.earendil.com".into(),
        }
    }

    /// A sleeper that records each wait and returns at once.
    fn recorder() -> (Sleeper, Arc<Mutex<Vec<u64>>>) {
        let waits = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&waits);
        let sleep: Sleeper = Arc::new(move |duration: Duration| {
            seen.lock().unwrap().push(duration.as_secs());
            Box::pin(async {})
        });
        (sleep, waits)
    }

    const CODE: &str = r#"{"device_code":"device-secret","user_code":"ABCD-EFGH","verification_uri":"https://radius.earendil.com/device","expires_in":120,"interval":2}"#;
    const TOKENS: &str = r#"{"access_token":"access-token","refresh_token":"refresh-token","expires_in":3600,"scope":"gateway offline_access"}"#;

    #[tokio::test]
    async fn device_login_polls_until_approved() {
        let (base, bodies) = serve(vec![
            (200, CODE),
            (400, r#"{"error":"authorization_pending"}"#),
            (200, TOKENS),
        ])
        .await;
        let endpoints = local(&base);
        let code = start(&endpoints).await.expect("a code");
        assert_eq!(code.user_code, "ABCD-EFGH");
        assert_eq!(code.page, "https://radius.earendil.com/device");
        assert!(!format!("{code:?}").contains("device-secret"));
        let (sleep, waits) = recorder();
        let tokens = wait(&endpoints, &code, &sleep).await.expect("tokens");
        assert_eq!(tokens.access.expose(), "access-token");
        assert_eq!(
            tokens.refresh.as_ref().map(Secret::expose),
            Some("refresh-token")
        );
        assert_eq!(tokens.expires_in, 3600);
        assert!(!format!("{tokens:?}").contains("token\""));
        assert_eq!(*waits.lock().unwrap(), vec![2, 2]);
        let bodies = bodies.lock().unwrap();
        assert_eq!(
            bodies[0],
            "client_id=pi-gateway&scope=gateway%20offline_access"
        );
        assert_eq!(
            bodies[1],
            "client_id=pi-gateway&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code=device-secret"
        );
    }

    #[tokio::test]
    async fn device_login_slow_down_waits_5_seconds_longer() {
        let (base, _) = serve(vec![
            (200, CODE),
            (400, r#"{"error":"slow_down"}"#),
            (400, r#"{"error":"authorization_pending"}"#),
            (200, TOKENS),
        ])
        .await;
        let endpoints = local(&base);
        let code = start(&endpoints).await.expect("a code");
        let (sleep, waits) = recorder();
        wait(&endpoints, &code, &sleep).await.expect("tokens");
        assert_eq!(*waits.lock().unwrap(), vec![2, 7, 7]);
    }

    #[tokio::test]
    async fn device_login_stops_on_denied_and_on_expiry() {
        let (base, _) = serve(vec![(200, CODE), (400, r#"{"error":"access_denied"}"#)]).await;
        let endpoints = local(&base);
        let code = start(&endpoints).await.expect("a code");
        let (sleep, _) = recorder();
        let error = wait(&endpoints, &code, &sleep).await.unwrap_err();
        assert!(error.contains("denied"), "{error}");

        let (base, _) = serve(vec![
            (200, r#"{"device_code":"d","user_code":"u","verification_uri":"https://radius.earendil.com/device","expires_in":3,"interval":2}"#),
            (400, r#"{"error":"authorization_pending"}"#),
            (400, r#"{"error":"authorization_pending"}"#),
        ])
        .await;
        let endpoints = local(&base);
        let code = start(&endpoints).await.expect("a code");
        let error = wait(&endpoints, &code, &sleep).await.unwrap_err();
        assert!(error.contains("expired"), "{error}");
    }

    #[tokio::test]
    async fn device_login_refuses_a_page_on_another_host() {
        for page in [
            "https://example.com/device",
            "javascript:alert(1)",
            "https://radius.earendil.com.evil.com/device",
            "https://user@radius.earendil.com/device",
            "http://radius.earendil.com/device",
        ] {
            let reply: &'static str = Box::leak(
                format!(
                    r#"{{"device_code":"d","user_code":"u","verification_uri":"{page}","expires_in":60}}"#
                )
                .into_boxed_str(),
            );
            let (base, _) = serve(vec![(200, reply)]).await;
            let error = start(&local(&base)).await.unwrap_err();
            assert!(
                error.contains("not on https://radius.earendil.com"),
                "{page}: {error}"
            );
        }
        assert!(on_origin(
            "https://radius.earendil.com",
            "https://radius.earendil.com"
        ));
        assert!(on_origin(
            "https://radius.earendil.com/pair?x=1",
            "https://radius.earendil.com"
        ));
    }

    #[tokio::test]
    async fn device_refresh_keeps_the_old_refresh_token_when_none_comes_back() {
        let (base, bodies) = serve(vec![
            (200, r#"{"access_token":"new-access","expires_in":60}"#),
            (
                400,
                r#"{"error":"invalid_grant","error_description":"secret detail"}"#,
            ),
        ])
        .await;
        let endpoints = local(&base);
        let tokens = refresh(&endpoints.token, &endpoints.client_id, "old-refresh")
            .await
            .expect("tokens");
        assert_eq!(tokens.access.expose(), "new-access");
        assert!(tokens.refresh.is_none());
        assert_eq!(
            bodies.lock().unwrap()[0],
            "client_id=pi-gateway&grant_type=refresh_token&refresh_token=old-refresh"
        );
        assert!(matches!(
            refresh(&endpoints.token, &endpoints.client_id, "old-refresh").await,
            Err(RefreshError::Expired)
        ));
    }

    #[tokio::test]
    async fn device_errors_never_show_the_reply_body() {
        let (base, _) = serve(vec![(
            500,
            r#"{"error":"server_error","access_token":"leak"}"#,
        )])
        .await;
        let error = start(&local(&base)).await.unwrap_err();
        assert_eq!(error, "the login could not start: HTTP 500 (server_error)");
    }

    #[tokio::test]
    async fn device_login_takes_a_fractional_interval_and_the_filled_in_page() {
        let (base, _) = serve(vec![(
            200,
            r#"{"device_code":"d","user_code":"ABCD-EFGH","verification_uri":"https://radius.earendil.com/pair","verification_uri_complete":"https://radius.earendil.com/pair?code=ABCD-EFGH","expires_in":300,"interval":0.1}"#,
        )])
        .await;
        let code = start(&local(&base)).await.expect("a code");
        assert_eq!(code.page, "https://radius.earendil.com/pair?code=ABCD-EFGH");
        assert_eq!(code.interval, 1);
        assert_eq!(code.expires_in, 300);
    }
}
