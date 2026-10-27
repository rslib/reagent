//! The shared OAuth pieces a login needs: a PKCE verifier and its challenge, a
//! random token, reading the code out of a callback URL, the device flow, the
//! ID token check and the browser callback.

pub mod device;
pub mod http;
pub mod jwt;
pub mod loopback;
pub mod metadata;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SecureRandom;

/// A one-request HTTP server the tests point a client at.
#[cfg(any(test, feature = "testing"))]
pub mod test_server;

/// The lowercase hex digits a token is written in.
const HEX: &[u8; 16] = b"0123456789abcdef";

/// The uppercase hex digits a percent escape is written in.
const UPPER_HEX: &[u8; 16] = b"0123456789ABCDEF";

/// The wait a [`Sleeper`] returns.
///
/// This crate has no runtime and so no timer: the caller hands one in, which
/// keeps every wait off the thread the process runs on.
pub type Wait = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Makes a wait of a duration, given by the caller at construction.
pub type Sleeper = Arc<dyn Fn(Duration) -> Wait + Send + Sync>;

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

/// The `auth-param` list of an HTTP challenge, such as the `Bearer` one of a
/// `WWW-Authenticate` header.
///
/// A quoted value keeps its commas and loses its backslash escapes; a scheme
/// word before the list is skipped.
pub fn auth_params(header: &str) -> Vec<(String, String)> {
    let bytes = header.as_bytes();
    let mut params = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if matches!(bytes[index], b',' | b' ' | b'\t') {
            index += 1;
            continue;
        }
        let name_start = index;
        while index < bytes.len() && !matches!(bytes[index], b'=' | b',' | b' ' | b'\t') {
            index += 1;
        }
        let name = &header[name_start..index];
        let equals = skip_spaces(bytes, index);
        if equals >= bytes.len() || bytes[equals] != b'=' {
            index = equals;
            continue;
        }
        let mut value = Vec::new();
        index = skip_spaces(bytes, equals + 1);
        if index < bytes.len() && bytes[index] == b'"' {
            index += 1;
            while index < bytes.len() {
                match bytes[index] {
                    b'\\' if index + 1 < bytes.len() => {
                        value.push(bytes[index + 1]);
                        index += 2;
                    }
                    b'"' => {
                        index += 1;
                        break;
                    }
                    byte => {
                        value.push(byte);
                        index += 1;
                    }
                }
            }
        } else {
            let value_start = index;
            while index < bytes.len() && !matches!(bytes[index], b',' | b' ' | b'\t') {
                index += 1;
            }
            value.extend_from_slice(&bytes[value_start..index]);
        }
        if !name.is_empty() {
            params.push((
                name.to_string(),
                String::from_utf8_lossy(&value).into_owned(),
            ));
        }
    }
    params
}

/// The first byte of `bytes` from `index` on that is not a space or a tab.
fn skip_spaces(bytes: &[u8], index: usize) -> usize {
    let mut at = index;
    while at < bytes.len() && matches!(bytes[at], b' ' | b'\t') {
        at += 1;
    }
    at
}

/// The `%XX` form of `text`, for a value put in a query string.
pub fn percent_encode(text: &str) -> String {
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
    fn oauth_auth_params_read_a_bearer_challenge() {
        let params = auth_params(
            r#"Bearer resource_metadata="https://a/.well-known/oauth-protected-resource/mcp", error="insufficient_scope", scope="files.read files.write""#,
        );
        assert_eq!(
            params,
            vec![
                (
                    "resource_metadata".to_string(),
                    "https://a/.well-known/oauth-protected-resource/mcp".to_string()
                ),
                ("error".to_string(), "insufficient_scope".to_string()),
                ("scope".to_string(), "files.read files.write".to_string()),
            ]
        );
    }

    #[test]
    fn oauth_auth_params_keep_the_commas_and_escapes_of_a_quoted_value() {
        let params = auth_params(r#"Bearer scope="a,b\"c", realm=plain"#);
        assert_eq!(
            params,
            vec![
                ("scope".to_string(), "a,b\"c".to_string()),
                ("realm".to_string(), "plain".to_string()),
            ]
        );
    }

    #[test]
    fn oauth_auth_params_of_a_bare_scheme_are_empty() {
        assert!(auth_params("Bearer").is_empty());
        assert!(auth_params("").is_empty());
        assert_eq!(
            auth_params("Basic realm=\"x\""),
            vec![("realm".to_string(), "x".to_string())]
        );
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
