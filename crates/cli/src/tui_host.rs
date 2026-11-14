//! The host the TUI asks for what only the binary can do.
//!
//! It reads the config, the saved logins and the models cache, walks the
//! folder with the tools' own rules, runs the login of the command and starts
//! a worker for another session. No key and no token ever leaves here.
#![cfg(feature = "tui")]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use reagent_config::auth::Logins;
use reagent_config::{Provider as Table, Settings};
use reagent_protocol::discovery;
use reagent_store::list::SessionList;
use reagent_store::paths;
use reagent_tui::host::{
    Files, Host, HostFuture, LoginStep, Model, Models, Opened, Provider, Target,
};
use tokio::sync::mpsc;

use crate::login::Announce;
use crate::tui;

/// A million: the unit the sheet shows a price in.
const MILLION: f64 = 1_000_000.0;

/// The most model ids the sheet lists as used recently.
const RECENT: usize = 5;

/// The capacity of a login's channel: a page, a code, then the end.
const STEPS: usize = 8;

/// The host of one run: its folder, its profile, its state folder and the
/// words a worker of it starts with.
pub(crate) struct CliHost {
    /// The folder the run works in.
    cwd: PathBuf,
    /// The profile the run read, if any.
    profile: Option<String>,
    /// The state folder of the sessions and the workers.
    state: PathBuf,
    /// The words this run was given, for the worker of another session.
    worker_args: Vec<OsString>,
}

impl CliHost {
    /// The host of `cwd` under `profile`, with `state` for its sessions and the
    /// words of this run for a worker it starts.
    pub(crate) fn new(
        cwd: PathBuf,
        profile: Option<String>,
        state: PathBuf,
        worker_args: Vec<OsString>,
    ) -> CliHost {
        CliHost {
            cwd,
            profile,
            state,
            worker_args,
        }
    }
}

impl Host for CliHost {
    fn models(&self) -> HostFuture<Models> {
        let cwd = self.cwd.clone();
        let profile = self.profile.clone();
        let state = self.state.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || models_of(&cwd, profile.as_deref(), &state))
                .await
                .unwrap_or_default()
        })
    }

    fn files(&self) -> HostFuture<Files> {
        let cwd = self.cwd.clone();
        Box::pin(async move {
            let (paths, cut) = reagent_tools::list_files(&cwd, reagent_tui::host::FILE_CAP).await;
            Files {
                paths: paths.into_iter().map(Arc::from).collect(),
                cut,
            }
        })
    }

    fn logins(&self) -> &[&'static str] {
        crate::login::LOGIN_PROVIDERS
    }

    fn login(&self, provider: &str) -> mpsc::Receiver<LoginStep> {
        let (sender, receiver) = mpsc::channel(STEPS);
        let provider = provider.to_owned();
        tokio::spawn(async move {
            let open = |url: &str| {
                let _ = sender.try_send(LoginStep::Open {
                    url: Arc::from(url),
                });
            };
            let code = |page: &str, code: &str| {
                let _ = sender.try_send(LoginStep::Code {
                    page: Arc::from(page),
                    code: Arc::from(code),
                });
            };
            let line = |_: &str| {};
            let announce = Announce {
                open: &open,
                code: &code,
                line: &line,
            };
            let step = match crate::login::tui(&provider, &announce).await {
                Ok(()) => LoginStep::Done,
                Err(error) => LoginStep::Failed(Arc::from(error.as_str())),
            };
            let _ = sender.try_send(step);
        });
        receiver
    }

    fn open(&self, target: Target) -> HostFuture<Result<Opened, String>> {
        let state = self.state.clone();
        let cwd = self.cwd.clone();
        let args = self.worker_args.clone();
        Box::pin(async move {
            let flags = match &target {
                Target::Session(session) => {
                    let folder = discovery::folder(&state);
                    if let Some(entry) = discovery::list(&folder)
                        .await
                        .into_iter()
                        .find(|entry| &entry.session == session)
                    {
                        return Ok(opened(&state, &cwd, &entry));
                    }
                    tui::resume_flags(args, session)
                }
                Target::New => tui::new_flags(args),
                Target::Fork(session) => tui::fork_flags(args, session),
            };
            let entry = tui::start_worker(&state, flags).await?;
            Ok(opened(&state, &cwd, &entry))
        })
    }

    fn delete(&self, session: &str) -> HostFuture<Result<(), String>> {
        let state = self.state.clone();
        let cwd = self.cwd.clone();
        let session = session.to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let (folder, dir) = crate::session::locate_in(&state, &cwd, &session)?;
                crate::session::remove(&folder, &dir)
            })
            .await
            .map_err(|error| error.to_string())?
        })
    }

    fn copy(&self, text: &str) {
        reagent_tui::clipboard::copy(text);
    }
}

/// The answer a worker's discovery entry gives, with the folder the store keeps
/// its session in.
fn opened(state: &Path, cwd: &Path, entry: &discovery::Entry) -> Opened {
    Opened {
        socket: entry.socket.clone(),
        token: entry.token.clone(),
        session_dir: Some(
            paths::sessions_root(state)
                .join(paths::cwd_key(cwd))
                .join(&entry.session),
        ),
    }
}

/// The models of one folder's config, read in one blocking job.
fn models_of(cwd: &Path, profile: Option<&str>, state: &Path) -> Models {
    let Ok(layers) = reagent_config::load_for(cwd, profile) else {
        return Models::default();
    };
    let settings = reagent_config::merge_all(&layers);
    let logins = reagent_config::auth::dir()
        .and_then(|dir| reagent_config::auth::read(&dir))
        .unwrap_or_default();
    let cache = crate::models::cache_dir();
    let folder = paths::sessions_root(state).join(paths::cwd_key(cwd));
    let set = |var: &str| std::env::var_os(var).is_some_and(|value| !value.is_empty());
    models_in(&settings, &logins, &set, cache.as_deref(), Some(&folder))
}

/// What the sheet lists: each provider `settings` names that can sign in, then
/// each built-in preset with no table there that can sign in, with the models
/// `cache` lists for each, and the models the sessions of `folder` used last.
/// `set` says whether an environment variable holds a value.
///
/// No network is read: a provider's models are the ones the models cache holds.
fn models_in(
    settings: &Settings,
    logins: &Logins,
    set: &dyn Fn(&str) -> bool,
    cache: Option<&Path>,
    folder: Option<&Path>,
) -> Models {
    let bare = Table::default();
    let presets = reagent_ai::providers::ALL
        .iter()
        .filter(|preset| !settings.providers.contains_key(preset.name))
        .map(|preset| (preset.name, &bare));
    let tables = settings
        .providers
        .iter()
        .map(|(name, table)| (name.as_str(), table));
    let mut providers = Vec::with_capacity(settings.providers.len());
    for (name, table) in tables.chain(presets) {
        let Some(note) = note_of(name, table, logins, set) else {
            continue;
        };
        providers.push(Provider {
            name: Arc::from(name),
            note,
            models: cache.map_or_else(Vec::new, |dir| entries_in(dir, name)),
        });
    }
    Models {
        recent: folder.map_or_else(Vec::new, recent_in),
        providers,
    }
}

/// How `name` signs in, or `None` when it has neither a key nor a saved login.
fn note_of(
    name: &str,
    table: &Table,
    logins: &Logins,
    set: &dyn Fn(&str) -> bool,
) -> Option<Arc<str>> {
    if let Some(var) = env_var(name, table)
        && set(var)
    {
        return Some(Arc::from(format!("key from ${var}")));
    }
    if table.api_key.is_some() {
        return Some(Arc::from("key from config"));
    }
    let login = table.login.as_deref().unwrap_or(name);
    logins.get(login).is_some().then(|| Arc::from("logged in"))
}

/// The variable that holds the key of `name`: the one its table names, else the
/// one its preset names. An empty name is no variable.
fn env_var<'a>(name: &'a str, table: &'a Table) -> Option<&'a str> {
    table
        .api_key_env
        .as_deref()
        .filter(|var| !var.is_empty())
        .or_else(|| preset(name, table).map(|preset| preset.api_key_env))
        .filter(|var| !var.is_empty())
}

/// The preset a provider table takes its defaults from.
fn preset<'a>(name: &'a str, table: &'a Table) -> Option<&'static reagent_ai::Preset> {
    reagent_ai::providers::get(table.preset.as_deref().unwrap_or(name))
}

/// The models the cache of `name` lists, with the window and the price of
/// each. The cache prices a token; the sheet shows a million of them.
fn entries_in(dir: &Path, name: &str) -> Vec<Model> {
    let Some(cache) = reagent_ai::catalog::read(dir, name) else {
        return Vec::new();
    };
    cache
        .models
        .into_iter()
        .map(|entry| Model {
            id: Arc::from(format!("{name}/{}", entry.id)),
            window: entry.context_window,
            prices: entry
                .price
                .map(|price| (price.input * MILLION, price.output * MILLION)),
            reasoning: entry.reasoning,
        })
        .collect()
}

/// The distinct `provider/model` of the sessions in `folder`, newest first, at
/// most [`RECENT`] of them.
fn recent_in(folder: &Path) -> Vec<Arc<str>> {
    let Ok(list) = SessionList::open(folder, crate::session::entry_of) else {
        return Vec::new();
    };
    let mut recent: Vec<Arc<str>> = Vec::with_capacity(RECENT);
    for entry in list.all() {
        if entry.provider.is_empty() || entry.model.is_empty() {
            continue;
        }
        let id: Arc<str> = Arc::from(format!("{}/{}", entry.provider, entry.model));
        if recent.contains(&id) {
            continue;
        }
        recent.push(id);
        if recent.len() == RECENT {
            break;
        }
    }
    recent
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use reagent_config::Secret;
    use reagent_config::auth::Login;

    /// A variable the tests leave unset, and one they know is set.
    const UNSET: &str = "REAGENT_TUI_HOST_TEST_KEY_UNSET";
    const SET: &str = "PATH";

    /// The providers of a config: one with a key, one with a saved login, one
    /// whose variable is set, and one with nothing at all.
    fn settings() -> Settings {
        let mut settings = Settings::default();
        settings.providers.insert(
            "paid".to_string(),
            Table {
                api_key: Some(Secret::from("sk-paid")),
                ..Table::default()
            },
        );
        settings
            .providers
            .insert("logged".to_string(), Table::default());
        settings.providers.insert(
            "wired".to_string(),
            Table {
                api_key_env: Some(SET.to_string()),
                ..Table::default()
            },
        );
        settings.providers.insert(
            "unset".to_string(),
            Table {
                api_key_env: Some(UNSET.to_string()),
                ..Table::default()
            },
        );
        settings
    }

    /// Writes the cache file of `paid` with one priced model.
    fn cache(folder: &Path) {
        use reagent_ai::catalog::{Cache, Entry, Price};
        let entry = Entry {
            id: "fast".to_string(),
            context_window: Some(200_000),
            max_output: None,
            price: Some(Price {
                input: 0.000003,
                output: 0.000015,
                cache_read: 0.0,
                cache_write: 0.0,
            }),
            price_source: None,
            reasoning: None,
            images: None,
        };
        reagent_ai::catalog::write(
            folder,
            "paid",
            &Cache {
                fetched_at: 1,
                models: vec![entry],
            },
        )
        .expect("writes the cache");
    }

    fn logins() -> Logins {
        let mut logins = Logins::default();
        logins.set(
            "logged",
            Login::Key {
                key: Secret::from("sk-logged"),
            },
        );
        logins
    }

    /// A variable is set only when it is [`SET`], so the real environment of
    /// the test, such as a set `OPENROUTER_API_KEY`, lists no preset.
    fn env(var: &str) -> bool {
        var == SET
    }

    #[test]
    fn models_tui_host_lists_the_presets_that_can_sign_in() {
        let mut settings = settings();
        settings
            .providers
            .insert("deepseek".to_string(), Table::default());
        let mut logins = logins();
        for name in ["chatgpt", "deepseek"] {
            logins.set(
                name,
                Login::Key {
                    key: Secret::from("sk-saved"),
                },
            );
        }
        let has = |var: &str| var == "OPENROUTER_API_KEY";

        let models = models_in(&settings, &logins, &has, None, None);

        let names: Vec<&str> = models.providers.iter().map(|one| &*one.name).collect();
        assert_eq!(
            names,
            ["deepseek", "logged", "paid", "chatgpt", "openrouter"],
            "the tables first, then each preset with a login or a key, once"
        );
        assert_eq!(&*models.providers[3].note, "logged in");
        assert_eq!(&*models.providers[4].note, "key from $OPENROUTER_API_KEY");
    }

    #[test]
    fn models_tui_host_lists_the_providers_that_can_sign_in() {
        let cache_dir = tempfile::tempdir().expect("temp dir");
        cache(cache_dir.path());

        let models = models_in(&settings(), &logins(), &env, Some(cache_dir.path()), None);

        let names: Vec<&str> = models.providers.iter().map(|one| &*one.name).collect();
        assert_eq!(
            names,
            ["logged", "paid", "wired"],
            "no provider without a key"
        );
        assert_eq!(&*models.providers[0].note, "logged in");
        assert_eq!(&*models.providers[1].note, "key from config");
        assert_eq!(&*models.providers[2].note, format!("key from ${SET}"));
        let listed = &models.providers[1].models;
        assert_eq!(listed.len(), 1);
        assert_eq!(&*listed[0].id, "paid/fast");
        assert_eq!(listed[0].window, Some(200_000));
        let (input, output) = listed[0].prices.expect("the entry prices the model");
        assert!((input - 3.0).abs() < 1e-9, "{input}");
        assert!((output - 15.0).abs() < 1e-9, "{output}");
        assert!(models.providers[2].models.is_empty(), "no cache, no model");
    }

    /// The model list of the TUI carries the reasoning plan of the cache, so the
    /// level sheet lists the words of the model the session runs.
    #[test]
    fn the_tui_model_list_carries_the_reasoning_plan() {
        use reagent_ai::catalog::{Cache, Entry};
        use reagent_ai::levels::{Level, Levels, Reasoning};

        let cache_dir = tempfile::tempdir().expect("temp dir");
        let mut levels = Levels::default();
        levels.insert(Level::Low);
        levels.insert(Level::High);
        let entry = Entry {
            id: "fast".to_string(),
            context_window: Some(200_000),
            max_output: None,
            price: None,
            price_source: None,
            reasoning: Some(Reasoning {
                levels,
                off: Some(true),
                mandatory: false,
                default: Some(Level::High),
                unknown: vec!["ultra".to_string()],
            }),
            images: None,
        };
        reagent_ai::catalog::write(
            cache_dir.path(),
            "paid",
            &Cache {
                fetched_at: 1,
                models: vec![entry],
            },
        )
        .expect("writes the cache");

        let models = models_in(&settings(), &logins(), &env, Some(cache_dir.path()), None);
        let plan = models.providers[1].models[0]
            .reasoning
            .as_ref()
            .expect("the plan of the model");
        assert_eq!(&*models.providers[1].models[0].id, "paid/fast");
        assert_eq!(plan.off, Some(true));
        assert!(!plan.mandatory);
        assert_eq!(plan.default, Some(Level::High));
        assert!(plan.levels.contains(Level::Low));
        assert!(plan.levels.contains(Level::High));
        assert_eq!(plan.unknown, ["ultra"]);
    }

    #[tokio::test]
    async fn list_files_tui_host_leaves_the_ignored_ones_out_and_cuts_at_the_cap() {
        let root = tempfile::tempdir().expect("temp dir");
        for (name, text) in [
            (".gitignore", "*.log\n"),
            ("a.txt", "one\n"),
            ("b.log", "two\n"),
            ("c.txt", "three\n"),
        ] {
            tokio::fs::write(root.path().join(name), text)
                .await
                .expect("writes a file");
        }

        let (paths, cut) = reagent_tools::list_files(root.path(), 2).await;

        assert_eq!(paths, [".gitignore", "a.txt"]);
        assert!(cut, "the list stopped at the cap");
        let (all, uncut) = reagent_tools::list_files(root.path(), 100).await;
        assert_eq!(all, [".gitignore", "a.txt", "c.txt"], "the ignored file");
        assert!(!uncut);
    }

    /// The login hands its page to the callback of its caller and never a key.
    /// The flow then waits for a callback that never comes here.
    #[tokio::test]
    async fn login_steps_tui_host_carry_the_url_of_the_login() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (sender, mut opened) = tokio::sync::mpsc::channel(1);
        let folder = dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            let open = |url: &str| {
                let _ = sender.try_send(url.to_string());
            };
            let code = |_: &str, _: &str| {};
            let line = |_: &str| {};
            let announce = Announce {
                open: &open,
                code: &code,
                line: &line,
            };
            let pasted: &'static [u8] = b"";
            crate::login::flow(
                "openrouter",
                "openrouter",
                &folder,
                false,
                &announce,
                pasted,
            )
            .await
        });
        let url = tokio::time::timeout(Duration::from_secs(10), opened.recv())
            .await
            .expect("the login announces its page")
            .expect("the sender lives");
        task.abort();
        assert!(url.starts_with("https://openrouter.ai/auth?"), "{url}");
        assert!(!url.contains("sk-"), "{url}");
    }

    /// A provider the command cannot log in to fails the login at once, and
    /// the channel closes: the sheet waits for one step or the end.
    #[tokio::test]
    async fn login_steps_tui_host_end_at_the_reason_of_a_refused_provider() {
        let host = CliHost::new(PathBuf::from("."), None, std::env::temp_dir(), Vec::new());
        assert_eq!(host.logins(), ["openrouter", "chatgpt", "radius"]);

        let mut steps = host.login("no-such-provider");
        let step = tokio::time::timeout(Duration::from_secs(10), steps.recv())
            .await
            .expect("a step in time")
            .expect("a step");
        let LoginStep::Failed(reason) = step else {
            panic!("a refused provider fails the login: {step:?}");
        };
        assert!(reason.contains("login supports"), "{reason}");
        assert_eq!(steps.recv().await, None, "the channel closes");
    }

    #[tokio::test]
    async fn open_tui_host_answers_a_live_entry_without_a_worker() {
        let state = tempfile::tempdir().expect("state");
        let cwd = tempfile::tempdir().expect("cwd");
        let folder = discovery::folder(state.path());
        let entry = discovery::Entry {
            pid: std::process::id(),
            instance: "one".to_string(),
            major: reagent_protocol::message::MAJOR,
            token: "join-token".to_string(),
            socket: folder.join("s1.sock"),
            session: "s1".to_string(),
            cwd: cwd.path().to_path_buf(),
            started_ms: 7,
            waiting: false,
            state: reagent_protocol::discovery::State::Idle,
            wait: None,
            terminal: None,
        };
        discovery::write(&folder, &entry)
            .await
            .expect("writes the entry");
        let host = CliHost::new(
            cwd.path().to_path_buf(),
            None,
            state.path().to_path_buf(),
            Vec::new(),
        );

        let opened = host
            .open(Target::Session("s1".to_owned()))
            .await
            .expect("the live entry answers");

        assert_eq!(opened.socket, entry.socket);
        assert_eq!(opened.token, "join-token");
        let session_dir = paths::sessions_root(state.path())
            .join(paths::cwd_key(cwd.path()))
            .join("s1");
        assert_eq!(opened.session_dir, Some(session_dir));
        assert_eq!(
            discovery::list(&folder).await.len(),
            1,
            "the answer started no worker"
        );
    }

    /// A session folder of `cwd` under `state`, with one part, and the folder.
    fn session(state: &Path, cwd: &Path, id: &str) -> PathBuf {
        let dir = paths::sessions_root(state)
            .join(paths::cwd_key(cwd))
            .join(id);
        let writer = reagent_store::Writer::create(
            &dir,
            reagent_store::Header {
                part_id: 1,
                session_id: id.to_owned(),
                created_ms: 1,
                fork: None,
                meta: Default::default(),
            },
        )
        .expect("a session part");
        writer.close().expect("close the part");
        assert!(dir.is_dir());
        dir
    }

    #[tokio::test]
    async fn delete_tui_host_removes_a_session_folder() {
        let state = tempfile::tempdir().expect("state");
        let cwd = tempfile::tempdir().expect("cwd");
        let dir = session(state.path(), cwd.path(), "s1");
        let host = CliHost::new(
            cwd.path().to_path_buf(),
            None,
            state.path().to_path_buf(),
            Vec::new(),
        );

        let answer = host.delete("s1").await;

        assert_eq!(answer, Ok(()));
        assert!(!dir.exists(), "the session folder is gone");
        let folder = paths::sessions_root(state.path()).join(paths::cwd_key(cwd.path()));
        let list = reagent_store::SessionList::open(&folder, crate::session::entry_of)
            .expect("the session list")
            .all()
            .to_vec();
        assert!(list.is_empty(), "the list names the session no more");
    }

    #[tokio::test]
    async fn delete_tui_host_answers_err_for_an_unknown_id() {
        let state = tempfile::tempdir().expect("state");
        let cwd = tempfile::tempdir().expect("cwd");
        let host = CliHost::new(
            cwd.path().to_path_buf(),
            None,
            state.path().to_path_buf(),
            Vec::new(),
        );

        let answer = host.delete("0000").await;

        let Err(text) = answer else {
            panic!("an unknown session has no folder: {answer:?}");
        };
        assert!(text.contains("no session matches"), "{text}");
    }
}
