//! The HTTP client shared by the providers.

/// Builds the client with the system root certificates, or with Mozilla's
/// bundled roots when the system has none. Minimal task containers often ship
/// without `ca-certificates`, and reqwest's default builder panics there.
pub(crate) fn client() -> reqwest::Client {
    match reqwest::Client::builder().build() {
        Ok(client) => client,
        Err(_) => bundled_client(),
    }
}

fn bundled_client() -> reqwest::Client {
    let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .filter_map(|der| reqwest::Certificate::from_der(der).ok());
    reqwest::Client::builder()
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
}
