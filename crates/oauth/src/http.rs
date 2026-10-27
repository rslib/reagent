//! The HTTP client the logins share.

/// The user agent every request carries. Some gateways answer 403 to a request
/// without one.
const USER_AGENT: &str = concat!("reagent/", env!("CARGO_PKG_VERSION"));

/// reqwest is built without a crypto provider, so ring must be the process
/// default before a client is built. A second install is a no-op error.
fn install_ring() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Builds the client with the system root certificates, or with Mozilla's
/// bundled roots when the system has none. Minimal task containers often ship
/// without `ca-certificates`, and reqwest's default builder panics there.
pub fn client() -> reqwest::Client {
    install_ring();
    match reqwest::Client::builder()
        .user_agent(USER_AGENT)
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
        .user_agent(USER_AGENT)
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
}
