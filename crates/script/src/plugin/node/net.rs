//! The network host of a plugin's `fetch`: the host check, the redirects, the
//! open responses, the cookies and the client, all in Rust.
//!
//! The JS half is the wrapper [`NET`], which builds the web objects and calls
//! this module by its own name; a plugin has no name for either module, so
//! nothing it patches in JS can decide a host, reach a response it does not
//! hold or read another plugin's cookies. Every request checks the plugin's
//! `net` list before a connection, and every redirect hop checks it again.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use cookie_store::{CookieStore, RawCookie};
use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, LOCATION};
use reqwest::{Method, Response, StatusCode};
use rquickjs::function::Opt;
use rquickjs::module::{Declarations, Exports, ModuleDef};
use rquickjs::promise::Promised;
use rquickjs::{Array, Ctx, Function, IntoJs, Object, Result as JsResult, TypedArray, Value};
use sha2::{Digest as _, Sha256, Sha384, Sha512};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::super::manifest::Host;
use super::NODE;

/// The module of the `fetch` wrapper: the JS half of this host.
pub(super) const NET: &str = "reagent:raw/net";

/// What the wrapper imports: the host functions below.
pub(super) const NET_HOST: &str = "reagent:raw/net-host";

/// The most responses one plugin may have open at once.
const OPEN: usize = 64;

/// The most redirects one fetch follows.
const HOPS: u32 = 20;

/// The most bytes `text`, `json`, `arrayBuffer` and `bytes` gather.
const CAP: usize = 32 * 1024 * 1024;

/// How many chunks of a stream body may wait for the request to take them: one
/// chunk of back pressure.
const FEED: usize = 1;

/// The name of the file a plugin's cookies with an expiry are saved in, under
/// the plugin's own folder.
const JAR: &str = "cookies.json";

/// The host functions the wrapper calls. A plugin has no name for this module,
/// so it cannot call one.
pub(super) struct NetHost;

impl ModuleDef for NetHost {
    fn declare(declare: &Declarations) -> JsResult<()> {
        for name in [
            "cancel", "close", "gather", "owner", "pull", "push", "reserve", "send",
        ] {
            declare.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &Exports<'js>) -> JsResult<()> {
        exports.export("owner", Function::new(ctx.clone(), owner)?)?;
        exports.export("reserve", Function::new(ctx.clone(), reserve)?)?;
        exports.export("cancel", Function::new(ctx.clone(), cancel)?)?;
        exports.export("send", Function::new(ctx.clone(), send)?)?;
        exports.export("push", Function::new(ctx.clone(), push)?)?;
        exports.export("close", Function::new(ctx.clone(), close)?)?;
        exports.export("pull", Function::new(ctx.clone(), pull)?)?;
        exports.export("gather", Function::new(ctx.clone(), gather)?)?;
        Ok(())
    }
}

/// The index of the plugin whose runtime runs this call: a runtime holds one
/// plugin's code and no other's, so every frame of it acts for that plugin,
/// `eval` code and the body of a `new Function` included.
fn caller(ctx: &Ctx<'_>) -> Option<usize> {
    super::index_of(ctx)
}

/// `owner()`: the index of the plugin whose runtime runs this call.
fn owner(ctx: Ctx<'_>) -> Option<u32> {
    u32::try_from(caller(&ctx)?).ok()
}

/// Whether the slot of `id` is held by the plugin at `index`.
fn owned(id: u64, index: usize) -> bool {
    STATE.with(|net| {
        net.borrow()
            .slots
            .get(&id)
            .is_some_and(|slot| slot.plugin == index)
    })
}

/// The plugin a slot function may act for: the runtime's plugin, when the slot
/// is that plugin's.
fn mine(ctx: &Ctx<'_>, id: u64) -> Option<usize> {
    let found = caller(ctx)?;
    STATE.with(|net| {
        let net = net.borrow();
        let slot = net.slots.get(&id)?;
        (slot.plugin == found).then_some(found)
    })
}

/// `reserve()`: a fresh id for one fetch, or `undefined` when the calling
/// plugin holds 64 open responses already. An id is never reused in the life of
/// the thread, so a late cancel or release of an old fetch touches nothing.
fn reserve(ctx: Ctx<'_>) -> Option<u64> {
    let index = owner(ctx)? as usize;
    STATE.with(|net| {
        let mut net = net.borrow_mut();
        let held = net.open.get(&index).copied().unwrap_or(0);
        if held >= OPEN {
            // A fetch whose response arrived while its own upload still runs is
            // one nobody reads: a call that was stopped leaves exactly those
            // behind, with nothing left in JS to read or cancel them. The new
            // fetch takes the oldest of them, so a stopped call cannot lock its
            // plugin out of fetching; the count stays at the cap, and only the
            // plugin's own slots are ever given up.
            let oldest = net
                .slots
                .iter()
                .filter(|(_, slot)| {
                    slot.plugin == index && slot.body.is_some() && slot.feed.is_some()
                })
                .map(|(id, _)| *id)
                .min();
            let oldest = oldest?;
            drop(net.slots.remove(&oldest));
        } else {
            net.open.insert(index, held + 1);
        }
        let id = net.next;
        net.next += 1;
        let entry = Slot {
            plugin: index,
            cancel: CancellationToken::new(),
            feed: None,
            digest: None,
            body: None,
        };
        #[cfg(test)]
        LIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        net.slots.insert(id, entry);
        Some(id)
    })
}

/// `cancel(id)`: the fetch or the body read of this id stops at once. An id
/// that is gone, or that belongs to another plugin, does nothing.
fn cancel(ctx: Ctx<'_>, id: u64) {
    if let Some(index) = mine(&ctx, id)
        && let Some(slot) = take(id, index)
    {
        slot.cancel.cancel();
    }
}

/// `send(index, id, request)`: the request, its redirect hops and the host
/// check of each of them. It answers the response's head with the body left in
/// the slot, or the reason it was refused.
fn send<'js>(ctx: Ctx<'js>, index: u32, id: u64, request: Object<'js>) -> JsResult<Value<'js>> {
    let index = super::acting(&ctx, index);
    let by = index as usize;
    if !owned(id, by) {
        let held = ctx.clone();
        return Promised(async move {
            Outcome::Refused(format!(
                "plugin {index}: that response belongs to another plugin"
            ))
            .into_js(&held)
        })
        .into_js(&ctx);
    }
    let method: String = request.get("method")?;
    let url: String = request.get("url")?;
    let redirect: String = request.get("redirect")?;
    let headers: Array<'js> = request.get("headers")?;
    let body: Option<Vec<u8>> = request.get("body")?;
    let streamed: Option<bool> = request.get("stream")?;
    let credentials: Option<String> = request.get("credentials")?;
    let referrer: Option<String> = request.get("referrer")?;
    let cache: Option<String> = request.get("cache")?;
    let integrity: Option<String> = request.get("integrity")?;
    let mut pairs = Vec::with_capacity(headers.len());
    for pair in headers.iter::<Array<'js>>() {
        let pair = pair?;
        pairs.push((pair.get::<String>(0)?, pair.get::<String>(1)?));
    }
    // A body the plugin streams in arrives over the feed of its slot, one chunk
    // per `push`, and grows none in memory: Rust pulls the next chunk only when
    // the request takes it.
    let body = match streamed {
        Some(true) => {
            let (feed, chunks) = mpsc::channel(FEED);
            if !hold(id, feed, by) {
                let held = ctx.clone();
                return Promised(async move { Outcome::Aborted.into_js(&held) }).into_js(&ctx);
            }
            Payload::Stream(Some(chunks))
        }
        _ => match body {
            Some(bytes) => Payload::Bytes(bytes),
            None => Payload::None,
        },
    };
    digest(id, integrity.as_deref(), by);
    let ask = Ask {
        method,
        url,
        headers: pairs,
        redirect: Mode::of(&redirect),
        credentials: Credentials::of(credentials.as_deref()),
        referrer: Header::new(referrer.as_deref()),
        cache: cache.unwrap_or_default(),
        body,
    };
    let held = ctx.clone();
    let sent = async move {
        let ctx = held;
        let Some(cancel) = slot_cancel(id, by) else {
            return Outcome::Aborted.into_js(&ctx);
        };
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => Outcome::Aborted,
            outcome = Box::pin(exchange(index, id, ask)) => outcome,
        };
        // Only a response keeps the slot: every other answer frees it at once,
        // so a plugin refused 64 times can still fetch.
        if !matches!(outcome, Outcome::Response { .. }) {
            release(id, by);
        }
        outcome.into_js(&ctx)
    };
    Promised(sent).into_js(&ctx)
}

/// `push(id, chunk)`: one chunk of a stream body, taken by the request; it
/// answers `false` once the request stopped reading, so the plugin stops
/// feeding it.
fn push<'js>(ctx: Ctx<'js>, id: u64, chunk: Vec<u8>) -> JsResult<Value<'js>> {
    let Some(index) = mine(&ctx, id) else {
        return Ok(Value::new_bool(ctx.clone(), false));
    };
    let sent = async move {
        let Some(feed) = feed(id, index) else {
            return false;
        };
        feed.send(Ok(chunk)).await.is_ok()
    };
    Promised(sent).into_js(&ctx)
}

/// `close(id, reason)`: the end of a stream body, or, with a reason, the
/// failure that ends the request.
fn close(ctx: Ctx<'_>, id: u64, reason: Opt<String>) -> bool {
    let Some(index) = mine(&ctx, id) else {
        return false;
    };
    let feed = STATE.with(|net| {
        let mut net = net.borrow_mut();
        net.slots
            .get_mut(&id)
            .filter(|entry| entry.plugin == index)
            .and_then(|entry| entry.feed.take())
    });
    let Some(feed) = feed else {
        return false;
    };
    if let Some(reason) = reason.0 {
        let _ = feed.try_send(Err(std::io::Error::other(reason)));
    }
    drop(feed);
    true
}

/// `pull(id)`: one chunk of the body, `null` at its end, or the marker of a
/// cancel, a read error or a slot of another plugin.
fn pull<'js>(ctx: Ctx<'js>, id: u64) -> JsResult<Value<'js>> {
    let Some(index) = mine(&ctx, id) else {
        return marker(&ctx, FOREIGN, false);
    };
    let held = ctx.clone();
    let read = async move {
        let ctx = held;
        let Some((cancel, mut response)) = body(id, index) else {
            release(id, index);
            return marker(&ctx, "abort", true);
        };
        let chunk = tokio::select! {
            biased;
            () = cancel.cancelled() => None,
            chunk = Box::pin(response.chunk()) => Some(chunk),
        };
        match chunk {
            None => {
                release(id, index);
                marker(&ctx, "abort", true)
            }
            Some(Ok(Some(bytes))) => {
                feed_digest(id, &bytes, index);
                restore(id, response, index);
                Ok(TypedArray::new_copy(ctx.clone(), bytes.as_ref())?.into_value())
            }
            Some(Ok(None)) => {
                let refused = end_digest(id, index);
                release(id, index);
                match refused {
                    Some(reason) => marker(&ctx, &reason, false),
                    None => Ok(Value::new_null(ctx.clone())),
                }
            }
            Some(Err(error)) => {
                release(id, index);
                marker(
                    &ctx,
                    &format!("the response body could not be read: {error}"),
                    false,
                )
            }
        }
    };
    Promised(read).into_js(&ctx)
}

/// `gather(id)`: the whole body as one `Uint8Array`, refused once it passes
/// 32 MiB, or the marker of a cancel, a read error or a slot of another plugin.
fn gather<'js>(ctx: Ctx<'js>, id: u64) -> JsResult<Value<'js>> {
    let Some(index) = mine(&ctx, id) else {
        return marker(&ctx, FOREIGN, false);
    };
    let held = ctx.clone();
    let read = async move {
        let ctx = held;
        let Some((cancel, mut response)) = body(id, index) else {
            release(id, index);
            return marker(&ctx, "abort", true);
        };
        let mut out: Vec<u8> = Vec::new();
        loop {
            let chunk = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                chunk = Box::pin(response.chunk()) => Some(chunk),
            };
            match chunk {
                None => {
                    release(id, index);
                    return marker(&ctx, "abort", true);
                }
                Some(Ok(Some(bytes))) => {
                    if out.len() + bytes.len() > CAP {
                        release(id, index);
                        return marker(
                            &ctx,
                            "the response body is larger than 33554432 bytes (32 MiB)",
                            false,
                        );
                    }
                    feed_digest(id, &bytes, index);
                    out.extend_from_slice(&bytes);
                }
                Some(Ok(None)) => {
                    let refused = end_digest(id, index);
                    release(id, index);
                    if let Some(reason) = refused {
                        return marker(&ctx, &reason, false);
                    }
                    return Ok(TypedArray::new_copy(ctx.clone(), out)?.into_value());
                }
                Some(Err(error)) => {
                    release(id, index);
                    return marker(
                        &ctx,
                        &format!("the response body could not be read: {error}"),
                        false,
                    );
                }
            }
        }
    };
    Promised(read).into_js(&ctx)
}

/// The reason `pull` and `gather` answer with for a slot another plugin holds.
const FOREIGN: &str = "that response belongs to another plugin";

/// The object `pull` and `gather` answer with instead of a chunk: `abort` names
/// a cancel, `error` the reason a read failed.
fn marker<'js>(ctx: &Ctx<'js>, text: &str, aborted: bool) -> JsResult<Value<'js>> {
    let object = Object::new(ctx.clone())?;
    if aborted {
        object.set("abort", true)?;
    } else {
        object.set("error", text)?;
    }
    Ok(object.into_value())
}

/// What one `send` answered: the head of a response whose body waits in the
/// slot, a cancel, or the reason the plugin may not make the request.
enum Outcome {
    Response {
        status: u16,
        status_text: String,
        headers: Vec<(String, String)>,
        url: String,
        redirected: bool,
    },
    Aborted,
    Refused(String),
}

impl Outcome {
    fn into_js<'js>(self, ctx: &Ctx<'js>) -> JsResult<Value<'js>> {
        let object = Object::new(ctx.clone())?;
        match self {
            Outcome::Response {
                status,
                status_text,
                headers,
                url,
                redirected,
            } => {
                object.set("ok", true)?;
                object.set("status", status)?;
                object.set("statusText", status_text)?;
                object.set("headers", pairs(ctx, &headers)?)?;
                object.set("url", url)?;
                object.set("redirected", redirected)?;
            }
            Outcome::Aborted => {
                object.set("ok", false)?;
                object.set("abort", true)?;
            }
            Outcome::Refused(message) => {
                object.set("ok", false)?;
                object.set("message", message)?;
            }
        }
        Ok(object.into_value())
    }
}

/// The headers of one response as the array of pairs a `Headers` is built from.
fn pairs<'js>(ctx: &Ctx<'js>, headers: &[(String, String)]) -> JsResult<Array<'js>> {
    let array = Array::new(ctx.clone())?;
    for (index, (name, value)) in headers.iter().enumerate() {
        let pair = Array::new(ctx.clone())?;
        pair.set(0, name.as_str())?;
        pair.set(1, value.as_str())?;
        array.set(index, pair)?;
    }
    Ok(array)
}

/// How a plugin wants a redirect answered.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Follow it, checking the host of every hop.
    Follow,
    /// Answer the 3xx itself.
    Manual,
    /// Refuse the fetch.
    Error,
}

impl Mode {
    fn of(text: &str) -> Self {
        match text {
            "manual" => Mode::Manual,
            "error" => Mode::Error,
            _ => Mode::Follow,
        }
    }
}

/// What one fetch asks for, after the wrapper read its options.
struct Ask {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    redirect: Mode,
    credentials: Credentials,
    /// The `referer` header the request carries, if any.
    referrer: Header,
    /// The `cache` mode, which decides the `cache-control` and `pragma`
    /// headers a request carries.
    cache: String,
    body: Payload,
}

/// The body a request carries, and whether it may be sent again.
enum Payload {
    /// No body.
    None,
    /// The bytes the wrapper gathered.
    Bytes(Vec<u8>),
    /// A stream the plugin feeds one chunk at a time: it cannot be replayed.
    Stream(Option<mpsc::Receiver<Feed>>),
}

/// What the request reads for one hop.
enum Hop {
    None,
    Bytes(Vec<u8>),
    Stream(mpsc::Receiver<Feed>),
}

/// One chunk of a stream body, or the failure that ends it.
type Feed = Result<Vec<u8>, std::io::Error>;

/// Where the cookies of a request come from, and where its response's go.
#[derive(Clone, Copy, PartialEq)]
enum Credentials {
    /// `omit`: neither sends nor stores.
    Omit,
    /// `same-origin` and `include`: the plugin's own jar.
    Jar,
}

impl Credentials {
    fn of(text: Option<&str>) -> Self {
        match text {
            Some("omit") => Credentials::Omit,
            _ => Credentials::Jar,
        }
    }
}

/// The value of the one header a request sets outside its own headers.
struct Header(Option<String>);

impl Header {
    fn new(referrer: Option<&str>) -> Self {
        match referrer {
            None | Some("" | "about:client") => Header(None),
            Some(text) => match Url::parse(text) {
                Ok(url) if matches!(url.scheme(), "http" | "https") => {
                    Header(Some(url.to_string()))
                }
                _ => Header(None),
            },
        }
    }
}

/// The request, its hops and its checks.
async fn exchange(index: u32, id: u64, ask: Ask) -> Outcome {
    let by = index as usize;
    let Ask {
        method,
        url,
        mut headers,
        redirect,
        credentials,
        referrer,
        cache,
        mut body,
    } = ask;
    let Some((name, hosts)) = policy(index) else {
        return Outcome::Refused(format!("plugin {index} may not fetch anything"));
    };
    let Ok(mut url) = Url::parse(&url) else {
        return Outcome::Refused(format!("plugin {name} may not fetch: that is not a URL"));
    };
    if let Err(reason) = reach(&name, &hosts, &url) {
        return Outcome::Refused(reason);
    }
    let Ok(mut method) = Method::from_bytes(method.as_bytes()) else {
        return Outcome::Refused(format!("plugin {name} may not fetch with that method"));
    };
    // A header of the connection, or one that is not a header at all, refuses
    // the request before any connection.
    if let Some(reason) = unwelcome(&name, &headers) {
        return Outcome::Refused(reason);
    }
    // The options the wrapper validated: the headers they set, unless the
    // caller set one of its own.
    if let Header(Some(value)) = referrer
        && !named(&headers, "referer")
    {
        headers.push(("referer".to_string(), value));
    }
    if matches!(cache.as_str(), "no-store" | "no-cache" | "reload") {
        for name in ["cache-control", "pragma"] {
            if !named(&headers, name) {
                headers.push((name.to_string(), "no-cache".to_string()));
            }
        }
    }
    let mut redirected = false;
    for _ in 0..=HOPS {
        // The client of this hop: the plugin's own for a host its `net` list
        // names, and the guarded one for a host only `*` allows, whose resolver
        // judges the addresses this request connects to.
        let client = match reach(&name, &hosts, &url) {
            Ok(Reach::Listed) => client(),
            Ok(Reach::Any) => guarded(),
            Err(reason) => return Outcome::Refused(reason),
        };
        // Every hop checks the headers again: a hop that follows a redirect
        // carries the ones the request still has.
        if let Some(reason) = unwelcome(&name, &headers) {
            return Outcome::Refused(reason);
        }
        let hop = match &mut body {
            Payload::None => Hop::None,
            Payload::Bytes(bytes) => Hop::Bytes(bytes.clone()),
            Payload::Stream(feed) => match feed.take() {
                Some(chunks) => Hop::Stream(chunks),
                None => {
                    return Outcome::Refused(format!(
                        "plugin {name}: a stream body cannot be replayed after a redirect from {url}"
                    ));
                }
            },
        };
        // The cookies of this hop: one header, and never one the caller set.
        let mut hop_headers = headers.clone();
        if credentials == Credentials::Jar
            && !named(&hop_headers, "cookie")
            && let Some(value) = cookies(index, &url)
        {
            hop_headers.push(("cookie".to_string(), value));
        }
        let mut request = client
            .request(method.clone(), url.clone())
            .headers(map_of(&hop_headers));
        request = match hop {
            Hop::None => request,
            Hop::Bytes(bytes) => request.body(bytes),
            Hop::Stream(chunks) => request.body(stream_body(chunks)),
        };
        let response = match Box::pin(request.send()).await {
            Ok(response) => response,
            Err(error) => {
                return Outcome::Refused(format!(
                    "plugin {name} could not fetch {url}: {}",
                    failed(&error)
                ));
            }
        };
        // Every hop's `set-cookie` goes into the plugin's own jar, saved as
        // soon as it changed, so a restart keeps what the server gave.
        if credentials == Credentials::Jar {
            remember(index, &url, response.headers()).await;
        }
        let status = response.status();
        if !status.is_redirection() || redirect == Mode::Manual {
            return finish(id, response, redirected, by);
        }
        if redirect == Mode::Error {
            return Outcome::Refused(format!("plugin {name} refuses the redirect from {url}"));
        }
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let Some(location) = location else {
            return finish(id, response, redirected, by);
        };
        let Ok(next) = url.join(&location) else {
            return Outcome::Refused(format!(
                "plugin {name} may not follow the redirect {location:?} from {url}"
            ));
        };
        if next.host_str() != url.host_str() {
            headers.retain(|(name, _)| !sensitive(name));
        }
        // A 303, and a 301 or 302 that answers a POST, continue as a plain GET.
        if status == StatusCode::SEE_OTHER
            || (matches!(status, StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND)
                && method == Method::POST)
        {
            method = Method::GET;
            body = Payload::None;
            headers.retain(|(name, _)| !describes(name));
        }
        redirected = true;
        url = next;
    }
    Outcome::Refused(format!(
        "plugin {name} followed too many redirects from {url}"
    ))
}

/// Whether the request carries a header of this name.
fn named(headers: &[(String, String)], name: &str) -> bool {
    headers
        .iter()
        .any(|(held, _)| held.eq_ignore_ascii_case(name))
}

/// The reqwest body of a stream the plugin feeds: one chunk per `push`, and the
/// next pulled only when the request asks for it.
fn stream_body(chunks: mpsc::Receiver<Feed>) -> reqwest::Body {
    reqwest::Body::wrap_stream(futures_util::stream::unfold(chunks, |mut chunks| async {
        let chunk = chunks.recv().await?;
        Some((chunk, chunks))
    }))
}

/// Whether a header of another host must not follow a redirect.
fn sensitive(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "cookie" | "referer"
    )
}

/// Whether a header describes the body a 303 drops.
fn describes(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "content-type" | "content-length"
    )
}

/// The headers that describe the connection itself: only this host sets one,
/// since a plugin that set one could name another host or smuggle a second
/// request into the first.
const FORBIDDEN: [&str; 11] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "upgrade",
    "expect",
    "keep-alive",
    "te",
    "trailer",
    "proxy-authorization",
    "proxy-connection",
];

/// Whether `value` is a header value: no NUL and no line break.
fn value_ok(value: &str) -> bool {
    !value.bytes().any(|byte| matches!(byte, 0 | b'\r' | b'\n'))
}

/// The reason a request may not carry `headers`, and `None` when it may: a
/// header of the connection, a name that is not a header name, or a value that
/// is not a header value.
fn unwelcome(name: &str, headers: &[(String, String)]) -> Option<String> {
    for (held, value) in headers {
        let lower = held.to_ascii_lowercase();
        if FORBIDDEN.contains(&lower.as_str()) {
            return Some(format!(
                "plugin {name} may not set the {lower} header: this host decides it"
            ));
        }
        if HeaderName::from_bytes(held.as_bytes()).is_err() {
            return Some(format!(
                "plugin {name} may not send the header {held:?}: that is not a header name"
            ));
        }
        if !value_ok(value) {
            return Some(format!(
                "plugin {name} may not send the header {held:?}: that is not a header value"
            ));
        }
    }
    None
}

/// The headers of the request, with every name and value a host cannot send
/// left out.
fn map_of(headers: &[(String, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            map.append(name, value);
        }
    }
    map
}

/// The name of the plugin at `index` and the hosts it may fetch from.
fn policy(index: u32) -> Option<(String, Vec<Host>)> {
    NODE.with(|node| {
        let node = node.borrow();
        let policy = node.get(index as usize)?;
        Some((policy.name.clone(), policy.net.clone()))
    })
}

/// Whether the plugin's `net` list names the host of `url`. A wildcard matches
/// a name that ends with `.` and the entry, so `example.com` itself and
/// `badexample.com` do not match `*.example.com`.
fn listed(hosts: &[Host], url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let bare = bare(host);
    match bare.parse::<IpAddr>() {
        Ok(address) => hosts
            .iter()
            .any(|host| matches!(host, Host::Ip(allowed) if *allowed == address)),
        Err(_) => hosts.iter().any(|host| match host {
            Host::Name(name) => name == bare,
            Host::Wildcard(name) => {
                bare.len() > name.len()
                    && bare.ends_with(name.as_str())
                    && bare.as_bytes()[bare.len() - name.len() - 1] == b'.'
            }
            Host::Ip(_) => false,
            Host::Any => false,
        }),
    }
}

/// The host of a URL without the brackets of an IPv6 literal.
fn bare(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host)
}

/// Whether `url` names this machine: plain HTTP reaches no further than that.
fn loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let bare = bare(host);
    match bare.parse::<IpAddr>() {
        Ok(address) => address.is_loopback(),
        Err(_) => bare == "localhost",
    }
}

/// How a fetch reaches the host of a URL: the plugin's own client, for a host
/// its `net` list names, or the guarded one, for a host only `*` allows.
#[derive(Debug)]
pub(super) enum Reach {
    /// A host the plugin's `net` list names, with the rules such a host always
    /// had.
    Listed,
    /// A host only `*` allows: over https, and only at a public address.
    Any,
}

/// How `url` may be fetched for this plugin, or the reason it may not: a host
/// the list names, else a host `*` allows, over https and at a public address.
/// Every hop of a fetch asks this, so a redirect is judged the same way.
pub(super) fn reach(name: &str, hosts: &[Host], url: &Url) -> Result<Reach, String> {
    let Some(host) = url.host_str() else {
        return Err(format!(
            "plugin {name} may not fetch from a URL with no host"
        ));
    };
    if listed(hosts, url) {
        return match url.scheme() {
            "https" => Ok(Reach::Listed),
            "http" if loopback(url) => Ok(Reach::Listed),
            "http" => Err(format!(
                "plugin {name} may fetch from {host} over https only"
            )),
            scheme => Err(format!("plugin {name} may not fetch over {scheme}")),
        };
    }
    if !hosts.contains(&Host::Any) {
        return Err(format!("plugin {name} may not fetch from {host}"));
    }
    // A host only `*` allows is reached over https: a plain request writes the
    // name of the host on the way, for anybody to read.
    match url.scheme() {
        "https" => {}
        "http" => {
            return Err(format!(
                "plugin {name} may fetch from {host} over https only"
            ));
        }
        scheme => return Err(format!("plugin {name} may not fetch over {scheme}")),
    }
    // An address the URL writes is judged here, since nothing resolves it; a
    // name is judged on its addresses at the connect, by [`Public`].
    match bare(host).parse::<IpAddr>() {
        Ok(address) if !public(address) => Err(format!(
            "plugin {name} may not fetch from {host}: that is not a public address"
        )),
        _ => Ok(Reach::Any),
    }
}

/// Whether `address` is a public address: not this machine, not a network of
/// this machine, and not a network that reaches it.
pub(super) fn public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let [first, second, ..] = address.octets();
            !(address.is_unspecified()
                || address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_broadcast()
                || address.is_multicast()
                || first == 0
                || (first == 100 && (64..=127).contains(&second)))
        }
        IpAddr::V6(address) => {
            let held = address.segments();
            // A v4 address carried by a v6 one, as `::ffff:a.b.c.d` and the
            // NAT64 `64:ff9b::/96` carry it, follows the rules of the v4 one.
            if let Some(address) = address.to_ipv4_mapped() {
                return public(IpAddr::V4(address));
            }
            if held[0] == 0x64 && held[1] == 0xff9b && held[2..6].iter().all(|part| *part == 0) {
                let inside = std::net::Ipv4Addr::new(
                    (held[6] >> 8) as u8,
                    held[6] as u8,
                    (held[7] >> 8) as u8,
                    held[7] as u8,
                );
                return public(IpAddr::V4(inside));
            }
            // The link-local `fe80::/10`, the old site-local `fec0::/10` and
            // the unique-local `fc00::/7`.
            !(address.is_unspecified()
                || address.is_loopback()
                || address.is_multicast()
                || held[0] & 0xffc0 == 0xfe80
                || held[0] & 0xffc0 == 0xfec0
                || held[0] & 0xfe00 == 0xfc00)
        }
    }
}

/// Resolves the name of a fetch that only `*` allows, and refuses the whole
/// answer when one of its addresses is not public: the addresses judged here
/// are the ones the request connects to, so DNS cannot answer one address at
/// the check and another at the connect.
#[derive(Debug)]
pub(super) struct Public;

impl reqwest::dns::Resolve for Public {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|error| {
                    std::io::Error::other(format!("{host} cannot be resolved: {error}"))
                })?;
            let found: Vec<std::net::SocketAddr> = found.collect();
            if found.iter().any(|address| !public(address.ip())) {
                return Err(Box::new(std::io::Error::other(format!(
                    "{host} resolves to an address that is not public"
                )))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(found.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// The client of this plugin thread for the hosts only `*` allows, made at the
/// first such fetch: its resolver judges every address a name answers.
fn guarded() -> reqwest::Client {
    STATE.with(|net| {
        let mut net = net.borrow_mut();
        if net.guarded.is_none() {
            net.guarded = Some(reagent_oauth::http::client_with_resolver(
                std::sync::Arc::new(Public),
            ));
        }
        net.guarded
            .clone()
            .expect("the client is made just before it is cloned")
    })
}

/// The reason a request failed, with every cause under it: a resolver refuses
/// under reqwest's own error, and the plugin has to read why.
fn failed(error: &reqwest::Error) -> String {
    let mut reason = error.to_string();
    let mut cause = std::error::Error::source(error);
    while let Some(error) = cause {
        reason.push_str(": ");
        reason.push_str(&error.to_string());
        cause = std::error::Error::source(error);
    }
    reason
}

/// The response as the head `send` answers with, its body left in the slot.
fn finish(id: u64, response: Response, redirected: bool, index: usize) -> Outcome {
    let status = response.status();
    let status_text = status.canonical_reason().unwrap_or_default().to_string();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let url = response.url().to_string();
    if !stash(id, response, index) {
        return Outcome::Aborted;
    }
    Outcome::Response {
        status: status.as_u16(),
        status_text,
        headers,
        url,
        redirected,
    }
}

/// The client of this plugin thread, made at the first fetch.
fn client() -> reqwest::Client {
    STATE.with(|net| {
        let mut net = net.borrow_mut();
        if net.client.is_none() {
            net.client = Some(reagent_oauth::http::client());
        }
        net.client
            .clone()
            .expect("the client is made just before it is cloned")
    })
}

/// One open response: the plugin that made it, its cancel, the feed of a stream
/// body, the integrity check of the body and the body still to read.
struct Slot {
    plugin: usize,
    cancel: CancellationToken,
    feed: Option<mpsc::Sender<Feed>>,
    digest: Option<Match>,
    body: Option<Response>,
}

/// Every slot this process holds, for the test that nothing outlives the
/// plugins: nextest runs each test in a process of its own.
#[cfg(test)]
static LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Drop for Slot {
    fn drop(&mut self) {
        #[cfg(test)]
        LIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// How many responses are open in this process, for the tests.
#[cfg(test)]
pub(super) fn open_now() -> usize {
    LIVE.load(std::sync::atomic::Ordering::SeqCst)
}

/// The net state of the one context of this thread. Rust data only, so nothing
/// here is reachable from JS.
struct NetState {
    client: Option<reqwest::Client>,
    /// The client of the hosts only `*` allows, made at the first such fetch.
    guarded: Option<reqwest::Client>,
    /// The open slots, by id: an id names one fetch for the life of the thread.
    slots: HashMap<u64, Slot>,
    /// The next id to hand out: never reused, so a late cancel of an old fetch
    /// cannot touch a newer one.
    next: u64,
    /// How many responses each plugin holds open.
    open: HashMap<usize, usize>,
    /// The cookies of each plugin, by its index: one jar, never shared.
    jars: HashMap<usize, Jar>,
}

impl NetState {
    fn new() -> Self {
        Self {
            client: None,
            guarded: None,
            slots: HashMap::new(),
            next: 0,
            open: HashMap::new(),
            jars: HashMap::new(),
        }
    }
}

thread_local! {
    /// The net state of the plugin thread, one entry per plugin index.
    static STATE: RefCell<NetState> = RefCell::new(NetState::new());
}

/// Drops the clients every plugin shares, once the last plugin stopped: they
/// must go while the runtime they ran on is still alive. The ids stay spent.
pub(super) fn drop_shared() {
    STATE.with(|net| {
        let mut net = net.borrow_mut();
        let next = net.next;
        *net = NetState::new();
        net.next = next;
    });
}

/// Frees the open responses, the jar and the client count of the plugin at
/// `index`: nothing of it may outlive its context.
pub(super) fn free(index: usize) {
    STATE.with(|net| {
        let mut net = net.borrow_mut();
        net.open.remove(&index);
        drop(net.jars.remove(&index));
        let mine: Vec<u64> = net
            .slots
            .iter()
            .filter(|(_, slot)| slot.plugin == index)
            .map(|(id, _)| *id)
            .collect();
        for id in mine {
            drop(net.slots.remove(&id));
        }
    });
}

/// The cancel of this slot, while it is open and held by `index`.
fn slot_cancel(id: u64, index: usize) -> Option<CancellationToken> {
    STATE.with(|net| {
        let net = net.borrow();
        net.slots
            .get(&id)
            .filter(|entry| entry.plugin == index)
            .map(|entry| entry.cancel.clone())
    })
}

/// Takes the response of this slot out of the table, so a read may wait for it
/// without holding the table.
fn body(id: u64, index: usize) -> Option<(CancellationToken, Response)> {
    STATE.with(|net| {
        let mut net = net.borrow_mut();
        let entry = net
            .slots
            .get_mut(&id)
            .filter(|entry| entry.plugin == index)?;
        Some((entry.cancel.clone(), entry.body.take()?))
    })
}

/// Puts the response back while it still has a body to read.
fn restore(id: u64, response: Response, index: usize) {
    STATE.with(|net| {
        let mut net = net.borrow_mut();
        if let Some(entry) = net.slots.get_mut(&id).filter(|entry| entry.plugin == index) {
            entry.body = Some(response);
        }
    });
}

/// Leaves the response of a finished request in its slot; `false` when the
/// fetch was cancelled and the slot is gone.
fn stash(id: u64, response: Response, index: usize) -> bool {
    STATE.with(|net| {
        let mut net = net.borrow_mut();
        match net.slots.get_mut(&id).filter(|entry| entry.plugin == index) {
            Some(entry) => {
                entry.body = Some(response);
                true
            }
            None => false,
        }
    })
}

/// Empties this slot, when the body ended, the fetch failed or its plugin
/// cancelled it.
fn take(id: u64, index: usize) -> Option<Slot> {
    STATE.with(|net| {
        let mut net = net.borrow_mut();
        let entry = match net.slots.get(&id) {
            Some(entry) if entry.plugin == index => net.slots.remove(&id)?,
            _ => return None,
        };
        if let Some(open) = net.open.get_mut(&entry.plugin) {
            *open = open.saturating_sub(1);
        }
        Some(entry)
    })
}

/// Empties this slot, when the body ended or the fetch failed.
fn release(id: u64, index: usize) {
    drop(take(id, index));
}

/// Holds the feed of a stream body in its slot; `false` when the slot is gone.
fn hold(id: u64, sender: mpsc::Sender<Feed>, index: usize) -> bool {
    STATE.with(|net| {
        let mut net = net.borrow_mut();
        match net.slots.get_mut(&id).filter(|entry| entry.plugin == index) {
            Some(entry) => {
                entry.feed = Some(sender);
                true
            }
            None => false,
        }
    })
}

/// The feed of this slot, while the request still reads the body.
fn feed(id: u64, index: usize) -> Option<mpsc::Sender<Feed>> {
    STATE.with(|net| {
        let net = net.borrow();
        net.slots
            .get(&id)
            .filter(|entry| entry.plugin == index)
            .and_then(|entry| entry.feed.clone())
    })
}

/// One plugin's cookies, and the file the ones with an expiry are saved in:
/// session cookies stay in this store and in memory only.
struct Jar {
    store: CookieStore,
    /// The file of the cookies with an expiry, `None` when the plugin has no
    /// state folder to save them in.
    path: Option<PathBuf>,
}

/// Opens the jar of the plugin at `index`, from `<folder>/cookies.json`, and
/// opens the plugin's own slot in the table. A file that cannot be read is one
/// note and an empty jar.
pub(super) fn open_jar(index: usize, folder: &Path) -> Option<String> {
    // A link where the jar lives is nobody's jar: whoever planted it would
    // choose the file this host reads and later writes.
    if planted(folder) || planted(&folder.join(JAR)) {
        start_empty(index);
        return Some(format!(
            "plugin {index}: its cookies are not read or saved: a symlink stands where they go"
        ));
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "runs on the plugin thread, off the async runtime"
    )]
    let made = std::fs::create_dir_all(folder);
    if let Err(error) = made {
        start_empty(index);
        return Some(format!(
            "plugin {index}: its cookies are not saved: {error}"
        ));
    }
    let path = folder.join(JAR);
    let mut note = None;
    let store = match read_jar(&path) {
        Ok(store) => store,
        Err(error) => {
            note = Some(format!(
                "plugin {index}: its cookies are not read, so it starts with none: {error}"
            ));
            CookieStore::new()
        }
    };
    STATE.with(|net| {
        let mut net = net.borrow_mut();
        net.jars.insert(
            index,
            Jar {
                store,
                path: Some(path),
            },
        );
    });
    note
}

/// Opens the jar of a plugin that starts with no cookies and saves none, when
/// its folder is not one this host may read or write.
fn start_empty(index: usize) {
    STATE.with(|net| {
        net.borrow_mut().jars.insert(
            index,
            Jar {
                store: CookieStore::new(),
                path: None,
            },
        );
    });
}

/// Whether a link stands at `path`: the jar folder and the jar file must be
/// real, since a link would make this host read or write a file it does not own.
#[expect(
    clippy::disallowed_methods,
    reason = "runs on the plugin thread, off the async runtime"
)]
fn planted(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

/// The cookies saved in `path`, and an empty jar when there is no file yet.
#[expect(
    clippy::disallowed_types,
    reason = "runs on the plugin thread, off the async runtime"
)]
fn read_jar(path: &Path) -> Result<CookieStore, String> {
    match std::fs::File::open(path) {
        Ok(file) => cookie_store::serde::json::load(std::io::BufReader::new(file))
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CookieStore::new()),
        Err(error) => Err(error.to_string()),
    }
}

/// The `cookie` header of a request to `url`, as one value.
fn cookies(index: u32, url: &Url) -> Option<String> {
    STATE.with(|net| {
        let net = net.borrow();
        let jar = net.jars.get(&(index as usize))?;
        let mut out = String::new();
        for (name, value) in jar.store.get_request_values(url) {
            if !out.is_empty() {
                out.push_str("; ");
            }
            out.push_str(name);
            out.push('=');
            out.push_str(value);
        }
        (!out.is_empty()).then_some(out)
    })
}

/// Stores the `set-cookie` headers of one response in the plugin's jar, and
/// saves the jar once it changed.
async fn remember(index: u32, url: &Url, headers: &HeaderMap) {
    let changed = STATE.with(|net| {
        let mut net = net.borrow_mut();
        let Some(jar) = net.jars.get_mut(&(index as usize)) else {
            return false;
        };
        let mut changed = false;
        for value in headers.get_all("set-cookie") {
            let Ok(text) = std::str::from_utf8(value.as_bytes()) else {
                continue;
            };
            let Ok(cookie) = RawCookie::parse(text.to_string()) else {
                continue;
            };
            if jar.store.insert_raw(&cookie, url).is_ok() {
                changed = true;
            }
        }
        changed
    });
    if changed {
        save_jar(index).await;
    }
}

/// Writes the cookies with an expiry of one plugin's jar to its file, through a
/// temporary file of the same folder so a reader never sees half a jar.
async fn save_jar(index: u32) {
    let written = STATE.with(|net| {
        let net = net.borrow();
        let jar = net.jars.get(&(index as usize))?;
        let path = jar.path.clone()?;
        let mut out = Vec::new();
        cookie_store::serde::json::save(&jar.store, &mut out).ok()?;
        Some((path, out))
    });
    let Some((path, out)) = written else {
        return;
    };
    let Some(folder) = path.parent() else {
        return;
    };
    // A link where the jar lives is nobody's jar: it is not read, and nothing is
    // written through it.
    if linked(folder).await || linked(&path).await {
        return;
    }
    // The temporary file is made beside the jar under a name nobody can guess,
    // and made new, so a link planted at a name is never opened, let alone
    // written: the jar holds whatever the server said, and nobody else may read
    // it.
    let temp = folder.join(format!("{JAR}.{}.tmp", random()));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600).custom_flags(NOFOLLOW);
    }
    let Ok(mut file) = options.open(&temp).await else {
        return;
    };
    if tokio::io::AsyncWriteExt::write_all(&mut file, &out)
        .await
        .is_err()
    {
        drop(file);
        let _ = tokio::fs::remove_file(&temp).await;
        return;
    }
    let _ = tokio::io::AsyncWriteExt::flush(&mut file).await;
    let _ = file.sync_all().await;
    drop(file);
    if tokio::fs::rename(&temp, &path).await.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
}

/// `O_NOFOLLOW`, which no `std` or `tokio` name stands for: the open fails
/// instead of following a link planted at the name.
#[cfg(all(unix, target_os = "macos"))]
const NOFOLLOW: i32 = 0x0100;

/// The same on Linux, where the flag is another number.
#[cfg(all(unix, not(target_os = "macos")))]
const NOFOLLOW: i32 = 0o400000;

/// Whether a link stands at `path`, which the async side of the jar asks.
async fn linked(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path)
        .await
        .is_ok_and(|meta| meta.file_type().is_symlink())
}

/// A name nothing can guess, for the file the jar is written through.
fn random() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst));
    hasher.finish()
}

/// The integrity check of one response body.
struct Match {
    hasher: Hasher,
    /// The digests this algorithm accepts, in the order the caller listed them.
    expected: Vec<Vec<u8>>,
}

/// The strongest algorithm an integrity string names.
enum Hasher {
    Sha256(Sha256),
    Sha384(Sha384),
    Sha512(Sha512),
}

impl Hasher {
    fn update(&mut self, bytes: &[u8]) {
        match self {
            Hasher::Sha256(hasher) => hasher.update(bytes),
            Hasher::Sha384(hasher) => hasher.update(bytes),
            Hasher::Sha512(hasher) => hasher.update(bytes),
        }
    }

    fn finish(self) -> Vec<u8> {
        match self {
            Hasher::Sha256(hasher) => hasher.finalize().to_vec(),
            Hasher::Sha384(hasher) => hasher.finalize().to_vec(),
            Hasher::Sha512(hasher) => hasher.finalize().to_vec(),
        }
    }
}

/// Sets what `send` asks of a body: the strongest algorithm the integrity
/// string names, and every digest it lists for that algorithm. Options after a
/// `?` are ignored, and a string with no digest a plugin can read checks
/// nothing.
fn digest(id: u64, integrity: Option<&str>, index: usize) {
    let Some(integrity) = integrity else {
        return;
    };
    let mut strongest = 0u8;
    let mut expected: Vec<(u8, Vec<u8>)> = Vec::new();
    for token in integrity.split_ascii_whitespace() {
        let token = token.split('?').next().unwrap_or_default();
        let Some((algorithm, text)) = token.split_once('-') else {
            continue;
        };
        let strength = match algorithm.to_ascii_lowercase().as_str() {
            "sha256" => 1,
            "sha384" => 2,
            "sha512" => 3,
            _ => continue,
        };
        let Some(bytes) = unbase64(text) else {
            continue;
        };
        strongest = strongest.max(strength);
        expected.push((strength, bytes));
    }
    if strongest == 0 {
        return;
    }
    let expected: Vec<Vec<u8>> = expected
        .into_iter()
        .filter(|(strength, _)| *strength == strongest)
        .map(|(_, bytes)| bytes)
        .collect();
    let hasher = match strongest {
        1 => Hasher::Sha256(Sha256::new()),
        2 => Hasher::Sha384(Sha384::new()),
        _ => Hasher::Sha512(Sha512::new()),
    };
    STATE.with(|net| {
        let mut net = net.borrow_mut();
        if let Some(entry) = net.slots.get_mut(&id).filter(|entry| entry.plugin == index) {
            entry.digest = Some(Match { hasher, expected });
        }
    });
}

/// Feeds one chunk of the body to its integrity check.
fn feed_digest(id: u64, bytes: &[u8], index: usize) {
    STATE.with(|net| {
        let mut net = net.borrow_mut();
        if let Some(digest) = net
            .slots
            .get_mut(&id)
            .filter(|entry| entry.plugin == index)
            .and_then(|entry| entry.digest.as_mut())
        {
            digest.hasher.update(bytes);
        }
    });
}

/// Ends the integrity check of a body: the reason it does not match, or `None`.
fn end_digest(id: u64, index: usize) -> Option<String> {
    let held = STATE.with(|net| {
        let mut net = net.borrow_mut();
        net.slots
            .get_mut(&id)
            .filter(|entry| entry.plugin == index)
            .and_then(|entry| entry.digest.take())
    })?;
    let found = held.hasher.finish();
    match held.expected.contains(&found) {
        true => None,
        false => Some(
            "integrity: the body of this response does not match the integrity metadata of the request"
                .to_string(),
        ),
    }
}

/// The bytes of a base64 string, or `None` when it is not one. The padding is
/// optional, as browsers read it.
fn unbase64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 3);
    let mut held = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        held = (held << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((held >> bits) & 0xff).ok()?);
        }
    }
    (!out.is_empty()).then_some(out)
}
