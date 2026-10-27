//! The metadata documents a login reads before anything else: the protected
//! resource of RFC 9728 and the authorization server of RFC 8414, with the
//! well-known URLs they live at.

use std::time::Duration;

use serde::Deserialize;

/// How long one metadata request may take.
const TIMEOUT: Duration = Duration::from_secs(15);

/// The well-known path of the protected resource metadata.
const RESOURCE_PATH: &str = "/.well-known/oauth-protected-resource";

/// The well-known path of the authorization server metadata.
const SERVER_PATH: &str = "/.well-known/oauth-authorization-server";

/// The well-known path of the OpenID provider metadata.
const OPENID_PATH: &str = "/.well-known/openid-configuration";

/// The only PKCE method a login uses.
const S256: &str = "S256";

/// The protected resource metadata of RFC 9728.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resource {
    /// The resource the document describes.
    pub resource: Option<String>,
    /// The issuers that authorize this resource, the first one first.
    pub authorization_servers: Vec<String>,
    /// The scopes the resource understands.
    pub scopes_supported: Vec<String>,
}

impl Resource {
    /// Checks that the document names `resource` as its own resource.
    ///
    /// A document that names another URL describes a different resource, so a
    /// login that reads it would get a token for the wrong one.
    pub fn bound_to(&self, resource: &str) -> Result<(), String> {
        match self.resource.as_deref() {
            Some(named) if canonical(named) == canonical(resource) => Ok(()),
            Some(named) => Err(format!(
                "the protected resource metadata names {named}, but the MCP server is {resource}"
            )),
            None => Err(format!(
                "the protected resource metadata names no resource; the MCP server is {resource}"
            )),
        }
    }
}

/// The authorization server metadata of RFC 8414.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Server {
    /// The issuer, which is the URL the document was found for.
    pub issuer: String,
    /// The endpoint the browser is sent to.
    pub authorization_endpoint: String,
    /// The endpoint a code is traded at.
    pub token_endpoint: String,
    /// The endpoint that registers a client, when the server has one.
    pub registration_endpoint: Option<String>,
    /// The scopes the server understands.
    pub scopes_supported: Vec<String>,
    /// Whether the server accepts a client id metadata document.
    pub client_metadata_document: bool,
    /// Whether the callback carries an `iss` parameter.
    pub iss_parameter: bool,
}

#[derive(Deserialize)]
struct ResourceDocument<'a> {
    #[serde(borrow)]
    resource: Option<&'a str>,
    #[serde(borrow, default)]
    authorization_servers: Vec<&'a str>,
    #[serde(borrow, default)]
    scopes_supported: Vec<&'a str>,
}

#[derive(Deserialize)]
struct ServerDocument<'a> {
    #[serde(borrow)]
    issuer: Option<&'a str>,
    #[serde(borrow)]
    authorization_endpoint: Option<&'a str>,
    #[serde(borrow)]
    token_endpoint: Option<&'a str>,
    #[serde(borrow)]
    registration_endpoint: Option<&'a str>,
    #[serde(borrow, default)]
    code_challenge_methods_supported: Option<Vec<&'a str>>,
    #[serde(borrow, default)]
    scopes_supported: Vec<&'a str>,
    #[serde(default)]
    client_id_metadata_document_supported: bool,
    #[serde(default)]
    authorization_response_iss_parameter_supported: bool,
}

/// `url` with the scheme and the host lowercased, no fragment, and no
/// trailing slash unless the path is only `/`.
pub fn canonical(url: &str) -> String {
    let (prefix, rest) = url.split_once("://").map_or_else(
        || (String::new(), url),
        |(scheme, rest)| (format!("{}://", scheme.to_ascii_lowercase()), rest),
    );
    let rest = rest.split('#').next().unwrap_or_default();
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    let mut text = prefix;
    text.push_str(&authority.to_ascii_lowercase());
    text.push_str(without_trailing_slashes(path));
    if !query.is_empty() {
        text.push('?');
        text.push_str(query);
    }
    text
}

/// `path` without its trailing slashes, or `/` when it is only slashes.
fn without_trailing_slashes(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.is_empty() && path.starts_with('/') {
        true => "/",
        false => trimmed,
    }
}

/// The scheme, host and port of `url`: what an error may name, never the path
/// or the query.
pub fn origin(url: &str) -> &str {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.split(['/', '?', '#']).next().unwrap_or_default();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    &url[..scheme.len() + "://".len() + end]
}

/// Whether `url` may carry a login: `https` anywhere, `http` on this machine.
fn safe(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest[..end].rsplit('@').next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    if scheme.eq_ignore_ascii_case("https") {
        return true;
    }
    scheme.eq_ignore_ascii_case("http")
        && ["127.0.0.1", "::1", "localhost"]
            .iter()
            .any(|name| host.eq_ignore_ascii_case(name))
}

/// Checks that the endpoint in `field` of `issuer` may carry a login.
fn endpoint(issuer: &str, field: &str, url: &str) -> Result<(), String> {
    if safe(url) {
        return Ok(());
    }
    Err(format!(
        "{issuer}: the {field} {} is neither https nor on this machine",
        origin(url)
    ))
}

/// The origin of `url` and its path, without the query and the fragment.
fn origin_path(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() {
        return None;
    }
    let tail = rest[end..].split('#').next().unwrap_or_default();
    let path = tail.split('?').next().unwrap_or_default();
    Some((&url[..scheme.len() + "://".len() + authority.len()], path))
}

/// The URLs the protected resource metadata of `url` may be at, the most
/// specific first.
pub fn resource_urls(url: &str) -> Vec<String> {
    let Some((origin, path)) = origin_path(url) else {
        return Vec::new();
    };
    let path = path.trim_end_matches('/');
    let mut urls = Vec::with_capacity(2);
    if !path.is_empty() {
        urls.push(format!("{origin}{RESOURCE_PATH}{path}"));
    }
    urls.push(format!("{origin}{RESOURCE_PATH}"));
    urls
}

/// The URLs the metadata of `issuer` may be at, the most specific first.
pub fn server_urls(issuer: &str) -> Vec<String> {
    let Some((origin, path)) = origin_path(issuer) else {
        return Vec::new();
    };
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return vec![
            format!("{origin}{SERVER_PATH}"),
            format!("{origin}{OPENID_PATH}"),
        ];
    }
    vec![
        format!("{origin}{SERVER_PATH}{path}"),
        format!("{origin}{OPENID_PATH}{path}"),
        format!("{origin}{path}{OPENID_PATH}"),
    ]
}

/// The protected resource metadata at the first of `urls` that has one.
pub async fn resource(urls: &[String]) -> Result<Resource, String> {
    let mut last = "no protected resource metadata was found".to_string();
    for url in urls {
        let text = match get(url).await {
            Ok(text) => text,
            Err(error) => {
                last = error;
                continue;
            }
        };
        let Ok(document) = serde_json::from_str::<ResourceDocument<'_>>(&text) else {
            last = format!("{}: not protected resource metadata", origin(url));
            continue;
        };
        return Ok(Resource {
            resource: document.resource.map(str::to_owned),
            authorization_servers: document
                .authorization_servers
                .into_iter()
                .map(str::to_owned)
                .collect(),
            scopes_supported: document
                .scopes_supported
                .into_iter()
                .map(str::to_owned)
                .collect(),
        });
    }
    Err(last)
}

/// The metadata of `issuer`, whose own `issuer` must equal it.
///
/// A server that does not accept the PKCE S256 method is refused: a login
/// never falls back to a weaker challenge.
pub async fn server(issuer: &str) -> Result<Server, String> {
    for url in server_urls(issuer) {
        let Ok(text) = get(&url).await else {
            continue;
        };
        let Ok(document) = serde_json::from_str::<ServerDocument<'_>>(&text) else {
            continue;
        };
        if document.issuer != Some(issuer) {
            continue;
        }
        // A server that does not advertise PKCE is refused as one that does
        // not advertise S256: a login never falls back to a weaker challenge.
        let methods = document
            .code_challenge_methods_supported
            .as_deref()
            .unwrap_or_default();
        if !methods.contains(&S256) {
            return Err(format!("{issuer} does not accept the PKCE S256 method"));
        }
        let (Some(authorization_endpoint), Some(token_endpoint)) =
            (document.authorization_endpoint, document.token_endpoint)
        else {
            continue;
        };
        endpoint(issuer, "authorization_endpoint", authorization_endpoint)?;
        endpoint(issuer, "token_endpoint", token_endpoint)?;
        if let Some(registration) = document.registration_endpoint {
            endpoint(issuer, "registration_endpoint", registration)?;
        }
        return Ok(Server {
            issuer: issuer.to_string(),
            authorization_endpoint: authorization_endpoint.to_string(),
            token_endpoint: token_endpoint.to_string(),
            registration_endpoint: document.registration_endpoint.map(str::to_owned),
            scopes_supported: document
                .scopes_supported
                .into_iter()
                .map(str::to_owned)
                .collect(),
            client_metadata_document: document.client_id_metadata_document_supported,
            iss_parameter: document.authorization_response_iss_parameter_supported,
        });
    }
    Err(format!(
        "no authorization server metadata was found for {issuer}"
    ))
}

/// The body of one metadata document, or why it could not be read.
async fn get(url: &str) -> Result<String, String> {
    let request = crate::http::client()
        .get(url)
        .header("accept", "application/json");
    let place = origin(url);
    let response = tokio::time::timeout(TIMEOUT, request.send())
        .await
        .map_err(|_| format!("{place} did not answer"))?
        .map_err(|error| format!("{place}: {}", error.without_url()))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("{place}: HTTP {status}"));
    }
    response
        .text()
        .await
        .map_err(|_| format!("{place}: the body could not be read"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_resource_urls_try_the_path_first_then_the_root() {
        assert_eq!(
            resource_urls("https://a/mcp"),
            vec![
                "https://a/.well-known/oauth-protected-resource/mcp".to_string(),
                "https://a/.well-known/oauth-protected-resource".to_string(),
            ]
        );
        assert_eq!(
            resource_urls("https://a/mcp/v1/"),
            vec![
                "https://a/.well-known/oauth-protected-resource/mcp/v1".to_string(),
                "https://a/.well-known/oauth-protected-resource".to_string(),
            ]
        );
        for url in ["https://a", "https://a/", "https://a/?x=1"] {
            assert_eq!(
                resource_urls(url),
                vec!["https://a/.well-known/oauth-protected-resource".to_string()],
                "{url}"
            );
        }
        assert!(resource_urls("nonsense").is_empty());
    }

    #[test]
    fn oauth_server_urls_follow_the_issuer_path() {
        assert_eq!(
            server_urls("https://a/t1"),
            vec![
                "https://a/.well-known/oauth-authorization-server/t1".to_string(),
                "https://a/.well-known/openid-configuration/t1".to_string(),
                "https://a/t1/.well-known/openid-configuration".to_string(),
            ]
        );
        assert_eq!(
            server_urls("https://a"),
            vec![
                "https://a/.well-known/oauth-authorization-server".to_string(),
                "https://a/.well-known/openid-configuration".to_string(),
            ]
        );
    }

    #[test]
    fn oauth_canonical_lowercases_the_host_and_trims_the_slash() {
        assert_eq!(canonical("HTTPS://A.example/mcp/"), "https://a.example/mcp");
        assert_eq!(
            canonical("HTTPS://A.example:8443/MCP?x=1#frag"),
            "https://a.example:8443/MCP?x=1"
        );
        assert_eq!(canonical("https://a/"), "https://a/");
        assert_eq!(canonical("https://a"), "https://a");
        assert_eq!(canonical("https://a?x=1"), "https://a?x=1");
    }
}
