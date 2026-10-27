//! The OAuth login of an HTTP MCP server: the protected resource it names, the
//! authorization server behind it, the client, the browser callback, and the
//! token the transport sends and refreshes.
//!
//! No token ever reaches a `Debug` output or an error text: the tokens are
//! `Secret`, and a failure names the endpoint and the status only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reagent_config::Secret;
use reagent_oauth::http::client;
use reagent_oauth::loopback::{field, wait_for_query};
use reagent_oauth::metadata;
use reagent_oauth::{auth_params, percent_encode, pkce, random_token};
use reqwest::header::{ACCEPT, CONTENT_TYPE, WWW_AUTHENTICATE};
use serde::Deserialize;
use tokio::net::TcpListener;

use crate::error::Error;

/// How long before its expiry a token counts as spent and is refreshed.
const EXPIRY_MARGIN: u64 = 60;

/// The lifetime a token gets when its reply names none.
const DEFAULT_LIFETIME: u64 = 3600;

/// The path of the callback the redirect URI names.
const CALLBACK_PATH: &str = "/callback";

/// The error code a token endpoint answers with when it forgot the client.
const INVALID_CLIENT: &str = "invalid_client";

/// The client name a dynamic registration asks for.
const CLIENT_NAME: &str = "reagent";

/// The scope that buys a refresh token.
const OFFLINE: &str = "offline_access";

/// The content type of a form request.
const FORM: &str = "application/x-www-form-urlencoded";

/// The content type of a JSON request.
const JSON: &str = "application/json";

/// How long a login waits for the browser or a pasted URL.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// How long one request to an authorization server may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The request that asks a server for its era, which a `401` answers with the
/// challenge of a login.
const DISCOVER: &str = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"server/discover\"}\n";

/// The name of the file the logins are saved in.
const FILE: &str = "mcp-auth.json";

/// The file two runs take turns on while one writes the logins.
const LOCK: &str = "mcp-auth.json.lock";

/// How long one try at the lock waits before the next try.
const LOCK_RETRY: Duration = Duration::from_millis(20);

/// How long a run waits for another run's write before it gives up.
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Counts the writes of this process, so two never share a temp file.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// One HTTP MCP server's login settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerAuth {
    /// The name the token is saved and reported under.
    pub name: String,
    /// The MCP endpoint URL.
    pub url: String,
    /// A client id registered by hand, which skips registration.
    pub client_id: Option<String>,
    /// The secret of that client, when it has one.
    pub client_secret: Option<Secret>,
    /// A client id metadata document, which is itself the `client_id`.
    pub client_metadata_url: Option<String>,
    /// The port the callback listens on; any free one when absent.
    pub callback_port: Option<u16>,
    /// The scopes to ask for, in place of the resource's own.
    pub scope: Option<String>,
}

/// Where one server's login is saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthStore {
    /// The server name the token is keyed by.
    pub name: String,
    /// The state folder holding the file.
    pub dir: PathBuf,
}

/// One saved login: the client and the tokens of one server and issuer.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    /// The client id the token was issued to.
    pub client_id: String,
    /// The secret of that client, when it has one.
    pub client_secret: Option<Secret>,
    /// The bearer every request carries.
    pub access: Secret,
    /// The token that buys the next access token.
    pub refresh: Option<Secret>,
    /// The time the access token stops being usable, since the epoch.
    pub expires_at: u64,
    /// The scopes the token covers.
    pub scopes: Vec<String>,
    /// The scopes a step-up asked for, which a later login must include.
    pub needed: Vec<String>,
    /// The canonical URL of the server the token is for.
    pub resource: String,
    /// The callback URL a dynamic registration was made for; empty for a
    /// pre-registered or metadata-document client.
    pub redirect_uri: String,
}

/// The saved logins, by server name and then by issuer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Store(BTreeMap<String, BTreeMap<String, Token>>);

impl Store {
    /// The token of `server` at `issuer`.
    pub fn token(&self, server: &str, issuer: &str) -> Option<&Token> {
        self.0.get(server)?.get(issuer)
    }

    /// Saves `token` under `server` and `issuer`, replacing an earlier one.
    pub fn put(&mut self, server: &str, issuer: &str, token: Token) {
        self.0
            .entry(server.to_string())
            .or_default()
            .insert(issuer.to_string(), token);
    }

    /// The issuers `server` has a login at.
    pub fn issuers(&self, server: &str) -> Vec<&str> {
        self.0
            .get(server)
            .map(|issuers| issuers.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }

    /// Whether `server` has any login.
    pub fn has(&self, server: &str) -> bool {
        self.0.contains_key(server)
    }

    /// Drops every login of `server`; true when one was there.
    pub fn remove(&mut self, server: &str) -> bool {
        self.0.remove(server).is_some()
    }

    /// Whether no login is saved.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The file of `dir` the logins are saved in.
pub fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// Reads the logins of `dir`; a missing file holds none.
///
/// A file readable by others is refused, and an entry this version cannot read
/// is skipped, so a login written by a newer reagent never costs the others.
#[expect(
    clippy::disallowed_types,
    reason = "the login file is written the way the auth file is: temp file, 0600, rename"
)]
pub fn read(dir: &Path) -> Result<Store, Error> {
    use std::io::Read as _;

    let path = path(dir);
    let fail = |error: std::io::Error| failed(&path, &error);
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(error) => return Err(fail(error)),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata().map_err(fail)?.permissions().mode() & 0o077 != 0 {
            return Err(Error::Auth(format!(
                "{0}: readable by others; run chmod 600 {0}",
                path.display()
            )));
        }
    }
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(fail)?;
    parse(&path, &text)
}

/// Writes `store` to the file of `dir`, readable by you only.
pub fn write(dir: &Path, store: &Store) -> Result<(), Error> {
    let _lock = Lock::wait(dir)?;
    write_locked(dir, store)
}

/// Writes `store` to the file of `dir`; the caller holds the lock.
#[expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "the login file is written the way the auth file is: temp file, 0600, rename"
)]
fn write_locked(dir: &Path, store: &Store) -> Result<(), Error> {
    use std::io::Write as _;

    let path = path(dir);
    let text = serialize(&path, store)?;
    ensure(dir)?;
    let temp = dir.join(format!(
        "{FILE}.tmp-{}-{}",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp).map_err(|error| failed(&temp, &error))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| failed(&path, &error))?;
    drop(file);
    std::fs::rename(&temp, &path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        failed(&path, &error)
    })
}

/// Makes `dir`, readable by you only, when it is not there yet.
fn ensure(dir: &Path) -> Result<(), Error> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir).map_err(|error| failed(dir, &error))
}

/// An exclusive lock on the login file, released when it drops.
#[expect(
    clippy::disallowed_types,
    reason = "the lock of the login file is held by a run that writes it"
)]
struct Lock(std::fs::File);

impl Lock {
    /// Waits for the lock of `dir`, which another run may hold while it writes.
    #[expect(
        clippy::disallowed_types,
        clippy::disallowed_methods,
        reason = "the wait runs on the blocking pool, off the async thread"
    )]
    fn wait(dir: &Path) -> Result<Lock, Error> {
        use std::fs::TryLockError;

        ensure(dir)?;
        let path = dir.join(LOCK);
        let mut options = std::fs::OpenOptions::new();
        let file = options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| failed(&path, &error))?;
        let mut waited = Duration::ZERO;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Lock(file)),
                Err(TryLockError::WouldBlock) if waited < LOCK_TIMEOUT => {
                    std::thread::sleep(LOCK_RETRY);
                    waited += LOCK_RETRY;
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(Error::Auth(format!(
                        "{}: another run holds the login file",
                        path.display()
                    )));
                }
                Err(TryLockError::Error(error)) => return Err(failed(&path, &error)),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// One path and why it could not be read or written.
fn failed(path: &Path, error: &std::io::Error) -> Error {
    Error::Auth(format!("{}: {error}", path.display()))
}

/// The logins of `text`, with the entries this version cannot read left out.
///
/// The error names the file and the place of the bad byte, never the text: it
/// holds tokens.
fn parse(path: &Path, text: &str) -> Result<Store, Error> {
    let entries: BTreeMap<String, BTreeMap<String, serde_json::Value>> = serde_json::from_str(text)
        .map_err(|error| {
            Error::Auth(format!(
                "{}: not valid JSON at line {} column {}",
                path.display(),
                error.line(),
                error.column()
            ))
        })?;
    let mut store = Store::default();
    for (server, issuers) in entries {
        for (issuer, entry) in issuers {
            if let Ok(read) = serde_json::from_value::<Read>(entry) {
                store.put(&server, &issuer, read.token());
            }
        }
    }
    Ok(store)
}

/// The whole file as JSON, one login per line.
fn serialize(path: &Path, store: &Store) -> Result<String, Error> {
    let saved: BTreeMap<&str, BTreeMap<&str, Written<'_>>> = store
        .0
        .iter()
        .map(|(server, issuers)| {
            let issuers = issuers
                .iter()
                .map(|(issuer, token)| (issuer.as_str(), Written::of(token)))
                .collect();
            (server.as_str(), issuers)
        })
        .collect();
    serde_json::to_string_pretty(&saved)
        .map(|text| text + "\n")
        .map_err(|_| Error::Auth(format!("{}: cannot write the logins", path.display())))
}

/// A token as the file writes it, its secrets borrowed for this write only.
#[derive(serde::Serialize)]
struct Written<'a> {
    client_id: &'a str,
    client_secret: Option<&'a str>,
    access: &'a str,
    refresh: Option<&'a str>,
    expires_at: u64,
    scopes: &'a [String],
    needed: &'a [String],
    resource: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    redirect_uri: &'a str,
}

impl<'a> Written<'a> {
    fn of(token: &'a Token) -> Written<'a> {
        Written {
            client_id: &token.client_id,
            client_secret: token.client_secret.as_ref().map(Secret::expose),
            access: token.access.expose(),
            refresh: token.refresh.as_ref().map(Secret::expose),
            expires_at: token.expires_at,
            scopes: &token.scopes,
            needed: &token.needed,
            resource: &token.resource,
            redirect_uri: &token.redirect_uri,
        }
    }
}

/// One token as the file reads it.
#[derive(Deserialize)]
struct Read {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
    access: String,
    #[serde(default)]
    refresh: Option<String>,
    #[serde(default)]
    expires_at: u64,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    needed: Vec<String>,
    #[serde(default)]
    resource: String,
    #[serde(default)]
    redirect_uri: String,
}

impl Read {
    fn token(self) -> Token {
        Token {
            client_id: self.client_id,
            client_secret: self.client_secret.as_deref().map(Secret::from),
            access: Secret::from(self.access.as_str()),
            refresh: self.refresh.as_deref().map(Secret::from),
            expires_at: self.expires_at,
            scopes: self.scopes,
            needed: self.needed,
            resource: self.resource,
            redirect_uri: self.redirect_uri,
        }
    }
}

/// The seconds since the epoch, or 0 before 1970.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Runs one browser login of `server` and saves its token under `store_dir`.
///
/// `open` shows the authorize URL; a login driven by a test follows it itself
/// instead of opening a browser.
pub async fn login(
    server: &ServerAuth,
    store_dir: &Path,
    open: impl Fn(&str),
) -> Result<(), Error> {
    let resource = metadata::canonical(&server.url);
    let challenge = probe(&server.url).await;
    let protected = protected_resource(&resource, challenge.as_ref()).await?;
    protected.bound_to(&resource).map_err(Error::Auth)?;
    let issuer = protected
        .authorization_servers
        .first()
        .cloned()
        .ok_or_else(|| {
            Error::Auth(format!(
                "the resource metadata of {} names no authorization server",
                server.name
            ))
        })?;
    let authorization = metadata::server(&issuer).await.map_err(Error::Auth)?;
    // Read before the browser opens: a refused file fails the login while no
    // code has been issued yet.
    let prior = read_task(store_dir)
        .await?
        .token(&server.name, &issuer)
        .cloned();
    let saved_redirect = prior
        .as_ref()
        .map(|token| token.redirect_uri.as_str())
        .filter(|uri| !uri.is_empty());
    let listener = listener(server.callback_port, saved_redirect).await?;
    let port = listener
        .local_addr()
        .map_err(|error| Error::Auth(format!("no local port for the login: {error}")))?
        .port();
    let redirect = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let reuse = prior
        .as_ref()
        .filter(|token| token.redirect_uri == redirect);
    let client = register(server, &authorization, &redirect, reuse).await?;
    let scopes = requested(
        server,
        challenge.as_ref(),
        &protected,
        &authorization,
        prior.as_ref(),
    );
    let pkce = pkce().map_err(Error::Auth)?;
    let state = random_token().map_err(Error::Auth)?;
    let url = authorize_url(
        &authorization,
        &client,
        &redirect,
        &pkce.challenge,
        &state,
        &scopes,
        &resource,
    );
    open(&url);
    let input = tokio::io::BufReader::new(tokio::io::stdin());
    let query = tokio::time::timeout(
        LOGIN_TIMEOUT,
        wait_for_query(Some(&listener), CALLBACK_PATH, true, Some(&state), input),
    )
    .await
    .map_err(|_| Error::Auth("the login timed out".to_string()))?
    .map_err(Error::Auth)?;
    check_callback(&query, &state, &authorization)?;
    let code = field(&query, "code")
        .ok_or_else(|| Error::Auth("the login callback carried no code".to_string()))?;
    let mut pairs = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect.as_str()),
        ("client_id", client.id.as_str()),
        ("code_verifier", pkce.verifier.as_str()),
        ("resource", resource.as_str()),
    ];
    if let Some(secret) = client.secret.as_ref() {
        pairs.push(("client_secret", secret.expose()));
    }
    let (status, text) = post(
        &authorization.token_endpoint,
        form(&pairs).into_bytes(),
        FORM,
    )
    .await?;
    if reuse.is_some() && refused(&text) == Some(INVALID_CLIENT) {
        if let Some(mut token) = prior.clone() {
            token.redirect_uri.clear();
            save(store_dir, &server.name, &issuer, token).await?;
        }
        return Err(Error::Auth(format!(
            "the authorization server no longer knows the saved client of {0}; run reagent mcp login {0} again",
            server.name
        )));
    }
    let granted = granted(
        status,
        &text,
        prior.as_ref(),
        &scopes,
        metadata::origin(&authorization.token_endpoint),
    )?;
    let token = Token {
        client_id: client.id,
        client_secret: client.secret,
        access: granted.access,
        refresh: granted.refresh,
        expires_at: granted.expires_at,
        scopes: granted.scopes,
        needed: Vec::new(),
        resource,
        redirect_uri: match registers(server, &authorization) {
            true => redirect,
            false => String::new(),
        },
    };
    save(store_dir, &server.name, &issuer, token).await
}

/// Drops every saved login of `name` in `store_dir`; true when one was there.
pub fn logout(name: &str, store_dir: &Path) -> Result<bool, Error> {
    let _lock = Lock::wait(store_dir)?;
    let mut store = read(store_dir)?;
    if !store.remove(name) {
        return Ok(false);
    }
    write_locked(store_dir, &store)?;
    Ok(true)
}

/// The token one server's requests carry, and the refresh behind it.
#[derive(Clone, Debug)]
pub(crate) struct Bearer {
    /// The name the login is saved under.
    server: String,
    /// The folder the login file is in.
    dir: PathBuf,
    /// The canonical URL of the server, which a refresh names as the resource.
    resource: String,
    /// The issuer the token belongs to.
    issuer: String,
    /// The saved token, refreshed when it is near its expiry.
    token: Token,
}

impl Bearer {
    /// The token of `store` for the issuer the server names now, refreshed
    /// when it is near its expiry; `None` when no login is saved.
    ///
    /// A token saved under another issuer is never sent: the spec binds a
    /// credential to its issuer, so a changed one asks for a new login.
    pub(crate) async fn open(store: &AuthStore, url: &str) -> Result<Option<Bearer>, Error> {
        let saved = read_task(&store.dir).await?;
        if !saved.has(&store.name) {
            return Ok(None);
        }
        let resource = metadata::canonical(url);
        let protected = match metadata::resource(&metadata::resource_urls(&resource)).await {
            Ok(protected) => protected,
            Err(_) => return Ok(None),
        };
        let Some(issuer) = protected.authorization_servers.first().cloned() else {
            return Ok(None);
        };
        let Some(token) = saved.token(&store.name, &issuer).cloned() else {
            return Err(needs_login(&store.name, &[]));
        };
        if token.resource != resource {
            // A token bought for another URL is never sent to this one.
            return Err(needs_login(&store.name, &token.needed));
        }
        let mut bearer = Bearer {
            server: store.name.clone(),
            dir: store.dir.clone(),
            resource,
            issuer,
            token,
        };
        if bearer.expiring() {
            bearer.refresh().await?;
        }
        Ok(Some(bearer))
    }

    /// The access token the requests carry.
    pub(crate) fn access(&self) -> &str {
        self.token.access.expose()
    }

    /// Whether the token is close enough to its expiry to be refreshed.
    fn expiring(&self) -> bool {
        self.token.expires_at <= now().saturating_add(EXPIRY_MARGIN)
    }

    /// Buys a new access token with the saved refresh token and saves it.
    pub(crate) async fn refresh(&mut self) -> Result<(), Error> {
        let Some(refresh) = self.token.refresh.clone() else {
            return Err(self.needs_login());
        };
        let authorization = metadata::server(&self.issuer).await.map_err(Error::Auth)?;
        let mut pairs = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.expose()),
            ("client_id", self.token.client_id.as_str()),
            ("resource", self.resource.as_str()),
        ];
        if let Some(secret) = self.token.client_secret.as_ref() {
            pairs.push(("client_secret", secret.expose()));
        }
        let (status, text) = post(
            &authorization.token_endpoint,
            form(&pairs).into_bytes(),
            FORM,
        )
        .await?;
        if let Some(code) = refused(&text)
            && matches!(code, "invalid_grant" | "expired_token" | "access_denied")
        {
            return Err(self.needs_login());
        }
        let granted = granted(
            status,
            &text,
            Some(&self.token),
            &self.token.scopes,
            metadata::origin(&authorization.token_endpoint),
        )?;
        self.token.access = granted.access;
        self.token.refresh = granted.refresh;
        self.token.expires_at = granted.expires_at;
        self.token.scopes = granted.scopes;
        save(&self.dir, &self.server, &self.issuer, self.token.clone()).await
    }

    /// Records the scopes a step-up asked for, so a later login asks for them.
    pub(crate) async fn need(&mut self, scopes: &[String]) -> Result<(), Error> {
        for scope in scopes {
            if !self.token.needed.contains(scope) {
                self.token.needed.push(scope.clone());
            }
        }
        save(&self.dir, &self.server, &self.issuer, self.token.clone()).await
    }

    /// The error that tells the user to run the login command.
    pub(crate) fn needs_login(&self) -> Error {
        needs_login(&self.server, &self.token.needed)
    }

    /// The login hint, with the scopes this request asked for added.
    pub(crate) fn needs_login_with(&self, extra: &[String]) -> Error {
        let mut scopes = self.token.needed.clone();
        for scope in extra {
            if !scopes.contains(scope) {
                scopes.push(scope.clone());
            }
        }
        needs_login(&self.server, &scopes)
    }
}

/// The error that tells the user to run the login command.
fn needs_login(server: &str, scopes: &[String]) -> Error {
    Error::NeedsLogin {
        server: server.to_string(),
        scopes: scopes.to_vec(),
    }
}

/// Reads the login file off the async thread.
async fn read_task(dir: &Path) -> Result<Store, Error> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || read(&dir))
        .await
        .map_err(|_| Error::Auth("the login file task failed".to_string()))?
}

/// Saves one token in the login file, off the async thread.
async fn save(dir: &Path, server: &str, issuer: &str, token: Token) -> Result<(), Error> {
    let (dir, server, issuer) = (dir.to_path_buf(), server.to_string(), issuer.to_string());
    tokio::task::spawn_blocking(move || {
        let _lock = Lock::wait(&dir)?;
        let mut store = read(&dir)?;
        store.put(&server, &issuer, token);
        write_locked(&dir, &store)
    })
    .await
    .map_err(|_| Error::Auth("the login file task failed".to_string()))?
}

/// The challenge an unauthenticated request to `url` is refused with.
async fn probe(url: &str) -> Option<Challenge> {
    let request = client()
        .post(url)
        .header(CONTENT_TYPE, JSON)
        .header(ACCEPT, "application/json, text/event-stream")
        .body(DISCOVER);
    let response = tokio::time::timeout(REQUEST_TIMEOUT, request.send())
        .await
        .ok()?
        .ok()?;
    if response.status().as_u16() != 401 {
        return None;
    }
    let header = response.headers().get(WWW_AUTHENTICATE)?.to_str().ok()?;
    Some(Challenge::of(header))
}

/// The parts of a `WWW-Authenticate` challenge a login reads.
#[derive(Clone, Debug, Default)]
struct Challenge {
    /// The metadata URL the challenge names.
    resource_metadata: Option<String>,
    /// The scope the challenge asks for.
    scope: Option<String>,
}

impl Challenge {
    /// The challenge of one `WWW-Authenticate` header.
    fn of(header: &str) -> Challenge {
        let params = auth_params(header);
        Challenge {
            resource_metadata: param(&params, "resource_metadata"),
            scope: param(&params, "scope"),
        }
    }
}

/// The non-empty value of `name` in `params`.
fn param(params: &[(String, String)], name: &str) -> Option<String> {
    params
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
        .filter(|value| !value.is_empty())
}

/// The protected resource metadata of the server at `resource`.
async fn protected_resource(
    resource: &str,
    challenge: Option<&Challenge>,
) -> Result<metadata::Resource, Error> {
    if let Some(url) = challenge.and_then(|challenge| challenge.resource_metadata.clone())
        && let Ok(protected) = metadata::resource(&[url]).await
    {
        return Ok(protected);
    }
    metadata::resource(&metadata::resource_urls(resource))
        .await
        .map_err(Error::Auth)
}

/// The listener the callback arrives on: `port`, else the port of the
/// `saved` callback when it is free, else any free one.
async fn listener(port: Option<u16>, saved: Option<&str>) -> Result<TcpListener, Error> {
    if let Some(port) = port {
        return listen(port).await;
    }
    if let Some(port) = saved.and_then(saved_port)
        && let Ok(listener) = listen(port).await
    {
        return Ok(listener);
    }
    listen(0).await
}

/// Listens for the callback on `port`, or on any free one.
async fn listen(port: u16) -> Result<TcpListener, Error> {
    TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|error| Error::Auth(format!("cannot listen for the login callback: {error}")))
}

/// The port the callback URL `url` listens on.
fn saved_port(url: &str) -> Option<u16> {
    reqwest::Url::parse(url).ok()?.port()
}

/// Whether a login registers its client at the server, rather than naming one
/// by hand or by a metadata document.
fn registers(server: &ServerAuth, authorization: &metadata::Server) -> bool {
    server.client_id.is_none()
        && !server
            .client_metadata_url
            .as_ref()
            .is_some_and(|url| document_url(url) && authorization.client_metadata_document)
}

/// The client a login signs in as, registering one when it must.
async fn register(
    server: &ServerAuth,
    authorization: &metadata::Server,
    redirect: &str,
    prior: Option<&Token>,
) -> Result<Client, Error> {
    if let Some(id) = &server.client_id {
        return Ok(Client {
            id: id.clone(),
            secret: server.client_secret.clone(),
        });
    }
    if let Some(url) = &server.client_metadata_url
        && document_url(url)
        && authorization.client_metadata_document
    {
        return Ok(Client {
            id: url.clone(),
            secret: None,
        });
    }
    if let Some(prior) = prior
        && prior.redirect_uri == redirect
        && !prior.client_id.is_empty()
    {
        return Ok(Client {
            id: prior.client_id.clone(),
            secret: prior.client_secret.clone(),
        });
    }
    let Some(endpoint) = &authorization.registration_endpoint else {
        return Err(Error::Auth(format!(
            "the authorization server of {} accepts no new client; set client_id for the server",
            server.name
        )));
    };
    let body = serde_json::json!({
        "client_name": CLIENT_NAME,
        "redirect_uris": [redirect],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "application_type": "native",
    })
    .to_string();
    let (status, text) = post(endpoint, body.into_bytes(), JSON).await?;
    let place = metadata::origin(endpoint);
    if !(200..300).contains(&status) {
        return Err(Error::Auth(format!("{place}: HTTP {status}")));
    }
    let registration: Registration<'_> = serde_json::from_str(&text)
        .map_err(|_| Error::Auth(format!("{place}: the reply is not JSON")))?;
    let Some(id) = registration.client_id.filter(|id| !id.is_empty()) else {
        return Err(Error::Auth(format!("{place}: the reply has no client id")));
    };
    Ok(Client {
        id: id.to_string(),
        secret: registration.client_secret.map(Secret::from),
    })
}

/// Whether `url` is a client id metadata document: `https` and a path.
fn document_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let path = rest.find('/').map(|at| &rest[at..]).unwrap_or_default();
    !path.is_empty() && path != "/"
}

/// One client a login signs in as.
struct Client {
    /// The client id, which the authorize and token requests name.
    id: String,
    /// The secret of that client, when it has one.
    secret: Option<Secret>,
}

/// One dynamic registration as the token endpoint answers it.
#[derive(Deserialize)]
struct Registration<'a> {
    #[serde(borrow)]
    client_id: Option<&'a str>,
    #[serde(borrow)]
    client_secret: Option<&'a str>,
}

/// One token or error reply as the token endpoint writes it.
#[derive(Deserialize)]
struct Tokens<'a> {
    #[serde(borrow)]
    access_token: Option<&'a str>,
    #[serde(borrow)]
    refresh_token: Option<&'a str>,
    #[serde(borrow)]
    scope: Option<&'a str>,
    #[serde(borrow)]
    error: Option<&'a str>,
    expires_in: Option<f64>,
}

/// What a token endpoint reply gives, before it is saved.
struct Granted {
    /// The new access token.
    access: Secret,
    /// The refresh token, which a reply without one leaves as it was.
    refresh: Option<Secret>,
    /// The time the access token stops being usable.
    expires_at: u64,
    /// The scopes the token covers.
    scopes: Vec<String>,
}

/// The token of one reply, or why it holds none.
///
/// A reply without a scope keeps the scopes of `previous`, or the ones the
/// login asked for.
fn granted(
    status: u16,
    text: &str,
    previous: Option<&Token>,
    asked: &[String],
    url: &str,
) -> Result<Granted, Error> {
    let reply: Option<Tokens<'_>> = serde_json::from_str(text).ok();
    if let Some(code) = reply.as_ref().and_then(|reply| reply.error) {
        return Err(Error::Auth(format!("{url}: HTTP {status} ({code})")));
    }
    if !(200..300).contains(&status) {
        return Err(Error::Auth(format!("{url}: HTTP {status}")));
    }
    let reply = reply.ok_or_else(|| Error::Auth(format!("{url}: the reply is not JSON")))?;
    let Some(access) = reply.access_token.filter(|token| !token.is_empty()) else {
        return Err(Error::Auth(format!("{url}: the reply has no access token")));
    };
    let refresh = match reply.refresh_token.filter(|token| !token.is_empty()) {
        Some(token) => Some(Secret::from(token)),
        None => previous.and_then(|token| token.refresh.clone()),
    };
    let lifetime = reply
        .expires_in
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map_or(DEFAULT_LIFETIME, |seconds| seconds.ceil() as u64);
    let scopes = match reply.scope {
        Some(scope) => scope.split_whitespace().map(str::to_owned).collect(),
        None => match previous {
            Some(token) => token.scopes.clone(),
            None => asked.to_vec(),
        },
    };
    Ok(Granted {
        access: Secret::from(access),
        refresh,
        expires_at: now().saturating_add(lifetime),
        scopes,
    })
}

/// The scopes one login asks for.
///
/// The challenge's scope wins, then the configured one, then the resource's.
/// The scopes a refresh needs and the ones a step-up asked for are added, so a
/// second login widens the grant instead of replacing it.
fn requested(
    server: &ServerAuth,
    challenge: Option<&Challenge>,
    protected: &metadata::Resource,
    authorization: &metadata::Server,
    prior: Option<&Token>,
) -> Vec<String> {
    let base = match challenge.and_then(|challenge| challenge.scope.as_deref()) {
        Some(scope) => scope.split_whitespace().map(str::to_owned).collect(),
        None => match server.scope.as_deref() {
            Some(scope) => scope.split_whitespace().map(str::to_owned).collect(),
            None => protected.scopes_supported.clone(),
        },
    };
    let mut scopes: Vec<String> = Vec::with_capacity(base.len() + 3);
    let add = |scopes: &mut Vec<String>, scope: &str| {
        if !scopes.iter().any(|one| one == scope) {
            scopes.push(scope.to_string());
        }
    };
    for scope in base {
        add(&mut scopes, &scope);
    }
    if authorization
        .scopes_supported
        .iter()
        .any(|one| one == OFFLINE)
    {
        add(&mut scopes, OFFLINE);
    }
    if let Some(prior) = prior {
        for scope in prior.scopes.iter().chain(&prior.needed) {
            add(&mut scopes, scope);
        }
    }
    scopes
}

/// The URL the browser is sent to for one login.
fn authorize_url(
    authorization: &metadata::Server,
    client: &Client,
    redirect: &str,
    challenge: &str,
    state: &str,
    scopes: &[String],
    resource: &str,
) -> String {
    let mut url = String::with_capacity(512);
    url.push_str(&authorization.authorization_endpoint);
    let separator = match authorization.authorization_endpoint.contains('?') {
        true => '&',
        false => '?',
    };
    url.push(separator);
    url.push_str("response_type=code&client_id=");
    url.push_str(&percent_encode(&client.id));
    url.push_str("&redirect_uri=");
    url.push_str(&percent_encode(redirect));
    url.push_str("&code_challenge=");
    url.push_str(&percent_encode(challenge));
    url.push_str("&code_challenge_method=S256&state=");
    url.push_str(&percent_encode(state));
    if !scopes.is_empty() {
        url.push_str("&scope=");
        url.push_str(&percent_encode(&scopes.join(" ")));
    }
    url.push_str("&resource=");
    url.push_str(&percent_encode(resource));
    url
}

/// Checks the callback against the login that is waiting on it.
///
/// The state is checked before anything else the callback carries is read, and
/// the `iss` of RFC 9207 must be the issuer of the server whose document the
/// login used. A refused callback never shows the `error` it carries.
fn check_callback(
    query: &[(String, String)],
    state: &str,
    authorization: &metadata::Server,
) -> Result<(), Error> {
    if field(query, "state") != Some(state) {
        return Err(Error::Auth(
            "the login callback names another state".to_string(),
        ));
    }
    match field(query, "iss") {
        Some(iss) if iss != authorization.issuer => {
            return Err(Error::Auth(
                "the login callback names another issuer".to_string(),
            ));
        }
        None if authorization.iss_parameter => {
            return Err(Error::Auth(
                "the login callback carries no issuer".to_string(),
            ));
        }
        _ => {}
    }
    match field(query, "error") {
        Some("access_denied") => Err(Error::Auth("the login was denied".to_string())),
        Some(error) => Err(Error::Auth(format!("the login failed: {error}"))),
        None => Ok(()),
    }
}

/// Posts `body` of `kind` to `url` and returns its status and its text.
async fn post(url: &str, body: Vec<u8>, kind: &str) -> Result<(u16, String), Error> {
    let request = client()
        .post(url)
        .header(CONTENT_TYPE, kind)
        .header(ACCEPT, JSON)
        .body(body);
    let place = metadata::origin(url);
    let response = tokio::time::timeout(REQUEST_TIMEOUT, request.send())
        .await
        .map_err(|_| Error::Auth(format!("{place} did not answer")))?
        .map_err(|error| Error::Auth(format!("{place}: {}", error.without_url())))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|error| Error::Auth(format!("{place}: {}", error.without_url())))?;
    Ok((status, text))
}

/// The `application/x-www-form-urlencoded` text of `pairs`.
fn form(pairs: &[(&str, &str)]) -> String {
    let mut text = String::with_capacity(pairs.len() * 32);
    for (name, value) in pairs {
        if !text.is_empty() {
            text.push('&');
        }
        text.push_str(name);
        text.push('=');
        text.push_str(&percent_encode(value));
    }
    text
}

/// The error code a token reply names.
fn refused(text: &str) -> Option<&str> {
    serde_json::from_str::<Tokens<'_>>(text).ok()?.error
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(scopes: &[String], needed: &[String]) -> Token {
        Token {
            client_id: "c1".to_string(),
            client_secret: Some(Secret::from("secret-of-the-client")),
            access: Secret::from("secret-of-the-access"),
            refresh: Some(Secret::from("secret-of-the-refresh")),
            expires_at: 1_800_000_000,
            scopes: scopes.to_vec(),
            needed: needed.to_vec(),
            resource: "https://a/mcp".to_string(),
            redirect_uri: String::new(),
        }
    }

    #[test]
    fn oauth_store_keeps_a_token_per_server_and_issuer() {
        let mut store = Store::default();
        store.put("srv", "https://a", token(&[], &[]));
        store.put("srv", "https://b", token(&[], &[]));
        store.put("other", "https://a", token(&[], &[]));
        assert_eq!(store.issuers("srv"), vec!["https://a", "https://b"]);
        assert!(store.token("srv", "https://c").is_none());
        assert!(store.remove("srv"));
        assert!(!store.has("srv"));
        assert!(!store.remove("srv"));
    }

    #[test]
    fn oauth_store_debug_hides_the_secrets() {
        let mut store = Store::default();
        store.put("srv", "https://a", token(&[], &[]));
        let text = format!("{store:?}");
        for secret in [
            "secret-of-the-client",
            "secret-of-the-access",
            "secret-of-the-refresh",
        ] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }

    #[test]
    fn oauth_store_round_trips_through_the_file() {
        let dir = tempfile::tempdir().expect("a temp folder");
        assert!(read(dir.path()).expect("reads").is_empty());
        let mut store = Store::default();
        store.put(
            "srv",
            "https://a",
            token(&["files.read".to_string()], &["files.write".to_string()]),
        );
        write(dir.path(), &store).expect("writes");
        assert_eq!(read(dir.path()).expect("reads"), store);
        assert!(logout("srv", dir.path()).expect("logs out"));
        assert!(read(dir.path()).expect("reads").is_empty());
    }

    #[test]
    #[expect(clippy::disallowed_methods, reason = "test setup on a temp dir")]
    fn oauth_store_skips_an_entry_it_cannot_read() {
        let dir = tempfile::tempdir().expect("a temp folder");
        let path = path(dir.path());
        std::fs::write(
            &path,
            "{\"srv\":{\"https://a\":{\"client_id\":\"c\",\"access\":\"a\",\"resource\":\"r\"},\"https://b\":5}}",
        )
        .expect("writes");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmods");
        let store = read(dir.path()).expect("reads");
        assert!(store.token("srv", "https://a").is_some());
        assert!(store.token("srv", "https://b").is_none());
    }

    #[test]
    fn oauth_callback_checks_the_state_then_the_issuer() {
        let authorization = metadata::Server {
            issuer: "https://a".to_string(),
            iss_parameter: true,
            ..metadata::Server::default()
        };
        let pairs = |items: &[(&str, &str)]| {
            items
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect::<Vec<_>>()
        };
        assert!(
            check_callback(
                &pairs(&[("state", "s"), ("code", "c"), ("iss", "https://a")]),
                "s",
                &authorization
            )
            .is_ok()
        );
        let other_issuer = pairs(&[("state", "s"), ("iss", "https://b"), ("error", "denied")]);
        let error = check_callback(&other_issuer, "s", &authorization).unwrap_err();
        assert!(!error.to_string().contains("denied"), "{error}");
        assert!(
            check_callback(
                &pairs(&[("state", "s"), ("code", "c")]),
                "s",
                &authorization
            )
            .is_err(),
            "an iss is required when the server says it sends one"
        );
        assert!(check_callback(&pairs(&[("state", "t")]), "s", &authorization).is_err());
    }

    #[test]
    fn oauth_requested_scopes_widen_a_stored_grant() {
        let server = ServerAuth::default();
        let protected = metadata::Resource {
            scopes_supported: vec!["files.read".to_string()],
            ..metadata::Resource::default()
        };
        let authorization = metadata::Server {
            scopes_supported: vec!["files.read".to_string(), OFFLINE.to_string()],
            ..metadata::Server::default()
        };
        let prior = token(&["files.read".to_string()], &["files.write".to_string()]);
        assert_eq!(
            requested(&server, None, &protected, &authorization, Some(&prior)),
            vec![
                "files.read".to_string(),
                OFFLINE.to_string(),
                "files.write".to_string(),
            ]
        );
        let challenge = Challenge {
            resource_metadata: None,
            scope: Some("admin".to_string()),
        };
        assert_eq!(
            requested(&server, Some(&challenge), &protected, &authorization, None),
            vec!["admin".to_string(), OFFLINE.to_string()]
        );
    }

    #[test]
    fn oauth_client_metadata_url_needs_https_and_a_path() {
        assert!(document_url("https://a/client.json"));
        assert!(!document_url("https://a"));
        assert!(!document_url("https://a/"));
        assert!(!document_url("http://a/client.json"));
    }

    #[test]
    fn oauth_authorize_url_names_the_challenge_the_state_and_the_resource() {
        let authorization = metadata::Server {
            authorization_endpoint: "https://a/authorize".to_string(),
            ..metadata::Server::default()
        };
        let client = Client {
            id: "c 1".to_string(),
            secret: None,
        };
        let url = authorize_url(
            &authorization,
            &client,
            "http://127.0.0.1:1/callback",
            "ch",
            "st",
            &["files.read".to_string()],
            "https://h/mcp",
        );
        assert_eq!(
            url,
            "https://a/authorize?response_type=code&client_id=c%201&redirect_uri=http%3A%2F%2F127.0.0.1%3A1%2Fcallback&code_challenge=ch&code_challenge_method=S256&state=st&scope=files.read&resource=https%3A%2F%2Fh%2Fmcp"
        );
    }
}
