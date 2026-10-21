//! The HTTP client shared by the providers.

use reqwest::header::{HeaderName, HeaderValue};

/// Extra request headers a caller adds to every provider request.
///
/// A header that [`check_header`] rejects, or a repeated name, is dropped,
/// never applied, so an extra header cannot replace the credential or the body
/// encoding. `Debug` prints the names only, never the values.
#[derive(Clone, Default)]
pub(crate) struct ExtraHeaders(Vec<(HeaderName, HeaderValue)>);

/// Header names the provider sets itself, which an extra header must not replace.
const RESERVED_HEADERS: [&str; 4] = [
    "authorization",
    "x-api-key",
    "content-type",
    "anthropic-version",
];

/// Checks that an extra request header is valid and not reserved.
///
/// The error never contains `value`, so a key passed by mistake is not printed.
pub fn check_header(name: &str, value: &str) -> Result<(), String> {
    let parsed = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| format!("invalid header name: {name}"))?;
    if RESERVED_HEADERS.contains(&parsed.as_str()) {
        return Err(format!("reserved header name: {name}"));
    }
    HeaderValue::from_str(value).map_err(|_| format!("invalid value for header {name}"))?;
    Ok(())
}

impl ExtraHeaders {
    /// Builds the set from `headers`, dropping reserved, invalid and duplicate names.
    pub(crate) fn new(headers: Vec<(String, String)>) -> Self {
        let mut pairs: Vec<(HeaderName, HeaderValue)> = Vec::with_capacity(headers.len());
        for (name, value) in headers {
            if check_header(&name, &value).is_err() {
                continue;
            }
            let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&value),
            ) else {
                continue;
            };
            if pairs.iter().any(|(seen, _)| *seen == name) {
                continue;
            }
            pairs.push((name, value));
        }
        Self(pairs)
    }

    /// Adds every header to `builder`.
    pub(crate) fn apply(&self, mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (name, value) in &self.0 {
            builder = builder.header(name, value);
        }
        builder
    }
}

impl std::fmt::Debug for ExtraHeaders {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.0.iter().map(|(name, _)| name.as_str()).collect();
        formatter.debug_tuple("ExtraHeaders").field(&names).finish()
    }
}

/// reqwest is built without a crypto provider, so ring must be the process
/// default before a client is built. A second install is a no-op error.
fn install_ring() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Builds the client with the system root certificates, or with Mozilla's
/// bundled roots when the system has none. Minimal task containers often ship
/// without `ca-certificates`, and reqwest's default builder panics there.
pub(crate) fn client() -> reqwest::Client {
    install_ring();
    match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => bundled_client(),
    }
}

fn bundled_client() -> reqwest::Client {
    install_ring();
    let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .filter_map(|der| reqwest::Certificate::from_der(der).ok());
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .tls_certs_only(roots)
        .build()
        .expect("the bundled root certificates build a client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_roots_build_a_client() {
        assert!(!webpki_root_certs::TLS_SERVER_ROOT_CERTS.is_empty());
        let _ = bundled_client();
    }

    #[test]
    fn extra_headers_drop_reserved_names_and_hide_values() {
        let headers = ExtraHeaders::new(vec![
            ("x-extra".into(), "visible".into()),
            ("authorization".into(), "Bearer injected".into()),
            ("x-api-key".into(), "injected".into()),
            ("content-type".into(), "text/plain".into()),
            ("Anthropic-Version".into(), "1999-01-01".into()),
            ("x-extra".into(), "duplicate".into()),
            ("bad name".into(), "value".into()),
        ]);
        let text = format!("{headers:?}");
        assert!(text.contains("x-extra"), "{text}");
        assert!(!text.contains("visible"), "{text}");
        assert!(!text.contains("injected"), "{text}");

        let request = headers
            .apply(client().post("http://example.invalid"))
            .build()
            .expect("build the request");
        assert_eq!(
            request
                .headers()
                .get("x-extra")
                .and_then(|v| v.to_str().ok()),
            Some("visible")
        );
        assert!(request.headers().get("authorization").is_none());
        assert!(request.headers().get("x-api-key").is_none());
        assert!(request.headers().get("content-type").is_none());
        assert!(request.headers().get("anthropic-version").is_none());
    }

    #[test]
    fn check_header_rejects_bad_and_reserved_names_without_the_value() {
        assert!(check_header("x-opencode-session", "abc").is_ok());
        for (name, value) in [
            ("bad name", "sk-secret"),
            ("Authorization", "sk-secret"),
            ("anthropic-version", "sk-secret"),
            ("x-extra", "sk-secret\n"),
        ] {
            let error = check_header(name, value).unwrap_err();
            assert!(!error.contains("sk-secret"), "{error}");
        }
    }
}

/// A single-request HTTP server the provider tests point their clients at.
#[cfg(test)]
pub(crate) mod test_server {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    /// One HTTP request the test server recorded.
    #[derive(Debug)]
    pub(crate) struct Recorded {
        /// The request line, for example `POST /v1/messages HTTP/1.1`.
        pub(crate) request_line: String,
        /// The header names, lowercased, and their values.
        pub(crate) headers: Vec<(String, String)>,
    }

    impl Recorded {
        /// The value of the header named `name`, or `None`.
        pub(crate) fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(header, _)| header == name)
                .map(|(_, value)| value.as_str())
        }
    }

    /// Serves one request with `body` as an SSE reply.
    ///
    /// Returns the base URL and a receiver for the recorded request. The
    /// server task ends after it answers that one request.
    pub(crate) async fn serve(body: &'static str) -> (String, oneshot::Receiver<Recorded>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
        let address = listener.local_addr().expect("the bound address");
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept a connection");
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                match socket.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(read) => {
                        buffer.extend_from_slice(&chunk[..read]);
                        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let text = String::from_utf8_lossy(&buffer);
            let mut lines = text.split("\r\n");
            let request_line = lines.next().unwrap_or_default().to_owned();
            let headers = lines
                .take_while(|line| !line.is_empty())
                .filter_map(|line| line.split_once(": "))
                .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
                .collect();
            let _ = sender.send(Recorded {
                request_line,
                headers,
            });
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        (format!("http://{address}"), receiver)
    }
}
