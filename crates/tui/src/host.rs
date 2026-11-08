//! What only the binary can do for the TUI: the model list, the files, the
//! logins and the worker of another session.
//!
//! The binary reads the config and the models cache, walks files with the
//! tools' rules and starts a worker. The TUI only declares [`Host`], so the
//! app calls it through a fake in tests and through `crates/cli` in the binary.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// The most paths `files` keeps.
pub const FILE_CAP: usize = 50_000;

/// A boxed future the host returns; the app polls it on its own runtime.
pub type HostFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// One model the sheet lists.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// The `provider/model` text `set` takes.
    pub id: Arc<str>,
    /// The context window of the model, when it is known.
    pub window: Option<u64>,
    /// The price in USD per million tokens: in, out.
    pub prices: Option<(f64, f64)>,
    /// The reasoning levels the model takes, when they are known.
    pub reasoning: Option<reagent_ai::levels::Reasoning>,
}

/// The models of one configured provider.
#[derive(Debug, Clone, PartialEq)]
pub struct Provider {
    /// The provider's name.
    pub name: Arc<str>,
    /// How the run is authenticated: "logged in", "key from config".
    pub note: Arc<str>,
    /// The models the provider lists, in the order the sheet shows them.
    pub models: Vec<Model>,
}

/// What the model sheet lists: the recent model ids, newest first, then the
/// providers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Models {
    /// The model ids used recently, newest first.
    pub recent: Vec<Arc<str>>,
    /// The configured providers and their models.
    pub providers: Vec<Provider>,
}

/// The session folder's files, one `Arc<str>` per relative path, and whether
/// the list was cut at its cap.
#[derive(Debug, Clone, PartialEq)]
pub struct Files {
    /// The relative paths, at most [`FILE_CAP`] of them.
    pub paths: Arc<[Arc<str>]>,
    /// Whether the list stopped at [`FILE_CAP`].
    pub cut: bool,
}

impl Default for Files {
    fn default() -> Self {
        Self {
            paths: Arc::from(Vec::new()),
            cut: false,
        }
    }
}

/// One step of a login. No step ever holds a key or a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStep {
    /// The user opens this page in a browser.
    Open {
        /// The page to open.
        url: Arc<str>,
    },
    /// The user enters the code shown on this page.
    Code {
        /// The page that shows the code.
        page: Arc<str>,
        /// The code the user enters.
        code: Arc<str>,
    },
    /// The login finished.
    Done,
    /// The login failed, with the reason.
    Failed(
        /// Why the login failed.
        Arc<str>,
    ),
}

/// Where the worker of another session listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// The worker's socket.
    pub socket: PathBuf,
    /// The token that joins the worker.
    pub token: String,
    /// The session folder, when the worker knows it.
    pub session_dir: Option<PathBuf>,
}

/// The worker an open starts or finds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The worker of this session, found when it is running and started when it
    /// is not.
    Session(String),
    /// A worker on a new session in the same folder.
    New,
    /// A worker on a session forked from this one at its newest turn.
    Fork(String),
}

/// What the binary does for the app: the models, the files, the logins and the
/// worker of another session.
pub trait Host: Send + Sync {
    /// The models the sheet lists.
    fn models(&self) -> HostFuture<Models>;
    /// The session folder's files.
    fn files(&self) -> HostFuture<Files>;
    /// The providers a login can open, in the order the sheet lists them.
    fn logins(&self) -> &[&'static str];
    /// Runs a login and sends its steps; the channel closes after `Done` or
    /// `Failed`.
    fn login(&self, provider: &str) -> tokio::sync::mpsc::Receiver<LoginStep>;
    /// Starts or finds the worker `target` names.
    fn open(&self, target: Target) -> HostFuture<Result<Opened, String>>;
    /// Removes the session `session` names from the disk.
    fn delete(&self, session: &str) -> HostFuture<Result<(), String>>;
    /// Puts text in the local system clipboard; returns at once.
    fn copy(&self, text: &str);
}

/// A [`Host`] that answers at once from what it was given, for tests.
#[derive(Debug)]
pub struct FakeHost {
    models: Models,
    files: Files,
    providers: Vec<&'static str>,
    steps: Vec<Vec<LoginStep>>,
    open: Result<Opened, String>,
    delete: Result<(), String>,
    logged: Mutex<Vec<String>>,
    opened: Mutex<Vec<Target>>,
    deleted: Mutex<Vec<String>>,
    copied: Mutex<Vec<String>>,
}

impl FakeHost {
    /// A fake that has nothing: no model, no file, no login and no session.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the models the sheet lists.
    pub fn with_models(mut self, models: Models) -> Self {
        self.models = models;
        self
    }

    /// Sets the relative paths the file sheet lists.
    pub fn with_files(mut self, paths: Vec<&str>) -> Self {
        self.files.paths = paths.into_iter().map(Arc::from).collect();
        self
    }

    /// Sets the steps the login of `provider` sends.
    pub fn with_login(mut self, provider: &'static str, steps: Vec<LoginStep>) -> Self {
        self.providers.push(provider);
        self.steps.push(steps);
        self
    }

    /// Sets what opening a session answers.
    pub fn with_open(mut self, open: Result<Opened, String>) -> Self {
        self.open = open;
        self
    }

    /// Sets what deleting a session answers.
    pub fn with_delete(mut self, delete: Result<(), String>) -> Self {
        self.delete = delete;
        self
    }

    /// The sessions `delete` was called with, in order.
    pub fn deleted(&self) -> Vec<String> {
        self.deleted.lock().expect("no poisoned lock").clone()
    }

    /// The providers `login` was called with, in order.
    pub fn logged(&self) -> Vec<String> {
        self.logged.lock().expect("no poisoned lock").clone()
    }

    /// The targets `open` was called with, in order.
    pub fn opened(&self) -> Vec<Target> {
        self.opened.lock().expect("no poisoned lock").clone()
    }

    /// The texts `copy` was called with, in order.
    pub fn copied(&self) -> Vec<String> {
        self.copied.lock().expect("no poisoned lock").clone()
    }
}

impl Default for FakeHost {
    fn default() -> Self {
        Self {
            models: Models::default(),
            files: Files::default(),
            providers: Vec::new(),
            steps: Vec::new(),
            open: Err("no session".to_owned()),
            delete: Ok(()),
            logged: Mutex::new(Vec::new()),
            opened: Mutex::new(Vec::new()),
            deleted: Mutex::new(Vec::new()),
            copied: Mutex::new(Vec::new()),
        }
    }
}

impl Host for FakeHost {
    fn models(&self) -> HostFuture<Models> {
        let models = self.models.clone();
        Box::pin(async move { models })
    }

    fn files(&self) -> HostFuture<Files> {
        let files = self.files.clone();
        Box::pin(async move { files })
    }

    fn logins(&self) -> &[&'static str] {
        &self.providers
    }

    fn login(&self, provider: &str) -> tokio::sync::mpsc::Receiver<LoginStep> {
        self.logged
            .lock()
            .expect("no poisoned lock")
            .push(provider.to_owned());
        let at = self.providers.iter().position(|name| *name == provider);
        let steps = at.map(|at| self.steps[at].clone()).unwrap_or_default();
        let (sender, receiver) = tokio::sync::mpsc::channel(steps.len().max(1));
        for step in steps {
            let _ = sender.try_send(step);
        }
        receiver
    }

    fn open(&self, target: Target) -> HostFuture<Result<Opened, String>> {
        self.opened.lock().expect("no poisoned lock").push(target);
        let open = self.open.clone();
        Box::pin(async move { open })
    }

    fn delete(&self, session: &str) -> HostFuture<Result<(), String>> {
        self.deleted
            .lock()
            .expect("no poisoned lock")
            .push(session.to_owned());
        let delete = self.delete.clone();
        Box::pin(async move { delete })
    }

    fn copy(&self, text: &str) {
        self.copied
            .lock()
            .expect("no poisoned lock")
            .push(text.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::Receiver;

    fn fake_models() -> Models {
        Models {
            recent: vec![Arc::from("anthropic/opus")],
            providers: vec![Provider {
                name: Arc::from("anthropic"),
                note: Arc::from("logged in"),
                models: vec![Model {
                    id: Arc::from("anthropic/opus"),
                    window: Some(200_000),
                    prices: Some((15.0, 75.0)),
                    reasoning: None,
                }],
            }],
        }
    }

    fn fake_opened() -> Opened {
        Opened {
            socket: PathBuf::from("/tmp/session.sock"),
            token: "join-token".to_owned(),
            session_dir: Some(PathBuf::from("/tmp/session")),
        }
    }

    async fn drain(mut receiver: Receiver<LoginStep>) -> Vec<LoginStep> {
        let mut steps = Vec::new();
        while let Some(step) = receiver.recv().await {
            steps.push(step);
        }
        steps
    }

    #[tokio::test]
    async fn fake_answers_models_files_and_open() {
        let host = FakeHost::new()
            .with_models(fake_models())
            .with_files(vec!["src/main.rs", "README.md"])
            .with_open(Ok(fake_opened()));

        assert_eq!(host.models().await, fake_models());
        let files = host.files().await;
        assert_eq!(
            &*files.paths,
            &[Arc::from("src/main.rs"), Arc::from("README.md")]
        );
        assert!(!files.cut);
        let session = Target::Session("s1".to_owned());
        assert_eq!(host.open(session).await, Ok(fake_opened()));
    }

    #[tokio::test]
    async fn login_channel_yields_the_steps_then_closes() {
        let steps = vec![
            LoginStep::Open {
                url: Arc::from("https://example.test/auth"),
            },
            LoginStep::Code {
                page: Arc::from("https://example.test/device"),
                code: Arc::from("ABCD-1234"),
            },
            LoginStep::Done,
        ];
        let host = FakeHost::new().with_login("anthropic", steps.clone());

        assert_eq!(drain(host.login("anthropic")).await, steps);
    }

    #[tokio::test]
    async fn fake_records_login_and_open_calls() {
        let host = FakeHost::new()
            .with_login(
                "anthropic",
                vec![LoginStep::Failed(Arc::from("no network"))],
            )
            .with_login("openai", vec![LoginStep::Done])
            .with_open(Err("no such session".to_owned()));

        assert_eq!(drain(host.login("openai")).await, [LoginStep::Done]);
        let _ = host.login("anthropic");
        let _ = host.open(Target::Session("first".to_owned())).await;
        let _ = host.open(Target::New).await;
        let _ = host.open(Target::Fork("second".to_owned())).await;

        assert_eq!(host.logged(), ["openai", "anthropic"]);
        assert_eq!(
            host.opened(),
            [
                Target::Session("first".to_owned()),
                Target::New,
                Target::Fork("second".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn logins_lists_the_providers_in_the_order_given() {
        let host = FakeHost::new()
            .with_login("openai", vec![LoginStep::Done])
            .with_login("anthropic", vec![LoginStep::Done]);

        assert_eq!(host.logins(), ["openai", "anthropic"]);
    }

    #[tokio::test]
    async fn fake_records_deletes_and_answers_what_it_was_given() {
        let host = FakeHost::new();
        assert_eq!(host.delete("s1").await, Ok(()));
        assert_eq!(host.deleted(), ["s1"]);

        let host = FakeHost::new().with_delete(Err("a fork reads it".to_owned()));
        assert_eq!(host.delete("s2").await, Err("a fork reads it".to_owned()));
        assert_eq!(host.deleted(), ["s2"]);
    }
}
