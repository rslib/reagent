//! The OAuth PKCE pieces a `login` command needs: a verifier and its
//! challenge, a random token, and reading the code out of a callback URL.

pub mod chatgpt;
pub mod jwt;
pub mod openrouter;
pub mod session;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SecureRandom;

/// The lowercase hex digits a token is written in.
const HEX: &[u8; 16] = b"0123456789abcdef";

/// The uppercase hex digits a percent escape is written in.
const UPPER_HEX: &[u8; 16] = b"0123456789ABCDEF";

/// A PKCE verifier and the S256 challenge of it.
///
/// `Debug` hides the verifier, so a logged or panicked `Pkce` never shows the
/// text that redeems the code.
pub struct Pkce {
    /// The secret text sent only to the token endpoint.
    pub verifier: String,
    /// The SHA-256 of the verifier, sent in the authorize URL.
    pub challenge: String,
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Pkce")
            .field("verifier", &"***")
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// `count` bytes from the system random source.
fn random(count: usize) -> Result<Vec<u8>, String> {
    let mut buffer = vec![0u8; count];
    ring::rand::SystemRandom::new()
        .fill(&mut buffer)
        .map_err(|_| "no random source is available".to_string())?;
    Ok(buffer)
}

/// A fresh verifier and its challenge, per RFC 7636.
pub fn pkce() -> Result<Pkce, String> {
    let verifier = URL_SAFE_NO_PAD.encode(random(32)?);
    let challenge = challenge(&verifier);
    Ok(Pkce {
        verifier,
        challenge,
    })
}

/// The base64url, unpadded SHA-256 of `verifier`, its S256 challenge.
pub fn challenge(verifier: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest.as_ref())
}

/// A random token in hex, for a callback path or a `state` value.
pub fn random_token() -> Result<String, String> {
    let bytes = random(16)?;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(HEX[usize::from(byte >> 4)] as char);
        text.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    Ok(text)
}

/// A `urn:uuid:<v4>` id for the agent host, made once and saved with the
/// login so a later login is the same agent.
pub fn host_id() -> Result<String, String> {
    let mut bytes = random(16)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut text = String::with_capacity("urn:uuid:".len() + 36);
    text.push_str("urn:uuid:");
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        text.push(HEX[usize::from(byte >> 4)] as char);
        text.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    Ok(text)
}

/// The decoded query of a callback request that targets `path`.
///
/// `target` is the request target of a GET, `/auth/callback?code=X&x=1`, or a
/// whole URL pasted by the user. `None` when the path differs; the query of a
/// request with no `?` is then empty.
pub fn callback_query(target: &str, path: &str) -> Option<Vec<(String, String)>> {
    let target = target.trim();
    let (target, query) = target.split_once('?').unwrap_or((target, ""));
    if path_of(target) != path {
        return None;
    }
    Some(
        query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                (percent_decode(name), percent_decode(value))
            })
            .collect(),
    )
}

/// The `code` of a callback request that targets `path`.
///
/// `None` when the path differs or no non-empty `code` is there.
pub fn callback_code(target: &str, path: &str) -> Option<String> {
    callback_query(target, path)?
        .into_iter()
        .find_map(|(name, value)| (name == "code" && !value.is_empty()).then_some(value))
}

/// The path of a request target, with any `scheme://host` taken off.
fn path_of(target: &str) -> &str {
    let rest = target.split_once("://").map_or(target, |(_, rest)| rest);
    match rest.find('/') {
        Some(start) => &rest[start..],
        None => "",
    }
}

/// Reads the `%XX` escapes of `text`.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = |at: usize| (bytes[at] as char).to_digit(16);
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => match (hex(index + 1), hex(index + 2)) {
                (Some(high), Some(low)) => {
                    out.push((high * 16 + low) as u8);
                    index += 3;
                }
                _ => {
                    out.push(bytes[index]);
                    index += 1;
                }
            },
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `%XX` form of `text`, for a value put in a query string.
pub(crate) fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(UPPER_HEX[usize::from(byte >> 4)] as char);
            out.push(UPPER_HEX[usize::from(byte & 0x0f)] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_the_rfc_7636_vector() {
        // Appendix B of RFC 7636.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn oauth_pkce_gives_a_43_char_verifier_that_differs_each_call() {
        let first = pkce().expect("random works");
        let second = pkce().expect("random works");
        assert_eq!(first.verifier.len(), 43);
        assert_ne!(first.verifier, second.verifier);
        assert_eq!(first.challenge, challenge(&first.verifier));
        assert_eq!(first.challenge.len(), 43);
    }

    #[test]
    fn oauth_pkce_debug_hides_the_verifier() {
        let pkce = Pkce {
            verifier: "secret-verifier".into(),
            challenge: "public-challenge".into(),
        };
        let text = format!("{pkce:?}");
        assert!(!text.contains("secret-verifier"), "{text}");
        assert!(text.contains("public-challenge"), "{text}");
    }

    #[test]
    fn oauth_random_token_is_32_hex_chars_that_differ() {
        let first = random_token().expect("random works");
        let second = random_token().expect("random works");
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first}");
        assert_ne!(first, second);
    }

    #[test]
    fn oauth_callback_query_reads_the_decoded_pairs_of_a_matching_target() {
        let path = "/auth/callback";
        assert_eq!(
            callback_query("/auth/callback?code=a%2Fb&state=s%20t", path),
            Some(vec![
                ("code".to_string(), "a/b".to_string()),
                ("state".to_string(), "s t".to_string()),
            ])
        );
        assert_eq!(
            callback_query(
                "http://127.0.0.1:1455/auth/callback?error=access_denied",
                path
            ),
            Some(vec![("error".to_string(), "access_denied".to_string())])
        );
        assert_eq!(callback_query("/auth/callback", path), Some(Vec::new()));
        assert_eq!(
            callback_query("  /auth/callback?scope=a+b  ", path),
            Some(vec![("scope".to_string(), "a+b".to_string())])
        );
        assert_eq!(callback_query("/auth/other?code=X", path), None);
        assert_eq!(callback_query("", path), None);
    }

    #[test]
    fn oauth_callback_code_still_reads_only_a_non_empty_code() {
        let path = "/auth/callback";
        assert_eq!(callback_code("/auth/callback?error=denied", path), None);
        assert_eq!(callback_code("/auth/callback?code=&state=s", path), None);
        assert_eq!(
            callback_code("/auth/callback?state=s&code=c%201", path).as_deref(),
            Some("c 1")
        );
    }

    #[test]
    fn oauth_callback_code_reads_a_matching_request_target() {
        let path = "/oauth/callback/abc";
        assert_eq!(
            callback_code("/oauth/callback/abc?code=XYZ&x=1", path).as_deref(),
            Some("XYZ")
        );
        assert_eq!(
            callback_code("/oauth/callback/abc?x=1&code=X", path),
            Some("X".into())
        );
        assert_eq!(callback_code("/oauth/other?code=XYZ", path), None);
        assert_eq!(callback_code("/oauth/callback/abc?x=1", path), None);
        assert_eq!(callback_code("/oauth/callback/abc?code=", path), None);
        assert_eq!(
            callback_code("/oauth/callback/abc?code=a%2Fb%20c", path).as_deref(),
            Some("a/b c")
        );
        assert_eq!(
            callback_code("http://127.0.0.1:5000/oauth/callback/abc?code=XYZ", path).as_deref(),
            Some("XYZ")
        );
        assert_eq!(
            callback_code(
                "  https://h/o/oauth/callback/abc?code=Q  ",
                "/o/oauth/callback/abc"
            )
            .as_deref(),
            Some("Q")
        );
    }
}

/// A one-request HTTP server the tests point the client at.
#[cfg(test)]
pub(crate) mod test_server {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;

    /// One request the test server read, head and body.
    #[derive(Debug)]
    pub(crate) struct Request {
        /// The request line, for example `POST /api/v1/auth/keys HTTP/1.1`.
        pub(crate) request_line: String,
        /// The body, as text.
        pub(crate) body: String,
    }

    /// Serves one request, answering `status` with `reply`.
    ///
    /// Returns the base URL and a receiver for the recorded request.
    pub(crate) async fn serve(
        status: u16,
        reply: &'static str,
    ) -> (String, oneshot::Receiver<Request>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
        let address = listener.local_addr().expect("the bound address");
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept a connection");
            let request = read_request(&mut socket).await;
            let _ = sender.send(request);
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        (format!("http://{address}"), receiver)
    }

    /// The end of the request head, past the blank line.
    fn head_end(buffer: &[u8]) -> Option<usize> {
        buffer.windows(4).position(|window| window == b"\r\n\r\n")
    }

    /// The `content-length` of a request head, or 0.
    fn content_length(head: &str) -> usize {
        head.lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Reads one request off `socket`, head and body.
    async fn read_request(socket: &mut TcpStream) -> Request {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let end = head_end(&buffer);
            if let Some(end) = end {
                let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
                if buffer.len() >= end + 4 + content_length(&head) {
                    break;
                }
            }
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        }
        let text = String::from_utf8_lossy(&buffer).into_owned();
        let (head, body) = match text.split_once("\r\n\r\n") {
            Some((head, body)) => (head, body),
            None => (text.as_str(), ""),
        };
        let request_line = head.lines().next().unwrap_or_default().to_owned();
        Request {
            request_line,
            body: body.to_owned(),
        }
    }
}
