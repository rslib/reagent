//! The OAuth login of an HTTP MCP server: discovery, registration, PKCE, the
//! callback, the saved token, its refresh and its logout.

mod common;

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::http::{Oauth, Server, Settings, tool};
use reagent_config::Secret;
use reagent_mcp::auth::{self, AuthStore, ServerAuth, Store, Token};
use reagent_mcp::{Client, HttpSpec};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// How long a login or a call may take in a test.
const WAIT: Duration = Duration::from_secs(10);

/// The path of the protected resource metadata without the resource path.
const RESOURCE_PATH: &str = "/.well-known/oauth-protected-resource";

/// The seconds since the epoch, or 0 before 1970.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The login settings of the server at `url`, saved as `srv`.
fn settings(url: &str) -> ServerAuth {
    ServerAuth {
        name: "srv".to_string(),
        url: url.to_string(),
        ..ServerAuth::default()
    }
}

/// How the transport reaches `server`, with the saved logins of `dir`.
fn spec(server: &Server, dir: &Path) -> HttpSpec {
    HttpSpec {
        url: server.url.clone(),
        headers: Vec::new(),
        auth: Some(AuthStore {
            name: "srv".to_string(),
            dir: dir.to_path_buf(),
        }),
    }
}

/// A browser for the login: it opens the authorize URL and follows the
/// redirect it is sent to the loopback callback.
fn browser() -> impl Fn(&str) {
    |url: &str| {
        let url = url.to_string();
        tokio::spawn(async move {
            let http = reagent_oauth::http::client();
            let Ok(response) = http.get(&url).send().await else {
                return;
            };
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            if let Some(location) = location {
                let _ = http.get(&location).send().await;
            }
        });
    }
}

/// Runs one login, failing the test when it hangs.
async fn login(server: &ServerAuth, dir: &Path) -> Result<(), String> {
    tokio::time::timeout(WAIT, auth::login(server, dir, browser()))
        .await
        .map_err(|_| "the login did not finish".to_string())?
        .map_err(|error| error.to_string())
}

/// The permission bits of `path`.
#[cfg(unix)]
#[expect(clippy::disallowed_methods, reason = "a test reads its own file")]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .expect("stats the file")
        .permissions()
        .mode()
        & 0o777
}

/// A saved login with the values the tests compare against.
fn token(resource: &str) -> Token {
    Token {
        client_id: "dcr-client".to_string(),
        client_secret: None,
        access: Secret::from("access-1"),
        refresh: Some(Secret::from("refresh-1")),
        expires_at: now() + 3600,
        scopes: vec!["files.read".to_string()],
        needed: Vec::new(),
        resource: resource.to_string(),
        redirect_uri: String::new(),
    }
}

#[tokio::test]
async fn oauth_login_and_call() {
    let server = Server::start(Settings {
        oauth: Some(Oauth::default()),
        tools: vec![tool("echo", json!({"type": "object"}))],
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    login(&settings(&server.url), dir.path())
        .await
        .expect("the login works");

    let registration = server
        .requests()
        .into_iter()
        .find(|request| request.path == "/register")
        .expect("a registration");
    let body: Value = serde_json::from_str(&registration.body).expect("the body is JSON");
    assert_eq!(body["application_type"], "native", "{body}");
    assert_eq!(body["token_endpoint_auth_method"], "none", "{body}");
    assert_eq!(
        body["grant_types"],
        json!(["authorization_code", "refresh_token"]),
        "{body}"
    );
    assert_eq!(body["response_types"], json!(["code"]), "{body}");
    assert!(
        body["redirect_uris"][0]
            .as_str()
            .expect("a redirect")
            .starts_with("http://127.0.0.1:"),
        "{body}"
    );

    let challenge = server
        .pairs("/authorize", "code_challenge")
        .pop()
        .expect("a code challenge");
    let verifier = server
        .pairs("/token", "code_verifier")
        .pop()
        .expect("a code verifier");
    assert_eq!(reagent_oauth::challenge(&verifier), challenge);
    assert_eq!(
        server.pairs("/authorize", "code_challenge_method").pop(),
        Some("S256".to_string())
    );
    for path in ["/authorize", "/token"] {
        assert_eq!(
            server.pairs(path, "resource").pop().as_deref(),
            Some(server.url.as_str()),
            "the resource of {path}"
        );
    }
    assert_eq!(
        server.pairs("/token", "grant_type").pop().as_deref(),
        Some("authorization_code")
    );

    #[cfg(unix)]
    assert_eq!(mode(&auth::path(dir.path())), 0o600);
    let store = auth::read(dir.path()).expect("reads the store");
    let saved = store.token("srv", &server.origin).expect("a token");
    let access = saved.access.expose().to_string();
    assert!(access.starts_with("access-"), "{access}");
    assert_eq!(
        saved.refresh.as_ref().map(Secret::expose),
        Some("refresh-1")
    );

    let client = Client::http(&spec(&server, dir.path()))
        .await
        .expect("connects");
    let output = client
        .call("echo", "{}", &CancellationToken::new())
        .await
        .expect("calls echo");
    assert_eq!(output.text, "the echo tool ran");
    let call = server.call("tools/call").expect("the call");
    assert_eq!(
        call.header("authorization"),
        Some(format!("Bearer {access}").as_str())
    );
}

#[tokio::test]
async fn oauth_prm_well_known_fallback() {
    let server = Server::start(Settings {
        oauth: Some(Oauth {
            announce: false,
            ..Oauth::default()
        }),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    login(&settings(&server.url), dir.path())
        .await
        .expect("the login works");

    let paths = server.paths();
    assert!(
        paths
            .iter()
            .any(|path| path == &format!("{RESOURCE_PATH}/mcp")),
        "the path aware URL was not tried: {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| path == RESOURCE_PATH),
        "the root URL was tried first: {paths:?}"
    );
}

#[tokio::test]
async fn oauth_as_metadata_issuer_must_match() {
    let wrong = json!({
        "issuer": "{origin}",
        "authorization_endpoint": "{origin}/wrong/authorize",
        "token_endpoint": "{origin}/wrong/token",
    })
    .to_string();
    let server = Server::start(Settings {
        oauth: Some(Oauth {
            issuer: "{origin}/t1".to_string(),
            extra: vec![(
                "/.well-known/oauth-authorization-server/t1".to_string(),
                wrong,
            )],
            ..Oauth::default()
        }),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    login(&settings(&server.url), dir.path())
        .await
        .expect("the login works");

    let paths = server.paths();
    assert!(
        paths
            .iter()
            .any(|path| path == "/.well-known/openid-configuration/t1"),
        "the next candidate was not tried: {paths:?}"
    );
    assert!(
        server
            .requests()
            .iter()
            .all(|request| !request.path.starts_with("/wrong/")),
        "the document with the wrong issuer was used"
    );
    let store = auth::read(dir.path()).expect("reads the store");
    assert!(
        store
            .token("srv", &format!("{}/t1", server.origin))
            .is_some()
    );
}

#[tokio::test]
async fn oauth_refresh_before_expiry() {
    let server = Server::start(Settings {
        oauth: Some(Oauth::default()),
        tools: vec![tool("echo", json!({"type": "object"}))],
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let mut store = Store::default();
    let mut stale = token(&server.url);
    stale.access = Secret::from("old-access");
    stale.refresh = Some(Secret::from("old-refresh"));
    stale.expires_at = now() + 10;
    store.put("srv", &server.origin, stale);
    auth::write(dir.path(), &store).expect("writes the store");

    let client = Client::http(&spec(&server, dir.path()))
        .await
        .expect("connects");
    client
        .call("echo", "{}", &CancellationToken::new())
        .await
        .expect("calls echo");

    // The refresh comes before the call, so no request is ever refused.
    assert_eq!(
        server.calls("server/discover").len(),
        1,
        "the first request was refused: {:?}",
        server.paths()
    );
    assert_eq!(server.calls("tools/call").len(), 1);
    assert_eq!(
        server
            .paths()
            .iter()
            .filter(|path| *path == "/token")
            .count(),
        1
    );

    let saved = auth::read(dir.path()).expect("reads the store");
    let fresh = saved.token("srv", &server.origin).expect("a token");
    assert_ne!(fresh.access.expose(), "old-access");
    assert_eq!(
        fresh.refresh.as_ref().map(Secret::expose),
        Some("old-refresh"),
        "the old refresh token is kept"
    );
    assert_eq!(
        server.pairs("/token", "grant_type").pop().as_deref(),
        Some("refresh_token")
    );
    assert_eq!(
        server.pairs("/token", "refresh_token").pop().as_deref(),
        Some("old-refresh")
    );
    assert_eq!(
        server.pairs("/token", "client_id").pop().as_deref(),
        Some("dcr-client")
    );
    assert_eq!(
        server.pairs("/token", "resource").pop().as_deref(),
        Some(server.url.as_str())
    );
    let call = server.call("tools/call").expect("the call");
    assert_eq!(
        call.header("authorization"),
        Some(format!("Bearer {}", fresh.access.expose()).as_str())
    );
}

#[tokio::test]
async fn oauth_iss_mismatch_is_refused() {
    let server = Server::start(Settings {
        oauth: Some(Oauth {
            iss: Some("{origin}/elsewhere".to_string()),
            ..Oauth::default()
        }),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let error = login(&settings(&server.url), dir.path())
        .await
        .expect_err("the iss does not match");
    assert!(error.contains("iss"), "{error}");
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.path != "/token"),
        "a token was bought"
    );
    let store = auth::read(dir.path()).expect("reads the store");
    assert!(store.token("srv", &server.origin).is_none());
}

#[tokio::test]
async fn oauth_token_from_other_issuer_is_not_sent() {
    let server = Server::start(Settings {
        oauth: Some(Oauth::default()),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let mut store = Store::default();
    store.put(
        "srv",
        &format!("{}/other", server.origin),
        token(&server.url),
    );
    auth::write(dir.path(), &store).expect("writes the store");

    let error = Client::http(&spec(&server, dir.path()))
        .await
        .expect_err("the issuer changed")
        .to_string();
    assert!(error.contains("run: reagent mcp login srv"), "{error}");
    assert!(
        !server.paths().iter().any(|path| path == "/mcp"),
        "a request carried the token: {:?}",
        server.paths()
    );
}

#[tokio::test]
async fn oauth_headless_call_without_login() {
    let server = Server::start(Settings {
        oauth: Some(Oauth::default()),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let error = Client::http(&spec(&server, dir.path()))
        .await
        .expect_err("no login is saved")
        .to_string();
    assert!(error.contains("run: reagent mcp login srv"), "{error}");
    let probe = server.call("server/discover").expect("the probe");
    assert!(probe.header("authorization").is_none(), "{probe:?}");
}

#[tokio::test]
async fn oauth_pre_registered_client() {
    let server = Server::start(Settings {
        oauth: Some(Oauth::default()),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let mut registered = settings(&server.url);
    registered.client_id = Some("pre-registered".to_string());
    login(&registered, dir.path())
        .await
        .expect("the login works");

    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.path != "/register"),
        "a client was registered anyway"
    );
    assert_eq!(
        server.pairs("/authorize", "client_id").pop().as_deref(),
        Some("pre-registered")
    );
    assert_eq!(
        server.pairs("/token", "client_id").pop().as_deref(),
        Some("pre-registered")
    );
    let store = auth::read(dir.path()).expect("reads the store");
    assert!(store.token("srv", &server.origin).is_some());
}

#[tokio::test]
async fn oauth_no_s256_is_refused() {
    let server = Server::start(Settings {
        oauth: Some(Oauth {
            methods: vec!["plain".to_string()],
            ..Oauth::default()
        }),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let error = login(&settings(&server.url), dir.path())
        .await
        .expect_err("PKCE S256 is not offered");
    assert!(error.contains("S256"), "{error}");
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.path != "/token"),
        "a token was bought"
    );
}

#[tokio::test]
async fn oauth_step_up_asks_for_more_scopes() {
    let server = Server::start(Settings {
        oauth: Some(Oauth {
            step_up: Some("files.write".to_string()),
            ..Oauth::default()
        }),
        tools: vec![tool("echo", json!({"type": "object"}))],
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    login(&settings(&server.url), dir.path())
        .await
        .expect("the login works");

    let client = Client::http(&spec(&server, dir.path()))
        .await
        .expect("connects");
    let error = client
        .call("echo", "{}", &CancellationToken::new())
        .await
        .expect_err("the call needs a wider scope")
        .to_string();
    assert!(error.contains("run: reagent mcp login srv"), "{error}");
    assert!(error.contains("files.write"), "{error}");

    let store = auth::read(dir.path()).expect("reads the store");
    let needed = &store.token("srv", &server.origin).expect("a token").needed;
    assert_eq!(needed, &vec!["files.write".to_string()]);

    login(&settings(&server.url), dir.path())
        .await
        .expect("the login works again");
    let scope = server
        .pairs("/authorize", "scope")
        .pop()
        .expect("a requested scope");
    for wanted in ["files.read", "files.write", "offline_access"] {
        assert!(
            scope.split(' ').any(|one| one == wanted),
            "{wanted} is missing from {scope}"
        );
    }
}

#[tokio::test]
async fn oauth_logout_removes_tokens() {
    let dir = tempfile::tempdir().expect("a temp folder");
    let mut store = Store::default();
    store.put("srv", "https://a", token("https://a/mcp"));
    store.put("srv", "https://b", token("https://b/mcp"));
    store.put("other", "https://a", token("https://a/mcp"));
    auth::write(dir.path(), &store).expect("writes the store");

    assert!(auth::logout("srv", dir.path()).expect("logs out"));
    let store = auth::read(dir.path()).expect("reads the store");
    assert!(!store.has("srv"));
    assert!(store.issuers("srv").is_empty());
    assert!(store.token("other", "https://a").is_some());
    assert!(!auth::logout("srv", dir.path()).expect("logs out"));
}

#[tokio::test]
async fn oauth_client_metadata_document_is_the_client_id() {
    let server = Server::start(Settings {
        oauth: Some(Oauth {
            client_metadata: true,
            ..Oauth::default()
        }),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let mut registered = settings(&server.url);
    registered.client_metadata_url = Some("https://reagent.example/client.json".to_string());
    login(&registered, dir.path())
        .await
        .expect("the login works");

    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.path != "/register"),
        "a client was registered anyway"
    );
    assert_eq!(
        server.pairs("/authorize", "client_id").pop().as_deref(),
        Some("https://reagent.example/client.json")
    );
}

#[tokio::test]
async fn oauth_no_registration_needs_a_client_id() {
    let server = Server::start(Settings {
        oauth: Some(Oauth {
            registration: false,
            ..Oauth::default()
        }),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let error = login(&settings(&server.url), dir.path())
        .await
        .expect_err("no client can be registered");
    assert!(error.contains("client_id"), "{error}");
    assert!(
        !server.paths().iter().any(|path| path == "/authorize"),
        "the browser was opened for nothing"
    );
}

/// How many token requests the server answered.
fn token_requests(server: &Server) -> usize {
    server
        .requests()
        .iter()
        .filter(|request| request.path == "/token")
        .count()
}

#[tokio::test]
async fn oauth_prm_resource_must_match_the_url() {
    let protected = json!({
        "resource": "https://other.example/mcp",
        "authorization_servers": ["{origin}"],
    })
    .to_string();
    let server = Server::start(Settings {
        oauth: Some(Oauth {
            extra: vec![(format!("{RESOURCE_PATH}/mcp"), protected)],
            ..Oauth::default()
        }),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let error = login(&settings(&server.url), dir.path())
        .await
        .expect_err("the metadata names another resource");
    assert!(error.contains("https://other.example/mcp"), "{error}");
    assert!(error.contains(&server.url), "{error}");
    assert_eq!(token_requests(&server), 0, "a token was bought");
    assert!(
        !server.paths().iter().any(|path| path == "/authorize"),
        "the browser was opened for nothing"
    );
}

#[tokio::test]
async fn oauth_token_of_another_resource_is_not_sent() {
    let server = Server::start(Settings {
        oauth: Some(Oauth::default()),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let mut store = Store::default();
    store.put("srv", &server.origin, token("https://a/mcp"));
    auth::write(dir.path(), &store).expect("writes the store");

    let error = Client::http(&spec(&server, dir.path()))
        .await
        .expect_err("the token belongs to another URL")
        .to_string();
    assert!(error.contains("run: reagent mcp login srv"), "{error}");
    assert!(
        !server.paths().iter().any(|path| path == "/mcp"),
        "a request carried the token: {:?}",
        server.paths()
    );
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.header("authorization").is_none()),
        "a request carried an authorization header"
    );
}

#[tokio::test]
async fn oauth_no_code_challenge_methods_is_refused() {
    let server = Server::start(Settings {
        oauth: Some(Oauth {
            methods: Vec::new(),
            ..Oauth::default()
        }),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let error = login(&settings(&server.url), dir.path())
        .await
        .expect_err("PKCE is not advertised");
    assert!(error.contains("S256"), "{error}");
    assert_eq!(token_requests(&server), 0, "a token was bought");
}

#[tokio::test]
async fn oauth_endpoint_with_a_foreign_scheme_is_refused() {
    let document = json!({
        "issuer": "{origin}",
        "authorization_endpoint": "smb://host/x",
        "token_endpoint": "{origin}/token",
        "code_challenge_methods_supported": ["S256"],
    })
    .to_string();
    let server = Server::start(Settings {
        oauth: Some(Oauth {
            extra: vec![(
                "/.well-known/oauth-authorization-server".to_string(),
                document,
            )],
            ..Oauth::default()
        }),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let error = login(&settings(&server.url), dir.path())
        .await
        .expect_err("the authorization endpoint is not safe");
    assert!(error.contains("authorization_endpoint"), "{error}");
    assert!(
        !server.paths().iter().any(|path| path == "/authorize"),
        "the browser was opened for nothing"
    );
}

#[tokio::test]
async fn oauth_eight_logins_save_at_once() {
    let server = Server::start(Settings {
        oauth: Some(Oauth::default()),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    const SERVERS: usize = 8;
    let mut store = Store::default();
    for index in 0..SERVERS {
        store.put(
            &format!("srv-{index}"),
            &server.origin,
            Token {
                expires_at: now(),
                ..token(&server.url)
            },
        );
    }
    auth::write(dir.path(), &store).expect("writes the store");

    let mut tasks = Vec::with_capacity(SERVERS);
    for index in 0..SERVERS {
        let spec = HttpSpec {
            url: server.url.clone(),
            headers: Vec::new(),
            auth: Some(AuthStore {
                name: format!("srv-{index}"),
                dir: dir.path().to_path_buf(),
            }),
        };
        tasks.push(tokio::spawn(async move {
            Client::http(&spec).await.map(|_client| ())
        }));
    }
    for task in tasks {
        task.await.expect("a task").expect("the refresh works");
    }

    let store = auth::read(dir.path()).expect("reads the store");
    let mut accesses = Vec::with_capacity(SERVERS);
    for index in 0..SERVERS {
        let name = format!("srv-{index}");
        let saved = store
            .token(&name, &server.origin)
            .unwrap_or_else(|| panic!("no token for {name}"));
        accesses.push(saved.access.expose().to_string());
    }
    accesses.sort();
    accesses.dedup();
    assert_eq!(accesses.len(), SERVERS, "two servers ended with one token");
    assert_eq!(token_requests(&server), SERVERS, "a refresh was lost");
}

#[tokio::test]
async fn oauth_two_calls_share_one_refresh() {
    let server = Server::start(Settings {
        oauth: Some(Oauth {
            revoke_after: Some(1),
            ..Oauth::default()
        }),
        tools: vec![tool("echo", json!({"type": "object"}))],
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let mut store = Store::default();
    store.put(
        "srv",
        &server.origin,
        Token {
            expires_at: now(),
            ..token(&server.url)
        },
    );
    auth::write(dir.path(), &store).expect("writes the store");

    let client = Client::http(&spec(&server, dir.path()))
        .await
        .expect("connects");
    let before = token_requests(&server);
    let cancel = CancellationToken::new();
    let (first, second) = tokio::join!(
        client.call("echo", "{}", &cancel),
        client.call("echo", "{}", &cancel),
    );
    let first = first.expect("the first call");
    let second = second.expect("the second call");
    assert_eq!(first.text, "the echo tool ran");
    assert_eq!(second.text, "the echo tool ran");
    assert_eq!(
        token_requests(&server) - before,
        1,
        "the two calls bought more than one token"
    );
}

#[tokio::test]
async fn oauth_second_login_reuses_client() {
    let server = Server::start(Settings {
        oauth: Some(Oauth::default()),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let server_auth = settings(&server.url);
    login(&server_auth, dir.path())
        .await
        .expect("the first login works");
    assert_eq!(server.registrations(), 1, "the first login registered");

    login(&server_auth, dir.path())
        .await
        .expect("the second login works");
    assert_eq!(
        server.registrations(),
        1,
        "the second login registered again"
    );

    let store = auth::read(dir.path()).expect("reads the store");
    let saved = store.token("srv", &server.origin).expect("a token");
    assert_eq!(
        server.pairs("/token", "client_id").pop().as_deref(),
        Some(saved.client_id.as_str())
    );
}

#[tokio::test]
async fn oauth_reused_client_refused_registers_next_time() {
    let server = Server::start(Settings {
        oauth: Some(Oauth {
            invalid_client: true,
            ..Oauth::default()
        }),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().expect("a temp folder");
    let server_auth = settings(&server.url);
    login(&server_auth, dir.path())
        .await
        .expect("the first login works");

    let error = login(&server_auth, dir.path())
        .await
        .expect_err("the saved client is refused");
    assert!(
        error.contains("no longer knows the saved client of srv"),
        "{error}"
    );
    assert!(error.contains("run reagent mcp login srv"), "{error}");
    let store = auth::read(dir.path()).expect("reads the store");
    assert_eq!(
        store
            .token("srv", &server.origin)
            .expect("the saved token")
            .redirect_uri,
        "",
        "the refused client was kept"
    );

    login(&server_auth, dir.path())
        .await
        .expect("the third login works");
    assert_eq!(
        server.registrations(),
        2,
        "the third login did not register again"
    );
}
