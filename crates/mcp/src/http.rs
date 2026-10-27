//! The streamable HTTP transport: one POST per message and its answer.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use reagent_ai::sse::SseSplitter;
use reqwest::Url;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, LOCATION};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::auth::{AuthStore, Bearer};
use crate::client::{READ_CAP, READ_CAP_MIB, Reply, RpcFailure};
use crate::error::Error;
use crate::protocol::{self, Era, Params};

/// How many more times a request is sent after a status that asks for one.
const RETRIES: usize = 2;

/// How long each retry waits when the server asks for nothing.
const RETRY_WAITS: [Duration; RETRIES] = [Duration::from_millis(500), Duration::from_secs(1)];

/// The longest wait a `Retry-After` header may ask for.
const RETRY_CAP: Duration = Duration::from_secs(5);

/// How much of an unusable reply body is kept for the error text.
const BODY_LIMIT: usize = 300;

/// How many warnings the client keeps.
const WARNING_LOG: usize = 8;

/// How long the modern probe may take before the server counts as legacy.
const PROBE_LIMIT: Duration = Duration::from_secs(5);

/// How long the `DELETE` that ends a legacy session may take.
const STOP_LIMIT: Duration = Duration::from_secs(2);

/// The `Content-Type` of every request and of a JSON reply.
const JSON: &str = "application/json";

/// The `Accept` of every request, which allows either kind of answer.
const ACCEPT: &str = "application/json, text/event-stream";

/// The statuses that ask a client to repeat its `POST` at another URL.
const REDIRECTS: [u16; 2] = [307, 308];

/// The redirect statuses this client does not follow.
const UNFOLLOWED_REDIRECTS: [u16; 3] = [301, 302, 303];

/// The note a redirect away from the server's origin carries.
const REDIRECT_FOREIGN: &str = "redirected to another origin";

/// The note a redirect this client does not follow carries.
const REDIRECT_UNFOLLOWED: &str = "redirected with a status this client does not follow";

/// The note a second redirect of one request carries.
const REDIRECT_REPEATED: &str = "redirected more than once";

/// The note a redirect without a usable `Location` carries.
const REDIRECT_UNUSABLE: &str = "redirected to a Location this client cannot read";

/// The prefix and the suffix of a base64 value of the spec.
const SENTINEL: &str = "=?base64?";
const SENTINEL_END: &str = "?=";

/// The request metadata headers of the spec.
const VERSION_HEADER: &str = "mcp-protocol-version";
const METHOD_HEADER: &str = "mcp-method";
const NAME_HEADER: &str = "mcp-name";

/// The lowercase prefix of the header that carries one argument.
const PARAM_PREFIX: &str = "mcp-param-";

/// The header that names a legacy session.
const SESSION_HEADER: &str = "mcp-session-id";

/// The characters of a header name token, per RFC 9110.
const TOKEN_PUNCTUATION: &[u8] = b"!#$%&'*+-.^_`|~";

/// How to reach one MCP server over streamable HTTP.
#[derive(Clone, Debug, Default)]
pub struct HttpSpec {
    /// The endpoint every message is posted to.
    pub url: String,
    /// Headers sent with every request, their values already expanded.
    pub headers: Vec<(String, String)>,
    /// Where the server's OAuth login is saved, when it has one.
    pub auth: Option<AuthStore>,
}

/// One server reached over the streamable HTTP transport.
#[derive(Clone)]
pub(crate) struct Http {
    shared: Arc<Shared>,
}

/// The state the clones of one server share.
struct Shared {
    client: reqwest::Client,
    url: String,
    headers: Vec<(HeaderName, HeaderValue)>,
    auth: Option<AuthStore>,
    login: Mutex<Login>,
    /// Serializes the refreshes and the step-ups that write the token.
    refreshing: tokio::sync::Mutex<()>,
    /// Counts the refreshes that replaced the token, so a request that waited
    /// for one knows it need not refresh again.
    refreshed: AtomicU64,
    session: Mutex<Option<HeaderValue>>,
    next_id: AtomicU64,
    stale: AtomicBool,
    warnings: Mutex<Vec<String>>,
}

/// The saved login of one server, with the header its token sends.
#[derive(Default)]
struct Login {
    bearer: Option<Bearer>,
    /// `bearer` as an `Authorization` value, built once per token.
    authorization: Option<HeaderValue>,
}

impl Login {
    /// The login of `bearer`, with the header it sends.
    fn new(bearer: Option<Bearer>) -> Login {
        let authorization = bearer
            .as_ref()
            .and_then(|bearer| HeaderValue::from_str(&format!("Bearer {}", bearer.access())).ok());
        Login {
            bearer,
            authorization,
        }
    }
}

impl Http {
    /// Connects, finds the era, and keeps the session of a legacy server.
    pub(crate) async fn connect(spec: &HttpSpec) -> Result<(Http, Era), Error> {
        // The token is resolved first: a server whose login is missing or
        // belongs to another issuer fails before a request is sent.
        let bearer = match &spec.auth {
            Some(store) => Bearer::open(store, &spec.url).await?,
            None => None,
        };
        let shared = Arc::new(Shared {
            client: reagent_oauth::http::client(),
            url: spec.url.clone(),
            headers: header_pairs(spec)?,
            auth: spec.auth.clone(),
            login: Mutex::new(Login::new(bearer)),
            refreshing: tokio::sync::Mutex::new(()),
            refreshed: AtomicU64::new(0),
            session: Mutex::new(None),
            next_id: AtomicU64::new(1),
            stale: AtomicBool::new(false),
            warnings: Mutex::new(Vec::new()),
        });
        let http = Http { shared };
        let era = http.detect().await?;
        Ok((http, era))
    }

    /// The id the next request takes.
    pub(crate) fn next_id(&self) -> u64 {
        self.shared.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// One request and its result text.
    pub(crate) async fn request(
        &self,
        era: Era,
        method: &str,
        params: Params<'_>,
    ) -> Result<Bytes, Error> {
        let id = self.next_id();
        self.round_trip(Post::new(Some(id), method, params, era))
            .await
    }

    /// One tool call, with the headers of its arguments and its own id.
    pub(crate) async fn call(
        &self,
        id: u64,
        era: Era,
        params: Params<'_>,
        extra: Vec<(HeaderName, HeaderValue)>,
    ) -> Result<Bytes, Error> {
        let mut post = Post::new(Some(id), "tools/call", params, era);
        post.extra = extra;
        self.round_trip(post).await
    }

    /// Sends one notification, which the server accepts with no answer.
    pub(crate) async fn notify(
        &self,
        era: Era,
        method: &str,
        params: Params<'_>,
    ) -> Result<(), Error> {
        let post = Post::new(None, method, params, era);
        self.exchange(&post).await.map(|_| ())
    }

    /// Tells a legacy server that the call `id` is cancelled, without waiting.
    pub(crate) fn cancel(&self, era: Era, id: u64) {
        let http = self.clone();
        tokio::spawn(async move {
            let params = Params::Cancelled { request_id: id };
            let _ = http.notify(era, "notifications/cancelled", params).await;
        });
    }

    /// Ends the legacy session with a `DELETE`, whose answer is ignored.
    pub(crate) async fn stop(&self) {
        let Some(session) = self.session() else {
            return;
        };
        let mut request = self
            .shared
            .client
            .delete(&self.shared.url)
            .header(HeaderName::from_static(SESSION_HEADER), session);
        if let Some(bearer) = self.authorization() {
            request = request.header(AUTHORIZATION, bearer);
        }
        let _ = tokio::time::timeout(STOP_LIMIT, request.send()).await;
    }

    /// Whether the server announced a new tool list.
    pub(crate) fn stale(&self) -> bool {
        self.shared.stale.load(Ordering::Relaxed)
    }

    pub(crate) fn set_stale(&self, stale: bool) {
        self.shared.stale.store(stale, Ordering::Relaxed);
    }

    /// Adds one warning to the log, keeping the last few.
    pub(crate) fn warn(&self, warning: String) {
        let mut log = self.warnings_mutex();
        if log.len() >= WARNING_LOG {
            log.remove(0);
        }
        log.push(warning);
    }

    /// The last few warnings of this server.
    pub(crate) fn warnings(&self) -> Vec<String> {
        self.warnings_mutex().clone()
    }

    /// Finds the era: the modern probe first, then the legacy handshake.
    async fn detect(&self) -> Result<Era, Error> {
        let modern = Era::Modern(protocol::MODERN);
        let id = self.next_id();
        let post = Post::new(Some(id), "server/discover", Params::Discover, modern);
        match self.reply(post, Some(PROBE_LIMIT)).await {
            Ok(Reply::Result(_)) => Ok(modern),
            Ok(Reply::Error(failure)) if failure.code == protocol::UNSUPPORTED_VERSION => {
                crate::client::version_shared(&failure)
            }
            Ok(Reply::Error(failure)) if failure.code == protocol::METHOD_NOT_FOUND => {
                self.handshake().await
            }
            Ok(Reply::Error(_)) => Ok(modern),
            Err(Error::Status { code, body }) => match refused(&body) {
                Some(failure) if matches!(code, 400 | 404) => {
                    if failure.code == protocol::UNSUPPORTED_VERSION {
                        crate::client::version_shared(&failure)
                    } else {
                        Ok(modern)
                    }
                }
                _ if matches!(code, 400 | 404 | 405) => self.handshake().await,
                _ => Err(Error::Status { code, body }),
            },
            Err(error) => Err(error),
        }
    }

    /// The legacy handshake: `initialize`, then `notifications/initialized`.
    async fn handshake(&self) -> Result<Era, Error> {
        let text = self.initialize().await?;
        let era = Era::Legacy(crate::client::legacy_version(&text)?);
        self.notify(era, "notifications/initialized", Params::Empty)
            .await?;
        Ok(era)
    }

    /// POSTs `initialize` and keeps the session id it answers with.
    async fn initialize(&self) -> Result<Bytes, Error> {
        *self.session_mutex() = None;
        let era = Era::Legacy(protocol::LEGACY_DEFAULT);
        let id = self.next_id();
        let params = Params::Initialize {
            version: protocol::LEGACY_DEFAULT,
        };
        let mut post = Post::new(Some(id), "initialize", params, era);
        // No version is negotiated before the handshake answers.
        post.version = false;
        let response = self
            .request_for(&post, &self.shared.url)
            .send()
            .await
            .map_err(transport)?;
        let status = response.status().as_u16();
        if let Some(session) = response.headers().get(SESSION_HEADER).cloned() {
            *self.session_mutex() = Some(session);
        }
        if status != 200 {
            return Err(Error::Status {
                code: status,
                body: head_text(response).await,
            });
        }
        let body = bounded(response).await?;
        let message =
            serde_json::from_slice::<protocol::Message<'_>>(&body).map_err(protocol_error)?;
        match crate::client::response(message.result, message.error, Some(&body)) {
            Some(Reply::Result(text)) => Ok(text),
            Some(Reply::Error(failure)) => Err(failure.error()),
            None => Err(Error::Protocol(
                "the server sent no answer to initialize".to_owned(),
            )),
        }
    }

    /// Repeats the handshake after a legacy session went away: a new
    /// `initialize`, then the notification the first handshake sends.
    async fn reinitialize(&self) -> Result<(), Error> {
        let text = self.initialize().await?;
        let era = Era::Legacy(crate::client::legacy_version(&text)?);
        let post = Post::new(None, "notifications/initialized", Params::Empty, era);
        self.send_once(&post).await
    }

    /// One request, waiting at most `limit` for its answer.
    async fn reply(&self, post: Post<'_>, limit: Option<Duration>) -> Result<Reply, Error> {
        let method = post.method;
        let answer = match limit {
            Some(limit) => tokio::time::timeout(limit, self.exchange(&post))
                .await
                .map_err(|_| Error::Timeout {
                    method: method.to_owned(),
                    ms: limit.as_millis() as u64,
                })?,
            None => self.exchange(&post).await,
        };
        match answer? {
            Answer::Reply(reply) => Ok(reply),
            Answer::Accepted => Err(Error::Protocol(
                "the server accepted a request without answering it".to_owned(),
            )),
        }
    }

    /// One message and the result text it answers with.
    async fn round_trip(&self, post: Post<'_>) -> Result<Bytes, Error> {
        match self.reply(post, None).await? {
            Reply::Result(text) => Ok(text),
            Reply::Error(failure) => Err(failure.error()),
        }
    }

    /// Sends one message and reads what comes back, retrying the statuses a
    /// server asks for.
    async fn exchange(&self, post: &Post<'_>) -> Result<Answer, Error> {
        let mut attempt = 0;
        let mut restored = false;
        let mut refreshed = false;
        // The URL of a followed redirect, which holds for this exchange only.
        let mut redirected: Option<String> = None;
        loop {
            let url = redirected.as_deref().unwrap_or(&self.shared.url);
            let response = self
                .request_for(post, url)
                .send()
                .await
                .map_err(transport)?;
            let status = response.status().as_u16();
            if let Some(session) = response.headers().get(SESSION_HEADER).cloned() {
                *self.session_mutex() = Some(session);
            }
            if UNFOLLOWED_REDIRECTS.contains(&status) {
                return Err(self.refuse(status, response.headers()));
            }
            if REDIRECTS.contains(&status) {
                if redirected.is_some() {
                    return Err(Error::Status {
                        code: status,
                        body: REDIRECT_REPEATED.to_owned(),
                    });
                }
                redirected = Some(self.follow(status, response.headers(), url)?);
                continue;
            }
            if retry_status(status) && attempt < RETRIES {
                let wait = retry_wait(response.headers(), attempt);
                attempt += 1;
                tokio::time::sleep(wait).await;
                continue;
            }
            if status == 401 {
                // One refresh and one retry, then the login is the answer.
                if !refreshed && self.refresh().await? {
                    refreshed = true;
                    continue;
                }
                return Err(self.refused(response.headers()));
            }
            if status == 403
                && self.logged_in()
                && let Some(scopes) = insufficient_scope(response.headers())
            {
                self.need(&scopes).await?;
                return Err(self.hint(scopes));
            }
            if status == 404 && self.session().is_some() && !restored {
                restored = true;
                self.reinitialize().await?;
                continue;
            }
            if status != 200 && status != 202 {
                return Err(Error::Status {
                    code: status,
                    body: head_text(response).await,
                });
            }
            let Some(id) = post.id else {
                return Ok(Answer::Accepted);
            };
            if status == 202 {
                return Err(Error::Protocol(
                    "the server accepted a request without answering it".to_owned(),
                ));
            }
            return if is_event_stream(response.headers()) {
                self.read_stream(response, id, post.era).await
            } else {
                self.read_json(response, id, post.era).await
            };
        }
    }

    /// Sends one message and drops its answer, for a reply to the server.
    async fn send_once(&self, post: &Post<'_>) -> Result<(), Error> {
        self.request_for(post, &self.shared.url)
            .send()
            .await
            .map_err(transport)?;
        Ok(())
    }

    /// The URL a redirect of `status` from `current` names, when this client
    /// may follow it: one step, on the server's own origin.
    fn follow(&self, status: u16, headers: &HeaderMap, current: &str) -> Result<String, Error> {
        let fail = |body: &str| Error::Status {
            code: status,
            body: body.to_owned(),
        };
        let location = located(headers).ok_or_else(|| fail(REDIRECT_UNUSABLE))?;
        let base = Url::parse(current).map_err(|_| fail(REDIRECT_UNUSABLE))?;
        let next = base.join(location).map_err(|_| fail(REDIRECT_UNUSABLE))?;
        let origin = Url::parse(&self.shared.url).map_err(|_| fail(REDIRECT_UNUSABLE))?;
        if !same_origin(&origin, &next) {
            let host = next.host_str().unwrap_or_default();
            return Err(fail(&format!("{REDIRECT_FOREIGN} ({host})")));
        }
        Ok(next.into())
    }

    /// The error of a redirect this client does not follow: its code and the
    /// host it names, never its path.
    fn refuse(&self, status: u16, headers: &HeaderMap) -> Error {
        let body =
            match located(headers).and_then(|location| redirect_host(location, &self.shared.url)) {
                Some(host) => format!("{REDIRECT_UNFOLLOWED} ({host})"),
                None => REDIRECT_UNFOLLOWED.to_owned(),
            };
        Error::Status { code: status, body }
    }

    /// The request of one message, without its body.
    fn request_for(&self, post: &Post<'_>, url: &str) -> reqwest::RequestBuilder {
        let mut request = self
            .shared
            .client
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                HeaderValue::from_static(JSON),
            )
            .header(reqwest::header::ACCEPT, HeaderValue::from_static(ACCEPT))
            .header(HeaderName::from_static(METHOD_HEADER), post.method);
        if post.version {
            request = request.header(
                HeaderName::from_static(VERSION_HEADER),
                HeaderValue::from_static(post.era.version()),
            );
        }
        if let Some(name) = post.name {
            request = request.header(HeaderName::from_static(NAME_HEADER), encode(name));
        }
        for (name, value) in &self.shared.headers {
            request = request.header(name.clone(), value.clone());
        }
        for (name, value) in &post.extra {
            request = request.header(name.clone(), value.clone());
        }
        if let Some(bearer) = self.authorization() {
            request = request.header(AUTHORIZATION, bearer);
        }
        if let Some(session) = self.session() {
            request = request.header(HeaderName::from_static(SESSION_HEADER), session);
        }
        request.body(post.body.clone())
    }

    /// The `Authorization` header the requests carry, when a token is saved.
    fn authorization(&self) -> Option<HeaderValue> {
        self.login_mutex().authorization.clone()
    }

    /// Whether the server signs in with OAuth at all.
    fn logged_in(&self) -> bool {
        self.shared.auth.is_some()
    }

    /// Refreshes the saved token; false when the server signs in with none.
    ///
    /// One refresh runs at a time and the token stays readable while it runs,
    /// so no request of this server goes out without one. A request that finds
    /// a refresh already done under it keeps the new token instead of buying a
    /// second one. A refresh that cannot buy a token leaves the login as the
    /// only way on.
    async fn refresh(&self) -> Result<bool, Error> {
        if !self.logged_in() {
            return Ok(false);
        }
        let seen = self.shared.refreshed.load(Ordering::SeqCst);
        let _refreshing = self.shared.refreshing.lock().await;
        if self.shared.refreshed.load(Ordering::SeqCst) != seen {
            return Ok(true);
        }
        let Some(mut bearer) = self.bearer() else {
            return Ok(false);
        };
        let outcome = bearer.refresh().await;
        if outcome.is_ok() {
            self.shared.refreshed.fetch_add(1, Ordering::SeqCst);
        }
        self.put_bearer(bearer);
        outcome.map_err(|error| match error {
            Error::NeedsLogin { .. } => error,
            _ => self.hint(Vec::new()),
        })?;
        Ok(true)
    }

    /// Records the scopes a step-up asked for, so a login asks for them too.
    async fn need(&self, scopes: &[String]) -> Result<(), Error> {
        let _refreshing = self.shared.refreshing.lock().await;
        let Some(mut bearer) = self.bearer() else {
            return Ok(());
        };
        let outcome = bearer.need(scopes).await;
        self.put_bearer(bearer);
        outcome
    }

    /// The error a refused request answers with.
    fn refused(&self, headers: &HeaderMap) -> Error {
        if self.logged_in() {
            return self.hint(asked_scopes(headers));
        }
        Error::Unauthorized {
            resource_metadata: resource_metadata(headers),
        }
    }

    /// The login hint of this server, with the scopes it needs.
    fn hint(&self, scopes: Vec<String>) -> Error {
        let login = self.login_mutex();
        match (login.bearer.as_ref(), &self.shared.auth) {
            (Some(bearer), _) => bearer.needs_login_with(&scopes),
            (None, Some(store)) => Error::NeedsLogin {
                server: store.name.clone(),
                scopes,
            },
            (None, None) => Error::Unauthorized {
                resource_metadata: None,
            },
        }
    }

    /// The bearer, copied out of the lock so the token stays readable.
    fn bearer(&self) -> Option<Bearer> {
        self.login_mutex().bearer.clone()
    }

    /// Puts the bearer back after an await, with the header it now sends.
    fn put_bearer(&self, bearer: Bearer) {
        *self.login_mutex() = Login::new(Some(bearer));
    }

    /// Reads a `200` that holds one JSON message.
    async fn read_json(
        &self,
        response: reqwest::Response,
        want: u64,
        era: Era,
    ) -> Result<Answer, Error> {
        let body = bounded(response).await?;
        let message =
            serde_json::from_slice::<protocol::Message<'_>>(&body).map_err(protocol_error)?;
        match self.handle(era, message, Some(&body)).await? {
            Some((id, reply)) if id == want => Ok(Answer::Reply(reply)),
            Some(_) => Err(Error::Protocol(
                "the server answered a request this client did not send".to_owned(),
            )),
            None => Err(Error::Protocol("the server sent no answer".to_owned())),
        }
    }

    /// Reads a `200` event stream until the answer to `want` arrives.
    async fn read_stream(
        &self,
        mut response: reqwest::Response,
        want: u64,
        era: Era,
    ) -> Result<Answer, Error> {
        let mut splitter = SseSplitter::new();
        let mut size = 0;
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            let mut lines = splitter.push(&chunk);
            while let Some(line) = lines.next_line() {
                size = event_size(size, line)?;
                if let Some(reply) = self.event(line, want, era).await? {
                    return Ok(Answer::Reply(reply));
                }
            }
        }
        let last = splitter.finish().map(<[u8]>::to_vec);
        if let Some(line) = last {
            event_size(size, &line)?;
            if let Some(reply) = self.event(&line, want, era).await? {
                return Ok(Answer::Reply(reply));
            }
        }
        Err(Error::Protocol(
            "the event stream ended without an answer".to_owned(),
        ))
    }

    /// Handles one line of an event stream; the answer to `want` ends it.
    async fn event(&self, line: &[u8], want: u64, era: Era) -> Result<Option<Reply>, Error> {
        if line.is_empty() || line[0] == b':' {
            return Ok(None);
        }
        if let Some(name) = line.strip_prefix(b"event:") {
            if name.trim_ascii() == b"endpoint" {
                return Err(unsupported_transport());
            }
            return Ok(None);
        }
        let Some(data) = line.strip_prefix(b"data:") else {
            return Ok(None);
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        if data.is_empty() {
            return Ok(None);
        }
        let message =
            serde_json::from_slice::<protocol::Message<'_>>(data).map_err(protocol_error)?;
        match self.handle(era, message, None).await? {
            Some((id, reply)) if id == want => Ok(Some(reply)),
            _ => Ok(None),
        }
    }

    /// Handles one message off the wire; a response returns its id and reply.
    ///
    /// `body` is the buffer the message was read from, when a result can be a
    /// slice of it instead of a copy.
    async fn handle(
        &self,
        era: Era,
        message: protocol::Message<'_>,
        body: Option<&Bytes>,
    ) -> Result<Option<(u64, Reply)>, Error> {
        match (message.method, message.id) {
            (Some(method), Some(id)) => {
                self.answer(era, method, id).await?;
                Ok(None)
            }
            (Some(method), None) => {
                if method == protocol::LIST_CHANGED {
                    self.shared.stale.store(true, Ordering::Relaxed);
                }
                Ok(None)
            }
            (None, Some(id)) => {
                let Some(id) = id.get().parse::<u64>().ok() else {
                    return Ok(None);
                };
                Ok(crate::client::response(message.result, message.error, body)
                    .map(|reply| (id, reply)))
            }
            (None, None) => Ok(None),
        }
    }

    /// Answers a request the server sent on a stream: `ping` and nothing else.
    async fn answer(&self, era: Era, method: &str, id: &RawValue) -> Result<(), Error> {
        let body = match method {
            "ping" => protocol::result(id, "{}"),
            other => protocol::error(
                id,
                protocol::METHOD_NOT_FOUND,
                &format!("method not found: {other}"),
            ),
        };
        let post = Post::answer(body, method, era);
        // The answer to an answer is not read: the server got its reply.
        self.send_once(&post).await
    }

    /// The session id a legacy server gave, which every later request carries.
    fn session(&self) -> Option<HeaderValue> {
        self.session_mutex().clone()
    }

    fn session_mutex(&self) -> MutexGuard<'_, Option<HeaderValue>> {
        self.shared
            .session
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn login_mutex(&self) -> MutexGuard<'_, Login> {
        self.shared
            .login
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn warnings_mutex(&self) -> MutexGuard<'_, Vec<String>> {
        self.shared
            .warnings
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

/// One message to post, and how its answer is read.
struct Post<'a> {
    /// The request id, `None` for a notification or an answer.
    id: Option<u64>,
    /// The method, for the `Mcp-Method` header.
    method: &'a str,
    /// The tool name, for the `Mcp-Name` header.
    name: Option<&'a str>,
    /// The era, for the version header and for the messages on a stream.
    era: Era,
    /// Whether the request carries the version header.
    version: bool,
    /// The JSON-RPC message, ending with a newline.
    body: Bytes,
    /// Headers of one call, such as `Mcp-Param-Region`.
    extra: Vec<(HeaderName, HeaderValue)>,
}

impl<'a> Post<'a> {
    /// A request or a notification of `method`.
    fn new(id: Option<u64>, method: &'a str, params: Params<'a>, era: Era) -> Post<'a> {
        Post {
            id,
            method,
            name: params.tool_name(),
            era,
            version: era.sends_version(),
            body: protocol::frame(id, method, params, era).into(),
            extra: Vec::new(),
        }
    }

    /// An answer to a request the server sent.
    fn answer(body: Vec<u8>, method: &'a str, era: Era) -> Post<'a> {
        Post {
            id: None,
            method,
            name: None,
            era,
            version: era.sends_version(),
            body: body.into(),
            extra: Vec::new(),
        }
    }
}

/// What one POST gave back.
enum Answer {
    /// The server took the message with no answer, a `202`.
    Accepted,
    /// The response to the request.
    Reply(Reply),
}

/// One argument a call sends as an HTTP header.
#[derive(Clone, Debug)]
pub(crate) struct HeaderParam {
    /// The name after `Mcp-Param-`.
    name: String,
    /// The argument names from the root of the arguments object.
    path: Vec<String>,
}

/// `text` with each `${NAME}` replaced by `lookup(NAME)`.
///
/// A `${NAME}` whose lookup finds nothing is an error that names it.
pub fn expand_env(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String, Error> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            break;
        };
        out.push_str(&rest[..start]);
        let name = &after[..end];
        let value = lookup(name).ok_or_else(|| Error::Env {
            name: name.to_owned(),
        })?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The header parameters of one tool, or why the tool cannot be used.
pub(crate) fn param_headers(schema: &Value) -> Result<Vec<HeaderParam>, String> {
    let mut params = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(params);
    };
    for (property, child) in properties {
        let mut path = vec![property.clone()];
        scan(child, true, &mut path, &mut params, &mut seen)?;
    }
    Ok(params)
}

/// Scans one property schema; `clear` says no keyword above it hides the
/// argument from the client.
fn scan(
    node: &Value,
    clear: bool,
    path: &mut Vec<String>,
    params: &mut Vec<HeaderParam>,
    seen: &mut Vec<String>,
) -> Result<(), String> {
    if let Some(items) = node.as_array() {
        for item in items {
            scan(item, clear, path, params, seen)?;
        }
        return Ok(());
    }
    let Some(object) = node.as_object() else {
        return Ok(());
    };
    if let Some(header) = object.get("x-mcp-header") {
        if !clear {
            return Err(
                "an x-mcp-header sits under a keyword this client cannot follow".to_owned(),
            );
        }
        let name = header
            .as_str()
            .ok_or_else(|| "an x-mcp-header is not a string".to_owned())?;
        if name.is_empty() {
            return Err("an x-mcp-header is empty".to_owned());
        }
        if !is_token(name) {
            return Err(format!("the x-mcp-header {name} is not a token"));
        }
        if object.get("type").and_then(Value::as_str) == Some("number") {
            return Err(format!("the x-mcp-header {name} is on a number"));
        }
        if seen.iter().any(|other| other.eq_ignore_ascii_case(name)) {
            return Err(format!("the x-mcp-header {name} is repeated"));
        }
        seen.push(name.to_owned());
        params.push(HeaderParam {
            name: name.to_owned(),
            path: path.clone(),
        });
    }
    for (key, child) in object {
        match key.as_str() {
            "properties" => {
                if let Some(properties) = child.as_object() {
                    for (property, schema) in properties {
                        path.push(property.clone());
                        scan(schema, clear, path, params, seen)?;
                        path.pop();
                    }
                }
            }
            "items" | "anyOf" | "oneOf" | "allOf" | "not" | "if" | "then" | "else" | "$ref" => {
                scan(child, false, path, params, seen)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// The headers one call sends for its tool's `x-mcp-header` arguments.
pub(crate) fn call_headers(
    params: &[HeaderParam],
    arguments: &str,
) -> Vec<(HeaderName, HeaderValue)> {
    if params.is_empty() {
        return Vec::new();
    }
    let Ok(arguments) = serde_json::from_str::<Value>(arguments) else {
        return Vec::new();
    };
    let mut headers = Vec::with_capacity(params.len());
    for param in params {
        let Some(value) = at(&arguments, &param.path).and_then(header_value) else {
            continue;
        };
        let Ok(name) = HeaderName::from_bytes(format!("{PARAM_PREFIX}{}", param.name).as_bytes())
        else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(&value) else {
            continue;
        };
        headers.push((name, value));
    }
    headers
}

/// The value at `path` of the arguments object.
fn at<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(key)?;
    }
    Some(current)
}

/// One argument as the header value it becomes, `None` when it is omitted.
fn header_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(encode(text)),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(if *flag { "true" } else { "false" }.to_owned()),
        _ => None,
    }
}

/// `text` as a header name or value: as written when that is safe, else base64.
fn encode(text: &str) -> String {
    if is_plain(text) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(SENTINEL.len() + text.len() * 2 + SENTINEL_END.len());
    out.push_str(SENTINEL);
    STANDARD.encode_string(text.as_bytes(), &mut out);
    out.push_str(SENTINEL_END);
    out
}

/// Whether `text` travels as written in a header name or value.
fn is_plain(text: &str) -> bool {
    text.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        && !text.starts_with(' ')
        && !text.ends_with(' ')
        && !(text.starts_with(SENTINEL) && text.ends_with(SENTINEL_END))
}

/// Whether `text` is a header name token, per RFC 9110.
fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || TOKEN_PUNCTUATION.contains(&byte))
}

/// The JSON-RPC error a refused probe named, when it is one we recognize.
fn refused(body: &str) -> Option<RpcFailure> {
    let message = serde_json::from_str::<protocol::Message<'_>>(body).ok()?;
    let error = message.error?;
    if !protocol::MODERN_ERRORS.contains(&error.code) {
        return None;
    }
    Some(RpcFailure {
        code: error.code,
        message: error.message.into_owned(),
        data: error.data.map(|data| data.get().into()),
    })
}

/// The error for the old HTTP+SSE transport, which this client does not speak.
fn unsupported_transport() -> Error {
    Error::Protocol(
        "the server speaks the 2024-11-05 HTTP+SSE transport, which this client does not support"
            .to_owned(),
    )
}

/// The base URL and headers of one server, checked.
fn header_pairs(spec: &HttpSpec) -> Result<Vec<(HeaderName, HeaderValue)>, Error> {
    let mut pairs = Vec::with_capacity(spec.headers.len());
    for (name, value) in &spec.headers {
        let header = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| Error::Header(format!("{name} is not a header name")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| Error::Header(format!("the value of {name} is not a header value")))?;
        pairs.push((header, value));
    }
    Ok(pairs)
}

fn transport(error: reqwest::Error) -> Error {
    Error::Http(error.without_url().to_string())
}

/// The error for a body, an event or a line past the read cap.
fn too_large() -> Error {
    Error::Protocol(format!(
        "the server sent more than the {READ_CAP_MIB} MiB cap this client reads"
    ))
}

/// The body of `response`, refusing one past the read cap.
async fn bounded(mut response: reqwest::Response) -> Result<Bytes, Error> {
    let hint = response
        .content_length()
        .map_or(0, |length| usize::try_from(length).unwrap_or(READ_CAP))
        .min(READ_CAP);
    let Some(first) = response.chunk().await.map_err(transport)? else {
        return Ok(Bytes::new());
    };
    if first.len() > READ_CAP {
        return Err(too_large());
    }
    // Nearly every server sends the whole body in one chunk, which is then
    // used as it is.
    let Some(second) = response.chunk().await.map_err(transport)? else {
        return Ok(first);
    };
    if first.len() + second.len() > READ_CAP {
        return Err(too_large());
    }
    let mut body = Vec::with_capacity(hint.max(first.len() + second.len()));
    body.extend_from_slice(&first);
    body.extend_from_slice(&second);
    while let Some(chunk) = response.chunk().await.map_err(transport)? {
        if body.len() + chunk.len() > READ_CAP {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

/// The size of the event that goes on after `line`; a blank line ends it.
fn event_size(size: usize, line: &[u8]) -> Result<usize, Error> {
    if line.is_empty() {
        return Ok(0);
    }
    let size = size + line.len();
    if size > READ_CAP {
        return Err(too_large());
    }
    Ok(size)
}

fn protocol_error(error: serde_json::Error) -> Error {
    Error::Protocol(error.to_string())
}

/// Whether the status is one a request is sent again after.
fn retry_status(status: u16) -> bool {
    matches!(status, 408 | 429) || status >= 500
}

/// Whether `url` is on the scheme, host and port of `base`.
fn same_origin(base: &Url, url: &Url) -> bool {
    base.scheme() == url.scheme()
        && base.host_str() == url.host_str()
        && base.port_or_known_default() == url.port_or_known_default()
}

/// The `Location` of a reply, when it is text.
fn located(headers: &HeaderMap) -> Option<&str> {
    headers.get(LOCATION).and_then(|value| value.to_str().ok())
}

/// The host `location` names, resolved against `base`.
fn redirect_host(location: &str, base: &str) -> Option<String> {
    let next = Url::parse(base).ok()?.join(location).ok()?;
    next.host_str().map(str::to_owned)
}

/// How long the retry `attempt` waits: the header's seconds, else the default.
fn retry_wait(headers: &HeaderMap, attempt: usize) -> Duration {
    let asked = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds).min(RETRY_CAP));
    asked.unwrap_or(RETRY_WAITS[attempt.min(RETRIES - 1)])
}

/// Whether the reply is an event stream.
fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"))
}

/// The `resource_metadata` a `WWW-Authenticate` header names.
fn resource_metadata(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(reqwest::header::WWW_AUTHENTICATE)?
        .to_str()
        .ok()?;
    let params = reagent_oauth::auth_params(value);
    params
        .into_iter()
        .find(|(name, _)| name == "resource_metadata")
        .map(|(_, value)| value)
        .filter(|value| !value.is_empty())
}

/// The scopes a `WWW-Authenticate` challenge asks for.
fn asked_scopes(headers: &HeaderMap) -> Vec<String> {
    let Some(value) = headers
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
    else {
        return Vec::new();
    };
    let params = reagent_oauth::auth_params(value);
    params
        .iter()
        .find(|(name, _)| name == "scope")
        .map(|(_, scope)| scope.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The scopes a step-up challenge asks for, when the status is a step-up.
fn insufficient_scope(headers: &HeaderMap) -> Option<Vec<String>> {
    let value = headers
        .get(reqwest::header::WWW_AUTHENTICATE)?
        .to_str()
        .ok()?;
    let params = reagent_oauth::auth_params(value);
    let error = params
        .iter()
        .find(|(name, _)| name == "error")
        .map(|(_, value)| value.as_str())?;
    if error != "insufficient_scope" {
        return None;
    }
    let scopes = asked_scopes(headers);
    (!scopes.is_empty()).then_some(scopes)
}

/// The first `BODY_LIMIT` bytes of a reply body.
async fn head_text(mut response: reqwest::Response) -> String {
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                body.extend_from_slice(&chunk);
                if body.len() >= BODY_LIMIT {
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => return format!("the body could not be read: {}", error.without_url()),
        }
    }
    String::from_utf8_lossy(&body[..body.len().min(BODY_LIMIT)]).into_owned()
}
