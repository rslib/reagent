//! The HTTP client the logins share.

/// The user agent every request carries. Some gateways answer 403 to a request
/// without one.
const USER_AGENT: &str = concat!("reagent/", env!("CARGO_PKG_VERSION"));

/// reqwest is built without a crypto provider, so ring must be the process
/// default before a client is built. A second install is a no-op error.
fn install_ring() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The builder every client starts from: the user agent every request carries,
/// and no redirects, which each caller follows and checks itself.
fn builder() -> reqwest::ClientBuilder {
    install_ring();
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
}

/// Builds the client with the system root certificates, or with Mozilla's
/// bundled roots when the system has none. Minimal task containers often ship
/// without `ca-certificates`, and reqwest's default builder panics there.
pub fn client() -> reqwest::Client {
    built(|builder| builder)
}

/// The same client, with `resolver` looking up every name a request reaches,
/// and no proxy: a proxy would resolve the target host itself, outside whatever
/// the resolver checks.
pub fn client_with_resolver(
    resolver: std::sync::Arc<dyn reqwest::dns::Resolve>,
) -> reqwest::Client {
    built(move |builder| {
        builder
            .dns_resolver(std::sync::Arc::clone(&resolver))
            .no_proxy()
    })
}

/// The builder of the clients of the bundled roots: the fallback of a system
/// with no root certificate.
fn bundled() -> reqwest::ClientBuilder {
    let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .filter_map(|der| reqwest::Certificate::from_der(der).ok());
    builder().tls_certs_only(roots)
}

/// Builds the client `extra` asks for, retrying with the bundled roots when the
/// system has none.
fn built(extra: impl Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder) -> reqwest::Client {
    match extra(builder()).build() {
        Ok(client) => client,
        Err(_) => extra(bundled())
            .build()
            .expect("the bundled root certificates build a client"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_roots_build_a_client() {
        assert!(!webpki_root_certs::TLS_SERVER_ROOT_CERTS.is_empty());
        let _ = bundled().build().expect("the bundled roots build a client");
    }
}
