//! A saved ChatGPT login as a live credential: the access header kept in
//! memory, and the refresh that replaces it behind a lock two runs share.

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::{self, Either};
use futures_util::lock::Mutex;
use reagent_config::Secret;
use reagent_config::auth::{self, Login, Logins};
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};

use super::chatgpt::{self, RefreshError};
use super::device;

pub use reagent_oauth::{Sleeper, Wait};

/// How long before the saved expiry a token is replaced, so a request that
/// starts now does not run past it.
const MARGIN: u64 = 60;

/// The name of the file two runs take turns on.
const LOCK_FILE: &str = "auth.json.lock";

/// How long one try at the lock waits before the next try.
const LOCK_RETRY: Duration = Duration::from_millis(50);

/// How long a run waits for another run to finish its refresh.
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the refresh request and its body may take.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// The seconds since the epoch, or 0 before 1970.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// `authorization: Bearer <access>`, marked sensitive.
fn bearer(access: &str) -> Result<HeaderValue, String> {
    let mut text = String::with_capacity("Bearer ".len() + access.len());
    text.push_str("Bearer ");
    text.push_str(access);
    let mut value = HeaderValue::from_str(&text)
        .map_err(|_| "the saved access token is not a valid header value".to_string())?;
    value.set_sensitive(true);
    Ok(value)
}

/// The access header and the time it stops being usable.
#[derive(Debug, Clone)]
struct Cached {
    header: HeaderValue,
    expires_at: u64,
}

/// One provider's saved ChatGPT login, and the access token it holds now.
///
/// `Debug` prints the provider name only: the cache holds a bearer of the
/// account.
pub struct Session {
    dir: PathBuf,
    name: String,
    token_url: String,
    cached: Mutex<Cached>,
    sleep: Sleeper,
    /// A save of the rotated tokens that failed, for the caller to print once.
    warning: StdMutex<Option<String>>,
}

impl fmt::Debug for Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Session")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// The saved login of `name` in `dir`, with its access token ready.
    ///
    /// `token_url` is a parameter so a test points it at its own server; the
    /// command passes the real ChatGPT endpoint. `sleep` is the timer of the
    /// caller's runtime, so no wait here holds the runtime thread.
    pub fn load(dir: &Path, name: &str, token_url: &str, sleep: Sleeper) -> Result<Self, String> {
        let logins = auth::read(dir)?;
        let (Some(Login::Oauth {
            access, expires_at, ..
        })
        | Some(Login::Device {
            access, expires_at, ..
        })) = logins.get(name)
        else {
            return Err(format!(
                "no ChatGPT login for {name}; run {}",
                login_command(name)
            ));
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            name: name.to_string(),
            token_url: token_url.to_string(),
            cached: Mutex::new(Cached {
                header: bearer(access.expose())?,
                expires_at: *expires_at,
            }),
            sleep,
            warning: StdMutex::new(None),
        })
    }

    /// The warning of a refresh that could not be saved, taken so it is
    /// printed once. It names the file and the reason, never a token.
    pub fn take_warning(&self) -> Option<String> {
        self.warning.lock().ok().and_then(|mut slot| slot.take())
    }

    /// The authorization header, refreshed first when the token is near its
    /// end.
    pub async fn header(&self) -> Result<(HeaderName, HeaderValue), String> {
        let mut cached = self.cached.lock().await;
        if now().saturating_add(MARGIN) < cached.expires_at {
            return Ok((AUTHORIZATION, cached.header.clone()));
        }
        *cached = self.refresh().await?;
        Ok((AUTHORIZATION, cached.header.clone()))
    }

    /// `work`, or the timeout error when the sleeper finishes first, so a
    /// stalled token server cannot hold the run.
    async fn within<T>(&self, work: impl Future<Output = T>) -> Result<T, String> {
        let work = Box::pin(work);
        let wait = (self.sleep)(REFRESH_TIMEOUT);
        match future::select(work, wait).await {
            Either::Left((value, _)) => Ok(value),
            Either::Right(_) => Err(format!("{} token refresh timed out", self.name)),
        }
    }

    /// The access token of the auth file, or a fresh one it trades and saves.
    ///
    /// The auth file and its lock are read and written with the blocking `std`
    /// calls on purpose: this runs once per token lifetime, and a lock cannot
    /// be held across the async exchange otherwise. Both the wait for the lock
    /// and the request are bounded, so a stalled token server costs one
    /// refresh, not the process.
    async fn refresh(&self) -> Result<Cached, String> {
        let _lock = Lock::wait(&self.dir.join(LOCK_FILE), &self.sleep).await?;
        let mut logins = auth::read(&self.dir)?;
        if let Some(Login::Device {
            provider,
            client_id,
            token_url,
            access,
            refresh,
            expires_at,
        }) = logins.get(&self.name).cloned()
        {
            return self
                .refresh_device(
                    &mut logins,
                    Device {
                        provider,
                        client_id,
                        token_url,
                        access,
                        refresh,
                        expires_at,
                    },
                )
                .await;
        }
        let Some(Login::Oauth {
            client_id,
            host_id,
            access,
            refresh,
            id_token,
            expires_at,
            subject,
            email,
        }) = logins.get(&self.name).cloned()
        else {
            return Err(expired(&self.name));
        };
        // Another run may have refreshed while this one waited for the lock.
        if now().saturating_add(MARGIN) < expires_at {
            return Ok(Cached {
                header: bearer(access.expose())?,
                expires_at,
            });
        }
        let refreshing = chatgpt::refresh(&self.token_url, &client_id, refresh.expose());
        let reply = match self.within(refreshing).await? {
            Ok(reply) => reply,
            // The login is gone, so the message names the fix.
            Err(RefreshError::Expired) => return Err(expired(&self.name)),
            Err(RefreshError::Other(message)) => return Err(message),
        };
        let expires_at = now().saturating_add(reply.expires_in);
        // The refresh token rotates, so the new one is saved at once.
        let id_token = reply.id_token.clone().unwrap_or(id_token);
        logins.set(
            &self.name,
            Login::Oauth {
                client_id,
                host_id,
                access: reply.access_token.clone(),
                refresh: reply.refresh_token.clone(),
                id_token,
                expires_at,
                subject,
                email,
            },
        );
        // The rotated tokens are in hand. A save that fails costs the next run
        // a refresh, but the run that traded them keeps its new access token,
        // so the problem is a warning for the caller to print, not an error.
        if let Err(error) = auth::write(&self.dir, &logins)
            && let Ok(mut slot) = self.warning.lock()
        {
            *slot = Some(error);
        }
        Ok(Cached {
            header: bearer(reply.access_token.expose())?,
            expires_at,
        })
    }

    /// The refresh of a device login, under the lock `refresh` holds.
    async fn refresh_device(&self, logins: &mut Logins, login: Device) -> Result<Cached, String> {
        // Another run may have refreshed while this one waited for the lock.
        if now().saturating_add(MARGIN) < login.expires_at {
            return Ok(Cached {
                header: bearer(login.access.expose())?,
                expires_at: login.expires_at,
            });
        }
        let refreshing =
            device::refresh(&login.token_url, &login.client_id, login.refresh.expose());
        let tokens = match self.within(refreshing).await? {
            Ok(tokens) => tokens,
            Err(device::RefreshError::Expired) => {
                return Err(format!(
                    "{} login expired; run reagent login {}",
                    login.provider,
                    login_target(&login.provider, &self.name)
                ));
            }
            Err(device::RefreshError::Other(message)) => return Err(message),
        };
        let expires_at = now().saturating_add(tokens.expires_in);
        logins.set(
            &self.name,
            Login::Device {
                provider: login.provider,
                client_id: login.client_id,
                token_url: login.token_url,
                access: tokens.access.clone(),
                // A reply without a refresh token keeps the old one.
                refresh: tokens.refresh.unwrap_or(login.refresh),
                expires_at,
            },
        );
        if let Err(error) = auth::write(&self.dir, logins)
            && let Ok(mut slot) = self.warning.lock()
        {
            *slot = Some(error);
        }
        Ok(Cached {
            header: bearer(tokens.access.expose())?,
            expires_at,
        })
    }
}

/// The fields of a saved device login.
struct Device {
    provider: String,
    client_id: String,
    token_url: String,
    access: Secret,
    refresh: Secret,
    expires_at: u64,
}

/// The `reagent login` arguments that save the login `name` of `provider`.
fn login_target(provider: &str, name: &str) -> String {
    if name == provider {
        provider.to_string()
    } else {
        format!("{provider} --as {name}")
    }
}

/// The command that saves the ChatGPT login `name`.
fn login_command(name: &str) -> String {
    if name == "chatgpt" {
        "reagent login chatgpt".to_string()
    } else {
        format!("reagent login chatgpt --as {name}")
    }
}

/// The message a login that can no longer be refreshed shows.
fn expired(name: &str) -> String {
    format!("chatgpt login expired; run {}", login_command(name))
}

/// An exclusive lock on a file, released when it drops.
#[expect(
    clippy::disallowed_types,
    reason = "a lock file is taken once per token lifetime and is not a data file"
)]
struct Lock(std::fs::File);

impl Lock {
    /// Takes the exclusive lock on `path`, creating the file when it is new.
    ///
    /// The try is the non-blocking one: a run that finds the lock taken waits
    /// on the caller's sleeper, so the only runtime thread keeps serving its
    /// other work, and gives up after `LOCK_TIMEOUT` rather than hanging on a
    /// run that stalled at the token server.
    async fn wait(path: &Path, sleep: &Sleeper) -> Result<Self, String> {
        use std::fs::TryLockError;

        let fail = |error: std::io::Error| format!("{}: {error}", path.display());
        #[expect(
            clippy::disallowed_types,
            reason = "the lock file of a token refresh is not a data file"
        )]
        let mut options = std::fs::OpenOptions::new();
        let file = options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(fail)?;
        let mut waited = Duration::ZERO;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self(file)),
                Err(TryLockError::WouldBlock) if waited < LOCK_TIMEOUT => {
                    sleep(LOCK_RETRY).await;
                    waited += LOCK_RETRY;
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(
                        "chatgpt token refresh is locked by another reagent run".to_string()
                    );
                }
                Err(TryLockError::Error(error)) => return Err(fail(error)),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use reagent_config::Secret;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// A folder of its own for one test, removed when it drops.
    struct Folder(tempfile::TempDir);

    impl Folder {
        /// A new temp folder named after `name`, empty.
        fn new(name: &str) -> Self {
            let dir = tempfile::Builder::new()
                .prefix(&format!("reagent-oauth-{name}-"))
                .tempdir()
                .expect("makes a temp folder");
            Self(dir)
        }

        fn path(&self) -> &Path {
            self.0.path()
        }
    }

    /// The sleeper of a test that needs a real wait: the test runtime's timer.
    fn timer() -> Sleeper {
        Arc::new(|duration: Duration| -> Wait { Box::pin(tokio::time::sleep(duration)) })
    }

    /// A sleeper that finishes at once, so a wait costs one poll: a held lock
    /// runs its tries out at once, and a request still pending times out at
    /// that poll.
    fn instant() -> Sleeper {
        Arc::new(|_: Duration| -> Wait { Box::pin(std::future::ready(())) })
    }

    /// The token server: answers each POST with `reply` and counts the calls.
    async fn token_server(status: u16, reply: &'static str, calls: Arc<AtomicUsize>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
        let address = listener.local_addr().expect("the bound address");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                calls.fetch_add(1, Ordering::SeqCst);
                let mut chunk = [0u8; 2048];
                let _ = socket.read(&mut chunk).await;
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{address}/oauth/token")
    }

    /// A saved login in `dir` with `expires_at` and `refresh`.
    fn save(dir: &Path, expires_at: u64, refresh: &str) {
        let mut logins = auth::Logins::default();
        logins.set(
            "chatgpt",
            Login::Oauth {
                client_id: "oaiapp_1".into(),
                host_id: "urn:uuid:00000000-0000-4000-8000-000000000000".into(),
                access: Secret::from("old-access"),
                refresh: Secret::from(refresh),
                id_token: Secret::from("old-id"),
                expires_at,
                subject: "user-1".into(),
                email: Some("a@b.c".into()),
            },
        );
        auth::write(dir, &logins).expect("writes the login");
    }

    /// The saved token fields of `dir`.
    fn saved(dir: &Path, name: &str) -> (String, String, u64) {
        let logins = auth::read(dir).expect("reads the login");
        let Some(Login::Oauth {
            access,
            refresh,
            expires_at,
            ..
        }) = logins.get(name)
        else {
            panic!("no oauth login");
        };
        (
            access.expose().to_string(),
            refresh.expose().to_string(),
            *expires_at,
        )
    }

    /// A saved device login in `dir` that refreshes at `token_url`.
    fn save_device(dir: &Path, name: &str, token_url: &str, expires_at: u64) {
        let mut logins = auth::Logins::default();
        logins.set(
            name,
            Login::Device {
                provider: "radius".into(),
                client_id: "pi-gateway".into(),
                token_url: token_url.into(),
                access: Secret::from("old-access"),
                refresh: Secret::from("old-refresh"),
                expires_at,
            },
        );
        auth::write(dir, &logins).expect("writes the login");
    }

    #[tokio::test]
    async fn device_session_refreshes_at_its_saved_endpoint() {
        let dir = Folder::new("device-rotate");
        let calls = Arc::new(AtomicUsize::new(0));
        let url = token_server(
            200,
            r#"{"access_token":"new-access","expires_in":3600}"#,
            Arc::clone(&calls),
        )
        .await;
        save_device(dir.path(), "radius", &url, 0);
        // The URL given to `load` is ChatGPT's; a device login keeps its own.
        let session =
            Session::load(dir.path(), "radius", "http://127.0.0.1:9/none", timer()).expect("loads");
        let (_, value) = session.header().await.expect("the header");
        assert_eq!(value.as_bytes(), b"Bearer new-access");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let logins = auth::read(dir.path()).expect("reads");
        let Some(Login::Device {
            access,
            refresh,
            expires_at,
            ..
        }) = logins.get("radius")
        else {
            panic!("no device login");
        };
        assert_eq!(access.expose(), "new-access");
        // No refresh token came back, so the old one stays.
        assert_eq!(refresh.expose(), "old-refresh");
        assert!(*expires_at > now() + 3000);
        let (_, value) = session.header().await.expect("the header");
        assert_eq!(value.as_bytes(), b"Bearer new-access");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn device_session_an_expired_login_names_its_command() {
        let dir = Folder::new("device-expired");
        let calls = Arc::new(AtomicUsize::new(0));
        let url = token_server(400, r#"{"error":"invalid_grant"}"#, Arc::clone(&calls)).await;
        save_device(dir.path(), "radius-work", &url, 0);
        let session = Session::load(dir.path(), "radius-work", &url, timer()).expect("loads");
        let error = session.header().await.unwrap_err();
        assert_eq!(
            error,
            "radius login expired; run reagent login radius --as radius-work"
        );
    }

    #[tokio::test]
    async fn oauth_session_a_fresh_token_makes_no_request() {
        let dir = Folder::new("fresh");
        save(dir.path(), now() + 3600, "r1");
        let calls = Arc::new(AtomicUsize::new(0));
        let url = token_server(200, "{}", Arc::clone(&calls)).await;
        let session = Session::load(dir.path(), "chatgpt", &url, timer()).expect("loads");
        let (name, value) = session.header().await.expect("the header");
        assert_eq!(name, "authorization");
        assert_eq!(value.as_bytes(), b"Bearer old-access");
        assert!(value.is_sensitive());
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no refresh was needed");
    }

    #[tokio::test]
    async fn oauth_session_an_expired_token_refreshes_once_and_saves_the_rotation() {
        let dir = Folder::new("rotate");
        save(dir.path(), 0, "r1");
        let calls = Arc::new(AtomicUsize::new(0));
        let url = token_server(
            200,
            r#"{"access_token":"new-access","refresh_token":"r2","expires_in":3600,"id_token":"new-id"}"#,
            Arc::clone(&calls),
        )
        .await;
        let session = Session::load(dir.path(), "chatgpt", &url, timer()).expect("loads");
        let (_, value) = session.header().await.expect("the header");
        assert_eq!(value.as_bytes(), b"Bearer new-access");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let (access, refresh, expires_at) = saved(dir.path(), "chatgpt");
        assert_eq!(access, "new-access");
        assert_eq!(refresh, "r2");
        assert!(expires_at > now() + 3000);
        // A second call rides on the saved token, which is now fresh.
        let (_, value) = session.header().await.expect("the header");
        assert_eq!(value.as_bytes(), b"Bearer new-access");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn oauth_session_two_sessions_refresh_once_through_the_file_lock() {
        let dir = Folder::new("lock");
        save(dir.path(), 0, "r1");
        let calls = Arc::new(AtomicUsize::new(0));
        let url = token_server(
            200,
            r#"{"access_token":"new-access","refresh_token":"r2","expires_in":3600}"#,
            Arc::clone(&calls),
        )
        .await;
        let first = Session::load(dir.path(), "chatgpt", &url, timer()).expect("loads");
        let second = Session::load(dir.path(), "chatgpt", &url, timer()).expect("loads");
        let (_, value) = first.header().await.expect("the first header");
        assert_eq!(value.as_bytes(), b"Bearer new-access");
        // The second session saw an expired token too, but the saved expiry is
        // fresh now, so it reads the new token instead of refreshing again.
        let (_, value) = second.header().await.expect("the second header");
        assert_eq!(value.as_bytes(), b"Bearer new-access");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one refresh, not two");
        assert_eq!(saved(dir.path(), "chatgpt").1, "r2");
    }

    #[tokio::test]
    async fn oauth_session_an_invalid_grant_names_the_login_command() {
        let dir = Folder::new("grant");
        save(dir.path(), 0, "r1");
        let calls = Arc::new(AtomicUsize::new(0));
        let url = token_server(
            400,
            r#"{"error":"invalid_grant","error_description":"r1 is gone"}"#,
            Arc::clone(&calls),
        )
        .await;
        let session = Session::load(dir.path(), "chatgpt", &url, timer()).expect("loads");
        let error = session.header().await.unwrap_err();
        assert_eq!(error, "chatgpt login expired; run reagent login chatgpt");
        assert!(!error.contains("r1"), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn oauth_session_a_refused_refresh_keeps_the_login_usable() {
        let dir = Folder::new("refused");
        save(dir.path(), 0, "r1");
        let calls = Arc::new(AtomicUsize::new(0));
        let url = token_server(500, r#"{"error":"server_error"}"#, Arc::clone(&calls)).await;
        let session = Session::load(dir.path(), "chatgpt", &url, timer()).expect("loads");
        let error = session.header().await.unwrap_err();
        assert!(error.contains("HTTP 500"), "{error}");
        assert!(error.contains("server_error"), "{error}");
        // Nothing was saved, so the refresh token is still the old one.
        assert_eq!(saved(dir.path(), "chatgpt").1, "r1");
    }

    #[tokio::test]
    async fn oauth_session_without_a_login_names_the_command() {
        let dir = Folder::new("missing");
        let error = Session::load(dir.path(), "chatgpt", "http://x", timer()).unwrap_err();
        assert!(error.contains("reagent login chatgpt"), "{error}");
    }

    #[tokio::test]
    async fn oauth_session_a_silent_token_server_times_out() {
        let dir = Folder::new("silent");
        save(dir.path(), 0, "r1");
        // An endpoint that takes the request and never answers.
        let silent = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
        let address = silent.local_addr().expect("the bound address");
        tokio::spawn(async move {
            let (_socket, _) = silent.accept().await.expect("accept");
            std::future::pending::<()>().await;
        });
        let url = format!("http://{address}/oauth/token");
        // The test sleeper finishes at once, so the timeout error comes from
        // the timer without a wait and says nothing of the token.
        let session = Session::load(dir.path(), "chatgpt", &url, instant()).expect("loads");
        let error = session.header().await.unwrap_err();
        assert_eq!(error, "chatgpt token refresh timed out");
        assert!(!error.contains("r1"), "{error}");
        assert_eq!(saved(dir.path(), "chatgpt").1, "r1", "nothing was saved");
    }

    #[tokio::test]
    async fn oauth_session_a_held_lock_gives_up() {
        let dir = Folder::new("held");
        save(dir.path(), 0, "r1");
        let calls = Arc::new(AtomicUsize::new(0));
        let url = token_server(
            200,
            r#"{"access_token":"new-access","refresh_token":"r2","expires_in":3600}"#,
            Arc::clone(&calls),
        )
        .await;
        // Another run holds the lock on the same file.
        #[expect(
            clippy::disallowed_types,
            reason = "a test holds the lock the session waits on"
        )]
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.path().join(LOCK_FILE))
            .expect("opens the lock file");
        held.try_lock().expect("takes the lock");
        let session = Session::load(dir.path(), "chatgpt", &url, instant()).expect("loads");
        let error = session.header().await.unwrap_err();
        assert_eq!(
            error,
            "chatgpt token refresh is locked by another reagent run"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no refresh while locked");
        drop(held);
    }

    #[tokio::test]
    async fn oauth_session_debug_hides_the_token() {
        let dir = Folder::new("debug");
        save(dir.path(), now() + 3600, "r1");
        let session = Session::load(dir.path(), "chatgpt", "http://x", timer()).expect("loads");
        let text = format!("{session:?}");
        assert!(!text.contains("old-access"), "{text}");
        assert!(text.contains("chatgpt"), "{text}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oauth_session_a_refused_save_keeps_the_new_token() {
        use std::os::unix::fs::PermissionsExt;

        let dir = Folder::new("refused-save");
        save(dir.path(), 0, "r1");
        // The lock file exists already, so a read-only folder fails the save of
        // the rotated tokens and nothing before it.
        #[expect(clippy::disallowed_methods, reason = "a test makes its own file")]
        std::fs::write(dir.path().join(LOCK_FILE), "").expect("makes the lock file");
        let calls = Arc::new(AtomicUsize::new(0));
        let url = token_server(
            200,
            r#"{"access_token":"new-access","refresh_token":"r2","expires_in":3600}"#,
            Arc::clone(&calls),
        )
        .await;
        let session = Session::load(dir.path(), "chatgpt", &url, timer()).expect("loads");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500))
            .expect("chmods");
        // Root writes through a read-only folder; the refusal is checked, and
        // only then is the warning asserted.
        #[expect(clippy::disallowed_methods, reason = "a test probes its own folder")]
        let refused = std::fs::write(dir.path().join("probe"), "").is_err();
        let (_, value) = session.header().await.expect("the header");
        assert_eq!(value.as_bytes(), b"Bearer new-access");
        if refused {
            let warning = session.take_warning().expect("a warning about the save");
            assert!(warning.contains("auth.json"), "{warning}");
            assert!(!warning.contains("new-access"), "{warning}");
            assert!(!warning.contains("r2"), "{warning}");
            assert!(session.take_warning().is_none(), "the warning prints once");
        }
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmods back");
        let (_, value) = session.header().await.expect("the cached header");
        assert_eq!(value.as_bytes(), b"Bearer new-access");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one refresh only");
    }
}
