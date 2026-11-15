//! The Mistral Vibe login flow: the sign-in request, the poll of its process
//! and the exchange that gives the API key.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::value::RawValue;

use super::percent_encode;
use super::session::Sleeper;

/// The console every request of the login goes to.
pub const CONSOLE: &str = "https://console.mistral.ai";

/// How long the login waits between two polls.
pub const POLL: Duration = Duration::from_secs(3);

/// How many network failures in a row end the poll.
const FAILURES: u32 = 3;

/// The line of a sign-in that ran out of time, or that the account let expire.
pub const EXPIRED: &str = "mistral-vibe login expired; run reagent login mistral-vibe again";

/// The line of a sign-in the account refused.
pub const DENIED: &str = "mistral-vibe login was denied";

/// The line of a workspace that cannot use Vibe.
pub const NO_PLAN: &str =
    "mistral-vibe is not available on this workspace: the Vibe plan is not enabled for it";

/// The alphabet of base64url, in order.
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// How many random bytes the verifier carries.
const VERIFIER_BYTES: usize = 64;

/// A sign-in waiting for the account to sign in.
#[derive(Debug)]
pub struct SignIn {
    /// The id the exchange names.
    pub process_id: String,
    /// The page the user signs in on.
    pub sign_in_url: String,
    /// The page the login asks for the state of the process.
    pub poll_url: String,
    /// The seconds since the epoch the process stops at.
    pub expires_at: i64,
}

/// The seconds since the epoch, or 0 before 1970.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// A PKCE verifier for this login: 64 random bytes, base64url and unpadded.
///
/// The shared random source hands out one hex token at a time, so four of them
/// are decoded into the 64 bytes and the text is their base64url. The console
/// takes the S256 challenge of it, which is [`super::challenge`].
pub fn verifier() -> Result<String, String> {
    let mut bytes = Vec::with_capacity(VERIFIER_BYTES);
    while bytes.len() < VERIFIER_BYTES {
        let token = super::random_token()?;
        let token = hex_bytes(&token).ok_or_else(|| "no random source is available".to_string())?;
        bytes.extend_from_slice(&token);
    }
    bytes.truncate(VERIFIER_BYTES);
    Ok(base64url(&bytes))
}

/// The bytes a lowercase hex token holds.
fn hex_bytes(token: &str) -> Option<Vec<u8>> {
    let digits = token.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks(2) {
        let text = std::str::from_utf8(pair).ok()?;
        bytes.push(u8::from_str_radix(text, 16).ok()?);
    }
    Some(bytes)
}

/// `bytes` in base64url with no padding.
fn base64url(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let mut word = 0u32;
        for (index, byte) in group.iter().enumerate() {
            word |= u32::from(*byte) << (16 - 8 * index);
        }
        for index in 0..=group.len() {
            text.push(BASE64URL[((word >> (18 - 6 * index)) & 0x3f) as usize] as char);
        }
    }
    text
}

/// The scheme, host and port of `url`: what a page must share with the console.
fn origin(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() || rest.is_empty() {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some(&url[..scheme.len() + 3 + end])
}

/// Whether `url` sits on the console `console`.
fn on_console(console: &str, url: &str) -> bool {
    match (origin(console), origin(url)) {
        (Some(want), Some(got)) => want.eq_ignore_ascii_case(got),
        _ => false,
    }
}

/// The sign-in request body.
#[derive(serde::Serialize)]
struct Start<'a> {
    code_challenge: &'a str,
    code_challenge_method: &'a str,
}

/// The sign-in reply as the console writes it.
#[derive(Deserialize)]
struct StartReply {
    process_id: String,
    sign_in_url: String,
    poll_url: String,
    expires_at: String,
}

/// Starts a sign-in for `challenge` at `console`, and refuses a page off it.
///
/// `console` is a parameter so a test drives the flow against its own server;
/// the command passes [`CONSOLE`].
pub async fn sign_in(console: &str, challenge: &str) -> Result<SignIn, String> {
    let body = Start {
        code_challenge: challenge,
        code_challenge_method: "S256",
    };
    let bytes = serde_json::to_vec(&body)
        .map_err(|_| "mistral-vibe login failed: cannot build the request".to_string())?;
    let response = crate::http::client()
        .post(format!("{console}/api/vibe/sign-in"))
        .header("content-type", "application/json")
        .body(bytes::Bytes::from(bytes))
        .send()
        .await
        .map_err(|error| format!("mistral-vibe login failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "mistral-vibe login failed: HTTP {}",
            status.as_u16()
        ));
    }
    let text = response
        .text()
        .await
        .map_err(|_| "mistral-vibe login failed: no sign-in page in reply".to_string())?;
    let reply: StartReply = serde_json::from_str(&text)
        .map_err(|_| "mistral-vibe login failed: no sign-in page in reply".to_string())?;
    let expires_at = epoch_of(&reply.expires_at)
        .ok_or_else(|| "mistral-vibe login failed: the reply names no expiry".to_string())?;
    // A page off the console is never opened and never asked.
    if !on_console(console, &reply.sign_in_url) {
        return Err(format!(
            "mistral-vibe login refused a sign-in page off {console}"
        ));
    }
    if !reply.poll_url.starts_with(&format!("{console}/api/")) {
        return Err(format!(
            "mistral-vibe login refused a poll page off {console}/api/"
        ));
    }
    Ok(SignIn {
        process_id: reply.process_id,
        sign_in_url: reply.sign_in_url,
        poll_url: reply.poll_url,
        expires_at,
    })
}

/// What one poll of a sign-in says.
enum Poll {
    /// The account has not signed in yet.
    Pending,
    /// The account signed in, and this is the token the exchange takes.
    Completed(String),
    /// The process ran out of time.
    Expired,
    /// The account refused the sign-in.
    Denied,
    /// The console named a reason.
    Failed(String),
}

/// The poll reply as the console writes it.
#[derive(Deserialize)]
struct PollReply {
    status: String,
    exchange_token: Option<String>,
    message: Option<String>,
}

/// Asks the console once for the state of the process at `url`.
async fn poll_once(url: &str) -> Result<Poll, String> {
    let response = crate::http::client()
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    if status.as_u16() == 410 {
        return Ok(Poll::Expired);
    }
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let text = response.text().await.unwrap_or_default();
    let reply: PollReply =
        serde_json::from_str(&text).map_err(|_| "the console gave no status".to_string())?;
    Ok(match reply.status.as_str() {
        "pending" => Poll::Pending,
        "completed" => match reply.exchange_token.filter(|token| !token.is_empty()) {
            Some(token) => Poll::Completed(token),
            None => Poll::Failed("a finished sign-in without its token".to_string()),
        },
        "expired" => Poll::Expired,
        "denied" => Poll::Denied,
        "error" => Poll::Failed(
            reply
                .message
                .unwrap_or_else(|| "the console gave no reason".to_string()),
        ),
        other => Poll::Failed(format!("the console gave an unknown status {other}")),
    })
}

/// Polls `sign_in` every [`POLL`] until the account finishes, and returns the
/// exchange token.
///
/// A poll that fails to reach the console is tried again; [`FAILURES`] of them
/// in a row end the login, so a console that is down does not hold the command.
pub async fn wait(sign_in: &SignIn, sleep: &Sleeper) -> Result<String, String> {
    let mut failures = 0;
    loop {
        if now() >= sign_in.expires_at {
            return Err(EXPIRED.to_string());
        }
        match poll_once(&sign_in.poll_url).await {
            Ok(Poll::Pending) => failures = 0,
            Ok(Poll::Completed(token)) => return Ok(token),
            Ok(Poll::Expired) => return Err(EXPIRED.to_string()),
            Ok(Poll::Denied) => return Err(DENIED.to_string()),
            Ok(Poll::Failed(message)) => {
                return Err(format!("mistral-vibe login failed: {message}"));
            }
            Err(error) => {
                failures += 1;
                if failures == FAILURES {
                    return Err(format!("mistral-vibe login failed: {error}"));
                }
            }
        }
        sleep(POLL).await;
    }
}

/// The exchange request body.
#[derive(serde::Serialize)]
struct Exchange<'a> {
    exchange_token: &'a str,
    code_verifier: &'a str,
}

/// A refusal of the console, of which only the error code matters.
#[derive(Deserialize)]
struct Refusal<'a> {
    #[serde(default, borrow)]
    error_code: Option<&'a RawValue>,
}

/// The exchange reply, of which only the key matters.
#[derive(Deserialize)]
struct KeyReply {
    api_key: Option<String>,
}

/// Trades the exchange token and `verifier` for the API key of the account.
///
/// The key does not expire. The reply body never reaches an error message: it
/// may echo the token.
pub async fn exchange(
    console: &str,
    process_id: &str,
    token: &str,
    verifier: &str,
) -> Result<String, String> {
    let body = Exchange {
        exchange_token: token,
        code_verifier: verifier,
    };
    let bytes = serde_json::to_vec(&body)
        .map_err(|_| "mistral-vibe login failed: cannot build the request".to_string())?;
    let url = format!(
        "{console}/api/vibe/sign-in/{}/exchange",
        percent_encode(process_id)
    );
    let response = crate::http::client()
        .post(url)
        .header("content-type", "application/json")
        .body(bytes::Bytes::from(bytes))
        .send()
        .await
        .map_err(|error| format!("mistral-vibe login failed: {error}"))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        if status.as_u16() == 403 && workspace_without_vibe(&text) {
            return Err(NO_PLAN.to_string());
        }
        return Err(format!(
            "mistral-vibe login failed: HTTP {}",
            status.as_u16()
        ));
    }
    let reply: KeyReply = serde_json::from_str(&text)
        .map_err(|_| "mistral-vibe login failed: no key in reply".to_string())?;
    reply
        .api_key
        .filter(|key| !key.is_empty())
        .ok_or_else(|| "mistral-vibe login failed: no key in reply".to_string())
}

/// Whether a refused exchange body says the workspace has no Vibe plan, which
/// the console writes as the error code 9004.
fn workspace_without_vibe(body: &str) -> bool {
    let Ok(refusal) = serde_json::from_str::<Refusal<'_>>(body) else {
        return false;
    };
    let Some(code) = refusal.error_code else {
        return false;
    };
    // The code is a number in the documentation and a string on some answers.
    code.get().trim_matches('"') == "9004"
}

/// The seconds since the epoch of the RFC 3339 time `text`, or `None`.
fn epoch_of(text: &str) -> Option<i64> {
    if !text.is_ascii() || text.len() < 20 {
        return None;
    }
    let field = |start: usize, end: usize| text.get(start..end)?.parse::<i64>().ok();
    let (year, month, day) = (field(0, 4)?, field(5, 7)?, field(8, 10)?);
    let (hour, minute, second) = (field(11, 13)?, field(14, 16)?, field(17, 19)?);
    let bytes = text.as_bytes();
    if (bytes[4], bytes[7]) != (b'-', b'-')
        || !matches!(bytes[10], b'T' | b't')
        || (bytes[13], bytes[16]) != (b':', b':')
    {
        return None;
    }
    // The fraction of a second is read and dropped: a login needs no part of it.
    let mut rest = text.get(19..)?;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction
            .find(|char: char| !char.is_ascii_digit())
            .unwrap_or(fraction.len());
        if digits == 0 {
            return None;
        }
        rest = &fraction[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hours: i64 = rest.get(1..3)?.parse().ok()?;
            let minutes: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (hours * 3_600 + minutes * 60)
        }
    };
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

/// The days from 1970-01-01 to the civil date, by Howard Hinnant's algorithm.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::oauth::session::Wait;
    use crate::oauth::test_server;

    /// A fake console: the base URL of a server that answers one
    /// `(status, reply)` pair per request, in order, and the requests it read.
    ///
    /// The listener is bound before `answers` runs, so a reply can name the
    /// base URL.
    async fn console(
        answers: impl FnOnce(&str) -> Vec<(u16, String)>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
        let base = format!(
            "http://{}",
            listener.local_addr().expect("the bound address")
        );
        let replies = answers(&base);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        tokio::spawn(async move {
            for (status, reply) in replies {
                let (mut socket, _) = listener.accept().await.expect("accept a connection");
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                while let Ok(read) = socket.read(&mut chunk).await {
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                recorder
                    .lock()
                    .expect("the recorder")
                    .push(String::from_utf8_lossy(&buffer).into_owned());
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (base, seen)
    }

    /// A sleeper that finishes at once, so a poll costs no wait.
    fn instant() -> Sleeper {
        Arc::new(|_: Duration| -> Wait { Box::pin(std::future::ready(())) })
    }

    /// A sign-in ready to poll at `url`.
    fn ready(url: &str) -> SignIn {
        SignIn {
            process_id: "p-1".to_string(),
            sign_in_url: "https://console.mistral.ai/sign-in/p-1".to_string(),
            poll_url: url.to_string(),
            expires_at: now() + 600,
        }
    }

    #[test]
    fn mistral_vibe_verifier_is_64_bytes_of_base64url() {
        // The RFC 4648 vectors, unpadded.
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
        let one = verifier().expect("a verifier");
        let other = verifier().expect("a second verifier");
        assert_eq!(one.len(), 86, "64 bytes in base64url: {one}");
        assert!(
            one.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
            "{one}"
        );
        assert_ne!(one, other, "a fresh verifier each login");
        assert_eq!(super::super::challenge(&one).len(), 43);
    }

    #[test]
    fn mistral_vibe_expiry_reads_rfc_3339() {
        assert_eq!(epoch_of("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_of("2024-02-29T12:34:56Z"), Some(1_709_210_096));
        assert_eq!(epoch_of("2024-02-29T13:34:56+01:00"), Some(1_709_210_096));
        assert_eq!(epoch_of("2024-02-29T12:34:56.123456Z"), Some(1_709_210_096));
        assert_eq!(epoch_of("2024-02-29T07:34:56-05:00"), Some(1_709_210_096));
        assert_eq!(epoch_of("not a time"), None);
        assert_eq!(epoch_of(""), None);
        assert_eq!(epoch_of("2024-02-29 12:34:56Z"), None);
        assert_eq!(epoch_of("2024-02-29T12:34:56"), None);
    }

    #[tokio::test]
    async fn mistral_vibe_sign_in_posts_the_challenge_and_reads_the_pages() {
        let (base, seen) = console(|base| {
            vec![(
                200,
                format!(
                    r#"{{"process_id":"p-1","sign_in_url":"{base}/sign-in/p-1","poll_url":"{base}/api/vibe/sign-in/p-1/poll","expires_at":"2099-01-01T00:00:00Z"}}"#
                ),
            )]
        })
        .await;
        let sign_in = super::sign_in(&base, "the-challenge")
            .await
            .expect("the sign-in starts");
        assert_eq!(sign_in.process_id, "p-1");
        assert_eq!(sign_in.sign_in_url, format!("{base}/sign-in/p-1"));
        assert_eq!(
            sign_in.poll_url,
            format!("{base}/api/vibe/sign-in/p-1/poll")
        );
        assert!(sign_in.expires_at > now(), "a future expiry");
        let seen = seen.lock().expect("the recorder");
        assert!(
            seen[0].starts_with("POST /api/vibe/sign-in "),
            "{}",
            seen[0]
        );
        assert!(
            seen[0].contains(r#""code_challenge":"the-challenge""#),
            "{}",
            seen[0]
        );
        assert!(
            seen[0].contains(r#""code_challenge_method":"S256""#),
            "{}",
            seen[0]
        );
    }

    #[tokio::test]
    async fn mistral_vibe_sign_in_refuses_a_page_off_the_console() {
        let (base, _seen) = console(|base| {
            vec![(
                200,
                format!(
                    r#"{{"process_id":"p-1","sign_in_url":"https://evil.example/sign-in/p-1","poll_url":"{base}/api/vibe/sign-in/p-1/poll","expires_at":"2099-01-01T00:00:00Z"}}"#
                ),
            )]
        })
        .await;
        let error = super::sign_in(&base, "challenge").await.unwrap_err();
        assert!(error.contains("refused a sign-in page"), "{error}");
        assert!(!error.contains("evil.example"), "the page is not echoed");

        let (base, _seen) = console(|base| {
            vec![(
                200,
                format!(
                    r#"{{"process_id":"p-1","sign_in_url":"{base}/sign-in/p-1","poll_url":"https://evil.example/api/vibe/poll","expires_at":"2099-01-01T00:00:00Z"}}"#
                ),
            )]
        })
        .await;
        let error = super::sign_in(&base, "challenge").await.unwrap_err();
        assert!(error.contains("refused a poll page"), "{error}");
    }

    #[tokio::test]
    async fn mistral_vibe_wait_polls_until_the_token() {
        let (base, seen) = console(|_| {
            vec![
                (200, r#"{"status":"pending"}"#.to_string()),
                (200, r#"{"status":"pending"}"#.to_string()),
                (
                    200,
                    r#"{"status":"completed","exchange_token":"the-exchange"}"#.to_string(),
                ),
            ]
        })
        .await;
        let token = wait(&ready(&format!("{base}/api/vibe/poll")), &instant())
            .await
            .expect("the sign-in finishes");
        assert_eq!(token, "the-exchange");
        assert_eq!(seen.lock().expect("the recorder").len(), 3);
    }

    #[tokio::test]
    async fn mistral_vibe_wait_ends_on_denied_and_on_expired() {
        let (base, _seen) = console(|_| {
            vec![
                (200, r#"{"status":"denied"}"#.to_string()),
                (200, r#"{"status":"expired"}"#.to_string()),
                (410, r#"{"status":"expired"}"#.to_string()),
            ]
        })
        .await;
        for (reply_index, want) in [(0, DENIED), (1, EXPIRED), (2, EXPIRED)] {
            let ready = ready(&format!("{base}/api/vibe/poll"));
            // Each answer is served once, so each case points at the next.
            let error = wait(&ready, &instant()).await.unwrap_err();
            assert_eq!(error, want, "reply {reply_index}");
        }
    }

    #[tokio::test]
    async fn mistral_vibe_wait_ends_when_the_console_is_silent() {
        // A port nothing listens on is one network failure each try.
        let error = wait(&ready("http://127.0.0.1:1/api/vibe/poll"), &instant())
            .await
            .unwrap_err();
        assert!(error.starts_with("mistral-vibe login failed:"), "{error}");
    }

    #[tokio::test]
    async fn mistral_vibe_wait_ends_past_the_expiry() {
        let mut sign_in = ready("http://127.0.0.1:1/api/vibe/poll");
        sign_in.expires_at = now() - 1;
        let error = wait(&sign_in, &instant()).await.unwrap_err();
        assert_eq!(error, EXPIRED);
    }

    #[tokio::test]
    async fn mistral_vibe_exchange_reads_the_key() {
        let (base, recorded) = test_server::serve(200, r#"{"api_key":"sk-vibe-test"}"#).await;
        let key = exchange(&base, "p-1", "the-exchange", "the-verifier")
            .await
            .expect("the exchange succeeds");
        assert_eq!(key, "sk-vibe-test");
        let request = recorded.await.expect("the server recorded the request");
        assert!(
            request
                .request_line
                .starts_with("POST /api/vibe/sign-in/p-1/exchange"),
            "{request:?}"
        );
        assert!(
            request.body.contains(r#""exchange_token":"the-exchange""#),
            "{request:?}"
        );
        assert!(
            request.body.contains(r#""code_verifier":"the-verifier""#),
            "{request:?}"
        );
    }

    #[tokio::test]
    async fn mistral_vibe_exchange_reports_a_workspace_without_vibe() {
        let refuse = |body: &'static str| async move {
            let (base, _recorded) = test_server::serve(403, body).await;
            exchange(&base, "p-1", "the-exchange", "v")
                .await
                .unwrap_err()
        };
        assert_eq!(
            refuse(r#"{"error_code":9004,"message":"no vibe"}"#).await,
            NO_PLAN
        );
        assert_eq!(
            refuse(r#"{"error_code":"9004"}"#).await,
            NO_PLAN,
            "the code as a string"
        );
        let other = refuse(r#"{"error_code":9001}"#).await;
        assert_eq!(other, "mistral-vibe login failed: HTTP 403");
    }

    #[tokio::test]
    async fn mistral_vibe_exchange_refuses_a_reply_without_a_key() {
        let (base, _recorded) = test_server::serve(200, r#"{"api_key":""}"#).await;
        let error = exchange(&base, "p-1", "the-exchange", "v")
            .await
            .unwrap_err();
        assert_eq!(error, "mistral-vibe login failed: no key in reply");
        assert!(!error.contains("the-exchange"), "the token is shown");
    }
}
