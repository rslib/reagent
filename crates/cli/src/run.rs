//! Runs one headless agent session and produces its output.

use std::future::Future;
use std::io::IsTerminal;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncReadExt;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use reagent_agent::agent_loop::{Agent, AgentConfig};
use reagent_agent::archive::{Archive, Expand};
use reagent_agent::compact::Compact;
use reagent_agent::event::{Event, TurnEndReason};
use reagent_agent::guard::Guard;
use reagent_agent::policy::TurnPolicy;
use reagent_agent::policy::budget::Budget;
use reagent_agent::policy::completion::Completion;
use reagent_agent::policy::stuck::Stuck;
use reagent_agent::tool::{Registry, Tool};
use reagent_agent::workspace::Workspace;
use reagent_ai::levels::{Level, Reasoning};
use reagent_ai::oauth::session::{Sleeper, Wait};
use reagent_ai::{Provider, StopReason, Text, Usage};
use reagent_config::Secret;
use reagent_config::mcp::McpDirect;
use reagent_config::permissions::Rules;
use reagent_script::codemode::{Codemode, ToolSource};
use reagent_tools::bash::{Bash, Fallback, Launcher};
use reagent_tools::edit::Edit;
use reagent_tools::find::Find;
use reagent_tools::grep::Grep;
use reagent_tools::read::Read;
use reagent_tools::write::Write;

use crate::args::{Args, SandboxMode, TOOL_NAMES};
use crate::prompt;

/// The tools' working folder, fingerprinted for the completion policy, and
/// the configured folders the fingerprint leaves out.
struct Folder(std::path::PathBuf, Vec<String>);

impl Workspace for Folder {
    fn fingerprint(&self) -> Pin<Box<dyn Future<Output = Option<u64>> + Send + '_>> {
        Box::pin(reagent_tools::workspace::fingerprint_with_skip(
            &self.0, &self.1,
        ))
    }
}

/// An async line writer over the process stdout. A write that fails (for
/// example a closed pipe) stops further writes instead of panicking, so a
/// piped consumer can close early.
struct Out<W = tokio::io::Stdout> {
    writer: W,
    online: bool,
}

impl Out<tokio::io::Stdout> {
    fn new() -> Self {
        Self::with_writer(tokio::io::stdout())
    }
}

impl<W: AsyncWrite + Unpin> Out<W> {
    /// An `Out` over any writer, so a test can count its writes and flushes.
    fn with_writer(writer: W) -> Self {
        Self {
            writer,
            online: true,
        }
    }

    /// Writes `text` plus a trailing newline when it has none. Returns false
    /// once a write has failed; later writes no-op.
    async fn line(&mut self, text: &str) -> bool {
        if !self.online {
            return false;
        }
        if !self.write(text.as_bytes()).await {
            return false;
        }
        if !text.is_empty() && !text.ends_with('\n') {
            self.write(b"\n").await
        } else {
            true
        }
    }

    /// Writes `bytes` and flushes. On any error, takes the writer offline.
    async fn write(&mut self, bytes: &[u8]) -> bool {
        let result = async {
            self.writer.write_all(bytes).await?;
            self.writer.flush().await
        }
        .await;
        let online = result.is_ok();
        if !online {
            self.online = false;
        }
        online
    }
}

/// The provider-call cap, so a looping model cannot run without end.
const MAX_TURNS: u32 = 200;

/// What cancelled the run, stored once by whichever fires first.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Cause {
    None = 0,
    Deadline = 1,
    CtrlC = 2,
}

impl Cause {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Cause::Deadline,
            2 => Cause::CtrlC,
            _ => Cause::None,
        }
    }
}

/// Records `cause` unless another cause came first, then cancels.
fn fire(slot: &AtomicU8, cause: Cause, token: &CancellationToken) {
    let _ = slot.compare_exchange(
        Cause::None as u8,
        cause as u8,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );
    token.cancel();
}

/// Installs the handlers that cancel a run: Ctrl-C, and on unix also SIGTERM
/// and SIGHUP, which a service manager or a closed terminal sends.
fn signal_watchers(cancel: &CancellationToken, cause: &Arc<AtomicU8>) -> Vec<JoinHandle<()>> {
    let mut watchers = Vec::new();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        for kind in [
            SignalKind::interrupt(),
            SignalKind::terminate(),
            SignalKind::hangup(),
        ] {
            let mut stream = match signal(kind) {
                Ok(stream) => stream,
                Err(error) => {
                    eprintln!("reagent: cannot listen for a signal: {error}");
                    continue;
                }
            };
            let token = cancel.clone();
            let cause = Arc::clone(cause);
            watchers.push(tokio::spawn(async move {
                if stream.recv().await.is_some() {
                    fire(&cause, Cause::CtrlC, &token);
                }
            }));
        }
    }
    #[cfg(not(unix))]
    {
        let token = cancel.clone();
        let cause = Arc::clone(cause);
        watchers.push(tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                fire(&cause, Cause::CtrlC, &token);
            }
        }));
    }
    watchers
}

/// Reads the whole task from standard input (`-p -`).
///
/// Returns an error message for empty or non-UTF-8 input; the caller exits 2.
async fn read_stdin_task() -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut stdin = tokio::io::stdin();
    stdin
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("cannot read the task from stdin: {error}"))?;
    if bytes.is_empty() {
        return Err("the task from stdin is empty".into());
    }
    match String::from_utf8(bytes) {
        Ok(task) => Ok(task),
        Err(_) => Err("the task from stdin is not UTF-8".into()),
    }
}

/// The estimated-token budget that compaction keeps the session under: three
/// quarters of the context window, capped at the default budget.
pub(crate) fn compact_budget(context_window: Option<u64>) -> u64 {
    let default = reagent_agent::compact::BUDGET;
    context_window.map_or(default, |window| {
        (window.saturating_mul(3) / 4).min(default)
    })
}

/// The cache entry the models cache of provider `name` holds for `model`.
pub(crate) fn cached_entry(
    dir: &std::path::Path,
    name: Option<&str>,
    model: &str,
) -> Option<reagent_ai::catalog::Entry> {
    let cache = reagent_ai::catalog::read(dir, name?)?;
    cache.models.into_iter().find(|entry| entry.id == model)
}

/// The context window the models cache of `provider` names for `model`, or
/// `None` when it names none: what a client that switches the model of a worker
/// is told.
pub(crate) fn window_of(dir: &std::path::Path, provider: &str, model: &str) -> Option<u64> {
    cached_entry(dir, Some(provider), model).and_then(|entry| entry.context_window)
}

/// The reasoning text a run sends for `asked` on a model whose cache entry
/// holds `reasoning`, or `None` when the model sends no reasoning for it.
pub(crate) fn reasoning_text(asked: &str, reasoning: Option<&Reasoning>) -> Option<String> {
    map_reasoning(asked, reasoning).sent
}

/// The provider and model text a run uses: `provider/model` when it names a
/// provider, else the model alone.
///
/// The start line and the answer a worker gives to `initialize` both name it.
fn model_text(provider: Option<&str>, model: &str) -> String {
    match provider {
        Some(name) => format!("{name}/{model}"),
        None => model.to_string(),
    }
}

/// What a run sends as its reasoning text, and the note the start line prints
/// when the model took another level than the one asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mapped {
    /// The text the request carries, or `None` when nothing is sent.
    sent: Option<String>,
    /// The note the start line appends, such as `reasoning medium -> high`.
    note: Option<String>,
}

/// Maps the asked reasoning text to what the model takes.
///
/// A token budget, a word reagent does not know and a model with no reasoning
/// data pass through unchanged: the provider is the one that reads them. A
/// named level the model lacks becomes the next higher one it takes, else the
/// next lower. `off` becomes a level when the model cannot turn thinking off.
fn map_reasoning(asked: &str, reasoning: Option<&Reasoning>) -> Mapped {
    let mut mapped = map_level(asked, reasoning);
    let Some(reasoning) = reasoning.filter(|reasoning| !reasoning.unknown.is_empty()) else {
        return mapped;
    };
    let text = asked.trim();
    let level = match reagent_ai::Reasoning::parse(text) {
        reagent_ai::Reasoning::Level(_) => true,
        asked => asked.is_off(),
    };
    // A word the model lists is sent as written; beside any other level the
    // words reagent cannot place are named, so the user can ask for one.
    if level && !reasoning.takes_unknown(text) {
        let words = format!("unknown levels {}", reasoning.unknown.join(" "));
        mapped.note = Some(match mapped.note {
            Some(note) => format!("{note}, {words}"),
            None => words,
        });
    }
    mapped
}

/// The level a run sends for `asked`, and the note that names a change.
fn map_level(asked: &str, reasoning: Option<&Reasoning>) -> Mapped {
    let text = asked.trim();
    if text.is_empty() {
        return Mapped {
            sent: None,
            note: None,
        };
    }
    let unchanged = || Mapped {
        sent: Some(text.to_string()),
        note: None,
    };
    // Only a level is mapped: a budget is a number the provider reads itself.
    let asked = reagent_ai::Reasoning::parse(text);
    let Some(reasoning) = reasoning.filter(|reasoning| !reasoning.is_empty()) else {
        return unchanged();
    };
    if reasoning.takes_unknown(text) {
        return unchanged();
    }
    if asked.is_off() {
        // Thinking that cannot be turned off starts at the model's default
        // level, else at the lowest one it takes.
        if reasoning.off == Some(false) || reasoning.mandatory {
            let Some(level) = reasoning.default.or_else(|| reasoning.levels.lowest()) else {
                return unchanged();
            };
            return Mapped {
                sent: Some(level.name().to_string()),
                note: Some(format!(
                    "reasoning off -> {} (thinking is mandatory)",
                    level.name()
                )),
            };
        }
        return unchanged();
    }
    let reagent_ai::Reasoning::Level(_) = asked else {
        return unchanged();
    };
    let Some(asked) = Level::parse(text) else {
        return unchanged();
    };
    if reasoning.levels.contains(asked) {
        return unchanged();
    }
    let Some(level) = reasoning
        .levels
        .above(asked)
        .or_else(|| reasoning.levels.below(asked))
    else {
        return unchanged();
    };
    Mapped {
        sent: Some(level.name().to_string()),
        note: Some(format!("reasoning {} -> {}", asked.name(), level.name())),
    }
}

/// What a run's usage costs, and whether a peak rule applies.
#[derive(Clone, Copy, Debug)]
struct Billing {
    /// The price in USD per token.
    price: reagent_ai::catalog::Price,
    /// The peak windows of a price that came from the models.dev catalogue.
    peak: Option<reagent_ai::providers::Peak>,
}

/// The price of a run: the model's config price, else the price of its model
/// list, which may have come from the models.dev catalogue. Only a catalogue
/// price brings the peak rule of its preset; a config price is final.
fn billing(
    config: Option<reagent_config::ModelPrice>,
    preset: Option<&'static reagent_ai::providers::Preset>,
    cached: Option<&reagent_ai::catalog::Entry>,
) -> Option<Billing> {
    if let Some(price) = config {
        return Some(Billing {
            price: per_token(price),
            peak: None,
        });
    }
    let entry = cached?;
    let peak = match entry.price_source {
        Some(reagent_ai::catalog::PriceSource::ModelsDev) => preset.and_then(|preset| preset.peak),
        _ => None,
    };
    Some(Billing {
        price: entry.price?,
        peak,
    })
}

/// A config price in USD per 1M tokens as USD per token.
fn per_token(price: reagent_config::ModelPrice) -> reagent_ai::catalog::Price {
    const MILLION: f64 = 1_000_000.0;
    reagent_ai::catalog::Price {
        input: price.input / MILLION,
        output: price.output / MILLION,
        cache_read: price.cached / MILLION,
        cache_write: price.cache_write.unwrap_or(0.0) / MILLION,
    }
}

/// The current Unix second.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The one stderr line a text-mode run ends with: turns, context use, cost and
/// cache hit rate. A part that is unknown is left out.
fn end_line(
    turns: u32,
    context_tokens: u64,
    context_window: Option<u64>,
    cost: Option<f64>,
    cache_hit_rate: f64,
) -> String {
    let noun = if turns == 1 { "turn" } else { "turns" };
    let mut line = format!(
        "{turns} {noun}, {}",
        context_use(context_tokens, context_window)
    );
    if let Some(cost) = cost {
        line.push_str(&format!(", ${cost:.4}"));
    }
    let percent = (cache_hit_rate * 100.0).round() as u32;
    line.push_str(&format!(", {percent}% cached"));
    line
}

/// The context part of [`end_line`]: `84k/200k context`, or `84k context` when
/// the window is unknown.
fn context_use(tokens: u64, window: Option<u64>) -> String {
    match window {
        Some(window) => format!("{}/{} context", kilo(tokens), kilo(window)),
        None => format!("{} context", kilo(tokens)),
    }
}

/// Whole thousands of tokens, with no decimals: `84k`.
fn kilo(tokens: u64) -> String {
    format!("{}k", tokens.saturating_add(500) / 1000)
}

/// Runs the agent for `args` and returns the process exit code.
pub async fn run(mut args: Args) -> ExitCode {
    // Resolve the task text before anything runs: `-p -` reads the whole task
    // from standard input, and an empty or non-UTF-8 task input exits 2.
    // The task moves out of `args`; `args` is still borrowed below. A continue
    // or a resume may run with no message at all.
    let source = args.task.take();
    // A run with a task and no terminal UI is headless: `-p`, or a task piped
    // in. The worker a terminal UI starts has no task, so it is not one.
    let headless = source.is_some();
    let mut task = match source {
        None => None,
        Some(crate::args::TaskSource::Text(text)) => Some(text),
        Some(crate::args::TaskSource::Stdin) => match read_stdin_task().await {
            Ok(task) => Some(task),
            Err(message) => {
                eprintln!("reagent: {message}");
                return ExitCode::from(2);
            }
        },
    };
    let cwd = match tokio::fs::canonicalize(&args.cwd).await {
        Ok(cwd) if tokio::fs::metadata(&cwd).await.is_ok_and(|m| m.is_dir()) => cwd,
        _ => {
            eprintln!("reagent: --cwd {} is not a folder", args.cwd.display());
            return ExitCode::from(2);
        }
    };
    let title = task
        .as_deref()
        .map(reagent_store::list::title)
        .unwrap_or_default();
    // A resumed run keeps the provider and the model the session ran with,
    // unless the arguments name one. Naming them before the part is planned
    // puts them in its header and its list entry, so the session shows the
    // model the run uses.
    if !args.model_given && args.profile.is_none() {
        match stored_model(&args, &cwd, &title, !args.provider_given, headless).await {
            Ok(Some(model)) => {
                match tokio::task::spawn_blocking(move || crate::args::reparse(&model))
                    .await
                    .unwrap_or_else(|error| Err(format!("could not read the config: {error}")))
                {
                    Ok(crate::args::Outcome::Run(next)) => {
                        let mut next = *next;
                        next.task = None;
                        args = next;
                    }
                    Ok(_) => {}
                    Err(message) => {
                        eprintln!("reagent: {message}");
                        return ExitCode::from(2);
                    }
                }
            }
            Ok(None) => {}
            Err(message) => {
                eprintln!("reagent: {message}");
                return ExitCode::from(1);
            }
        }
    }
    // The models cache names the context window and the price of the model, so
    // it is read once for both.
    let cached = crate::models::cache_dir()
        .and_then(|dir| cached_entry(&dir, args.provider_name.as_deref(), &args.model));
    if args.context_window.is_none() {
        args.context_window = cached.as_ref().and_then(|entry| entry.context_window);
    }
    let billing = billing(args.price, args.preset, cached.as_ref());
    // The asked reasoning level is mapped to what the model takes before the
    // request is built, so the start line can name the level it picked.
    let reasoning = args.reasoning.as_deref().map(|asked| {
        map_reasoning(
            asked,
            cached.as_ref().and_then(|entry| entry.reasoning.as_ref()),
        )
    });
    // The session this run writes, and what it continues from.
    if matches!(args.sessions, crate::args::Sessions::Resume(None)) {
        let cwd = cwd.clone();
        match tokio::task::spawn_blocking(move || crate::session::list(&cwd)).await {
            Ok(Ok(entries)) => {
                crate::session::print_list(&entries);
                return ExitCode::SUCCESS;
            }
            Ok(Err(message)) => {
                eprintln!("reagent: {message}");
                return ExitCode::from(1);
            }
            Err(_) => {
                eprintln!("reagent: the session store failed");
                return ExitCode::from(1);
            }
        }
    }
    let plan = match plan_session(&args, &cwd, &title, headless).await {
        Ok(plan) => plan,
        Err(message) => {
            eprintln!("reagent: {message}");
            return ExitCode::from(1);
        }
    };
    // A worker takes its session before it names it: the lock, the socket and
    // the entry a client reads are all there before the ready line goes out.
    // The model it serves is the one this run uses, and the window is the one
    // the `--json` summary reports.
    let model = model_text(args.provider_name.as_deref(), &args.model);
    let mut worker = match crate::worker::start(
        &args,
        plan.as_ref(),
        &cwd,
        &model,
        args.context_window,
        reasoning.as_ref().and_then(|mapped| mapped.sent.as_deref()),
    )
    .await
    {
        Ok(worker) => worker,
        Err(message) => {
            eprintln!("reagent: {message}");
            return ExitCode::from(1);
        }
    };
    // The environment block starts here, before the provider is built, so its
    // git runs overlap the provider setup. It is awaited below, just before the
    // first message is built.
    let probe = probe_for(&args.policy, &cwd);
    let (provider, session, source) = match build_provider(&args).await {
        Ok(built) => built,
        Err(message) => {
            eprintln!("reagent: {message}");
            return ExitCode::from(2);
        }
    };
    // The one line before the first request, in both output modes: the
    // profile, the model and where the credential came from, never the key.
    eprintln!(
        "{}",
        start_line(
            args.profile.as_deref(),
            args.provider_name.as_deref(),
            &args.model,
            source.as_ref(),
            reasoning.as_ref().and_then(|mapped| mapped.note.as_deref()),
        )
    );

    // Masking and compaction keep removed outputs for the expand tool.
    let archive = args
        .policy
        .iter()
        .any(|name| name == "mask" || name == "compact")
        .then(Archive::new);
    let launcher = match launcher(&args, &cwd) {
        Ok(launcher) => launcher,
        Err(message) => {
            eprintln!("reagent: {message}");
            return ExitCode::from(2);
        }
    };
    let cancel = CancellationToken::new();
    let cause = Arc::new(AtomicU8::new(Cause::None as u8));
    // The handlers go in before the servers start, so a signal during the
    // startup wait takes the same exit path as one during the run.
    let mut timers = signal_watchers(&cancel, &cause);
    if let Some(worker) = worker.as_mut() {
        worker.stop_on(cancel.clone());
    }
    let servers = start_servers(&args, &cancel).await;
    if cancel.is_cancelled() {
        stop_servers(servers).await;
        return exit_code(
            &TurnEndReason::Cancelled,
            Cause::from_u8(cause.load(Ordering::SeqCst)),
        );
    }
    let direct = direct_adapters(&args, servers.as_ref());
    let codemode = chosen(&args.tools, reagent_script::codemode::NAME).then(|| CodemodeSetup {
        rules: args.rules.clone(),
        wall: codemode_wall(args.codemode.wall_time, args.deadline),
        servers: servers.clone(),
        direct: direct
            .iter()
            .map(|adapter| adapter.spec().name.clone())
            .collect(),
    });
    let mut registry = tools(
        &cwd,
        &args.skip,
        archive.as_ref(),
        launcher,
        args.bash_reads,
        &args.tools,
        codemode,
    );
    for adapter in direct {
        registry.add(adapter);
    }
    // Only a run a person can answer through, on a socket or on stdin, offers
    // the question tool. A served run takes clients, but none of them answers
    // its calls, so it offers the tool list of a plain run.
    let questions = asks(&args.tools, worker.is_some() && !args.serve)
        .then(reagent_agent::question::Questions::new);
    if let Some(questions) = &questions {
        registry.add(reagent_agent::question::Ask::new(Arc::clone(questions)));
    }
    if let Err(message) = check_rule_tools(&args.rules, &registry) {
        eprintln!("reagent: {message}");
        stop_servers(servers).await;
        return ExitCode::from(2);
    }

    let global_agents = reagent_config::global_dir().map(|dir| dir.join("AGENTS.md"));
    let instructions = prompt::instructions(
        global_agents.as_deref(),
        reagent_config::repo_root(&cwd).as_deref(),
        &cwd,
    )
    .await;
    let system = prompt::system_with(cwd.to_string_lossy().as_ref(), &instructions);
    let config = AgentConfig {
        model: args.model.clone(),
        max_tokens: args.max_tokens,
        system: Some(system),
        reasoning: reasoning.and_then(|mapped| mapped.sent),
        cache_key: Some(Text::from("headless-v1")),
        max_turns: Some(MAX_TURNS),
        // On until the catalog says the model takes no images.
        images: cached.as_ref().and_then(|entry| entry.images) != Some(false),
    };

    // The loop policies.
    let mut policies: Vec<Box<dyn TurnPolicy>> = Vec::new();
    let mut mask_enabled = false;
    let mut completion_enabled = false;
    let mut compact_enabled = false;
    for name in &args.policy {
        match name.as_str() {
            "completion" => {
                completion_enabled = true;
                policies.push(Box::new(Completion::new()));
            }
            "stuck" => policies.push(Box::new(Stuck::default())),
            "budget" => policies.push(Box::new(Budget::new(
                args.deadline.map(Duration::from_secs),
                Some(MAX_TURNS),
            ))),
            // The probe reads the list up front, so it started already.
            "snapshot" => {}
            "mask" => mask_enabled = true,
            "compact" => compact_enabled = true,
            other => {
                eprintln!("reagent: unknown policy: {other}");
                stop_servers(servers).await;
                return ExitCode::from(2);
            }
        }
    }

    if let Some(seconds) = args.deadline {
        let token = cancel.clone();
        let cause = Arc::clone(&cause);
        timers.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            fire(&cause, Cause::Deadline, &token);
        }));
    }

    let folder = Folder(cwd.clone(), args.skip.clone());
    let mut agent = if policies.is_empty() {
        Agent::new(provider.as_ref(), &registry, config, cancel)
    } else {
        Agent::with_policies(provider.as_ref(), &registry, config, cancel, policies)
    };
    // A switch the run records names the config provider id it moves to, so the
    // id this run started on goes with it and a resume takes both back.
    agent = agent.with_config_provider(args.provider_name.as_deref().map(Text::from));
    if let Some(archive) = &archive {
        agent = agent.with_archive(Arc::clone(archive));
    }
    if mask_enabled {
        agent = agent.with_mask();
    }
    // A served run takes clients, but the answers a person gives are not its:
    // a call the rules send to a person is refused at once, as in a plain run.
    let guard = Guard::new(args.rules.clone());
    agent = agent.with_guard(if args.serve {
        guard.without_clients()
    } else {
        guard
    });
    if let Some(questions) = &questions {
        agent = agent.with_questions(Arc::clone(questions));
    }
    if compact_enabled {
        agent = agent.with_compact(match args.context_window {
            Some(_) => Compact::with_budget(compact_budget(args.context_window), cwd.clone()),
            None => Compact::new(cwd.clone()),
        });
    } else {
        agent = agent.with_compact_on_request(Compact::new(cwd.clone()));
    }
    if completion_enabled {
        agent = agent.with_workspace(&folder);
    }
    let (tx, rx) = mpsc::channel::<Event>(256);
    let session_id = plan.as_ref().map(|plan| plan.id.clone());
    // A run that takes its clients while it runs serves its session as a worker
    // does, and prints the lines of every run it takes.
    let steered = args.commands || args.listen || args.serve;
    let socket = worker.as_ref().and_then(|worker| worker.socket());
    let report = Report {
        json: args.json,
        // A served run takes clients, but it prints what a plain run prints.
        steered: args.commands || args.listen,
        session: session_id,
        socket,
        billing,
        context_window: args.context_window,
    };
    // The answer lines of the run's stdin client, when it has one, and the end
    // its session reports once it is over.
    let mut answers = None;
    let mut finish = None;
    // A worker's events go to its own tasks; a run prints them through
    // `drain`. A steered run has both: the tracker forwards every event it
    // takes to the writer that prints it.
    let writer = match worker.as_mut() {
        Some(worker) => {
            worker.watch(rx);
            if steered {
                let (forward, printed) = mpsc::channel::<Event>(PRINTED_EVENTS);
                let (lines, received) = mpsc::unbounded_channel::<Vec<u8>>();
                let (ended, last) = oneshot::channel::<TurnEndReason>();
                worker.print(forward);
                answers = Some(lines);
                finish = Some(ended);
                Some(tokio::spawn(drain(
                    printed,
                    Some(received),
                    Some(last),
                    Out::new(),
                    report,
                )))
            } else {
                None
            }
        }
        None => Some(tokio::spawn(drain(rx, None, None, Out::new(), report))),
    };

    if let Some(probe) = probe
        && let Some(text) = task.take()
    {
        task = Some(crate::snapshot::first_message(text, &probe.block().await));
    }
    // The history a resumed run continues. The calls its last reply left open
    // run first, so the message this run adds follows their results and the
    // journal writes the whole history in order.
    let resumed = plan.as_ref().and_then(|plan| plan.resumed.as_ref());
    let mut history: Vec<reagent_ai::Message> = match resumed {
        Some(resumed) => resumed.history.clone(),
        None => Vec::new(),
    };
    let pending = resumed
        .map(|resumed| resumed.pending.clone())
        .unwrap_or_default();
    let carried = crate::session::carried(resumed);
    // The turn numbers run on across a session: this run's first turn is the
    // one after the last turn the session recorded.
    agent = agent.with_first_turn(carried + 1);
    let mut journal = match open_journal(plan.as_ref()).await {
        Ok(journal) => journal,
        Err(message) => {
            stop_servers(servers).await;
            eprintln!("reagent: {message}");
            return ExitCode::from(1);
        }
    };
    // A worker lists its session at its first committed record, so a session
    // it never runs a message in keeps its place in the list and its parts as
    // they were. A `-p` run lists its session at once.
    let lists_at_commit = args.worker;
    // The record the resumed chain ends at before this part appends one, so
    // "nothing new" is this number, not zero.
    let committed_at_start = journal.as_ref().map_or(0, |(journal, _)| journal.record());
    // Recording the entry opens a list and writes it, so it runs on the
    // blocking pool like every other store call.
    if !lists_at_commit && let Some(plan) = journal.as_ref().map(|(_, plan)| plan) {
        let plan = clone_plan(plan);
        match tokio::task::spawn_blocking(move || crate::session::started(&plan, carried)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("reagent: warning: could not record the session: {error}")
            }
            Err(_) => eprintln!("reagent: warning: the session store failed"),
        }
    }
    let mut pending_reason = TurnEndReason::EndTurn;
    // A worker starts serving before the first run, so a client can follow the
    // calls a resumed session left open.
    if let Some(worker) = worker.as_mut() {
        let listed = journal
            .as_ref()
            .filter(|_| lists_at_commit)
            .map(|(_, plan)| crate::session::Listing {
                plan: clone_plan(plan),
                turns: carried,
            });
        worker.begin(committed_at_start, listed);
    }
    // The calls a resumed session left open may ask for an approval, which
    // only stdin can answer when no client joins, so the stdin client starts
    // before them; with none open it starts after the task, which runs first.
    if args.commands
        && !pending.is_empty()
        && let (Some(worker), Some(answers)) = (worker.as_mut(), answers.take())
    {
        worker.stdin_client(tokio::io::stdin(), answers);
    }
    if let Some(worker) = worker.as_mut() {
        if let Some((journal, _)) = journal.as_mut() {
            // A live session answers a call the rules send to a person from its
            // clients, and a cancel stops it.
            pending_reason = worker
                .pending(&mut agent, &mut history, &mut Some(journal), &tx, &pending)
                .await;
        }
    } else if let Some((journal, _)) = journal.as_mut() {
        pending_reason = agent
            .run_pending(&mut history, &mut Some(journal), &tx, &pending)
            .await;
    }
    // A resumed run whose provider or model changed writes them down, so the
    // next resume takes the ones this run used.
    if let Some(resumed) = resumed
        && (resumed.provider != provider.name() || resumed.model != args.model)
        && let Some((journal, _)) = journal.as_mut()
        && let Err(error) = journal
            .model(provider.name(), &args.model)
            .and_then(|()| journal.commit())
    {
        eprintln!("reagent: warning: could not record the model: {error}");
    }
    if matches!(pending_reason, TurnEndReason::Error) {
        eprintln!("reagent: the pending tool calls could not run");
    }
    let mut asked = task.is_some();
    let no_task = task.is_none();
    if let Some(text) = task {
        // The image files the task names by `@path` go with it: the text is
        // kept as written, then one note line and one image per image. A file
        // that is not an image adds nothing, and one that cannot be prepared
        // stops the run before any request.
        if let Some(worker) = worker.as_mut() {
            // A worker's task is its own message, as if a client had sent it.
            worker.task(text).await;
        } else {
            let attached = crate::attach::blocks(&cwd, &text).await;
            let blocks = match attached {
                Ok(blocks) => blocks,
                Err(message) => {
                    eprintln!("reagent: {message}");
                    stop_servers(servers).await;
                    return ExitCode::from(1);
                }
            };
            if blocks.is_empty() {
                history.push(reagent_ai::Message::user(text));
            } else {
                let mut content = Vec::with_capacity(1 + blocks.len());
                content.push(reagent_ai::Content::text(text));
                content.extend(blocks);
                history.push(reagent_ai::Message::user_content(content));
            }
        }
    }
    // The task is queued, so the client that reads the requests on stdin goes
    // after it: a request that comes first would start a run before the task.
    if args.commands
        && let (Some(worker), Some(answers)) = (worker.as_mut(), answers.take())
    {
        worker.stdin_client(tokio::io::stdin(), answers);
    }
    // A crash in the first request of a turn leaves its message with no reply,
    // and the continue asks for that reply.
    asked = asked
        || resumed.is_none_or(|resumed| resumed.open_turn || history.len() > resumed.recorded);
    // A steered run with no task owes the model the reply its session left open,
    // and asks for it here as a plain continue does. The client on stdin is
    // already reading, so a steer or a cancel reaches this run; the session loop
    // after it waits for what comes next.
    if steered
        && !args.worker
        && no_task
        && !matches!(pending_reason, TurnEndReason::Error)
        && resumed.is_some_and(|resumed| {
            resumed.open_turn || history.len() > resumed.recorded || !pending.is_empty()
        })
        && let Some(worker) = worker.as_mut()
        && let Some((journal, _)) = journal.as_mut()
    {
        pending_reason = worker
            .resume(&mut agent, &mut history, &mut Some(journal), &tx)
            .await;
    }
    if let Some(worker) = worker.as_mut() {
        // The worker's session runs from its inbox until its last client
        // leaves; the task it was given is already queued there.
        worker
            .serve(
                &mut agent,
                &mut history,
                journal.as_mut().map(|(journal, _)| &mut *journal),
                &tx,
            )
            .await;
    } else if !matches!(pending_reason, TurnEndReason::Error)
        && (asked || !pending.is_empty() || resumed.is_none())
    {
        agent
            .run(
                &mut history,
                journal.as_mut().map(|(journal, _)| &mut *journal),
                &tx,
            )
            .await;
    }
    drop(tx);
    // The turns the session has now: the number of the last turn this run
    // ended, or the one it found when it ended no turn.
    let turns = agent.last_turn();
    // A refresh that could not be saved costs the next run a refresh; the run
    // that traded the tokens keeps going, so this is a warning, printed once.
    if let Some(warning) = session.as_ref().and_then(|session| session.take_warning()) {
        eprintln!("reagent: warning: could not save the refreshed chatgpt login: {warning}");
    }
    for timer in timers {
        timer.abort();
    }
    // The writer task never panics; a join error would mean a runtime bug.
    let mut reason = match (writer, worker.as_mut()) {
        (Some(writer), Some(worker)) => {
            // A steered run ends on its own rule, so the end its session reports
            // is the one its writer prints; it waits for it before its summary.
            let reason = worker.reason().await;
            if let Some(finish) = finish.take() {
                let _ = finish.send(reason);
            }
            let _ = writer.await;
            reason
        }
        (Some(writer), None) => writer
            .await
            .unwrap_or_else(|_| panic!("writer task failed")),
        // A worker's writer is its tracker, which ends with the channel.
        (None, Some(worker)) => worker.reason().await,
        (None, None) => pending_reason,
    };
    // A run whose pending calls failed ends with that reason: the exit code and
    // the list entry say the run did not finish.
    if matches!(pending_reason, TurnEndReason::Error) {
        reason = TurnEndReason::Error;
    }

    stop_servers(servers).await;
    // The part is closed and the list entry says how the run ended, so a crash
    // before this leaves the entry open.
    if let Some((journal, plan)) = journal.take() {
        let state = crate::session::state_of(matches!(
            reason,
            TurnEndReason::EndTurn | TurnEndReason::MaxTurns
        ));
        // A worker whose session committed no record serves nothing worth
        // keeping: a new session's folder goes and the list never takes it,
        // and a resumed session keeps its folder, its parts and its entry.
        let unlisted = lists_at_commit && journal.record() == committed_at_start;
        let closed = tokio::task::spawn_blocking(move || {
            if unlisted {
                crate::session::discard(journal, &plan)
            } else {
                crate::session::close(journal, &plan, state, turns)
            }
        })
        .await;
        match closed {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                eprintln!("reagent: warning: could not close the session: {message}")
            }
            Err(_) => eprintln!("reagent: warning: the session writer stopped"),
        }
    }
    // Deleting the sessions the limits no longer keep runs after the final
    // output, so it costs the run no latency, and at most once a day. The run
    // is done either way, so a failure only warns.
    if let Some(plan) = &plan {
        let (keep, limits) = (plan.dir.clone(), crate::gc::Limits::from(args.session));
        match tokio::task::spawn_blocking(move || crate::gc::after_run(&keep, limits)).await {
            Ok(Ok(_)) => {}
            Ok(Err(message)) => {
                eprintln!("reagent: warning: could not delete old sessions: {message}")
            }
            Err(_) => eprintln!("reagent: warning: the session sweep failed"),
        }
    }
    // A worker stops because its last client left, so its exit code says it
    // stopped rather than how its last run ended. A steered run is a `-p` run
    // and ends with the reason of its session.
    if args.worker {
        return ExitCode::SUCCESS;
    }
    let cause = Cause::from_u8(cause.load(Ordering::SeqCst));
    if !args.json
        && let Some(note) = stop_note(&reason, cause, plan.as_ref().map(|plan| plan.id.as_str()))
    {
        eprintln!("reagent: {note}");
    }
    exit_code(&reason, cause)
}

/// The provider and model of the session a run continues, as one `--model`
/// value, or `None` when this run starts a session or already names them.
///
/// `with_provider` is off for a run that named a provider: it takes the model
/// of the session, not the provider. Reading the session header blocks, so it
/// runs on the blocking pool.
async fn stored_model(
    args: &Args,
    cwd: &std::path::Path,
    title: &str,
    with_provider: bool,
    headless: bool,
) -> Result<Option<String>, String> {
    let sessions = args.sessions.clone();
    let cwd = cwd.to_path_buf();
    let profile = args.profile.clone();
    let provider = args.provider_name.clone().unwrap_or_default();
    let model = args.model.clone();
    let title = title.to_owned();
    let worker = args.worker;
    let stored = tokio::task::spawn_blocking(move || {
        let ask = crate::session::Ask {
            sessions: &sessions,
            cwd: &cwd,
            profile: profile.as_deref(),
            provider: &provider,
            model: &model,
            title: &title,
            worker,
            headless,
        };
        crate::session::stored(&ask)
    })
    .await
    .map_err(|_| "the session store failed".to_string())??;
    let Some(stored) = stored else {
        return Ok(None);
    };
    // The value a `--model` flag takes: `provider/model`, or the model alone
    // when the session named no provider or the run named one of its own.
    let named = if with_provider && !stored.provider.is_empty() {
        format!("{}/{}", stored.provider, stored.model)
    } else {
        stored.model
    };
    let now = match args.provider_name.as_deref().filter(|_| with_provider) {
        Some(name) => format!("{name}/{}", args.model),
        None => args.model.clone(),
    };
    if named.is_empty() || named == now {
        return Ok(None);
    }
    Ok(Some(named))
}

/// Plans the session of a run, on the blocking pool.
async fn plan_session(
    args: &Args,
    cwd: &std::path::Path,
    title: &str,
    headless: bool,
) -> Result<Option<crate::session::Plan>, String> {
    let sessions = args.sessions.clone();
    let cwd = cwd.to_path_buf();
    let profile = args.profile.clone();
    let provider = args.provider_name.clone().unwrap_or_default();
    let model = args.model.clone();
    let title = title.to_owned();
    // A steered run serves its session to clients as a worker does, so it may
    // continue a session with nothing open and wait for a message.
    let worker = args.worker || args.commands || args.listen;
    tokio::task::spawn_blocking(move || {
        let ask = crate::session::Ask {
            sessions: &sessions,
            cwd: &cwd,
            profile: profile.as_deref(),
            provider: &provider,
            model: &model,
            title: &title,
            worker,
            headless,
        };
        crate::session::plan(&ask)
    })
    .await
    .map_err(|_| "the session store failed".to_string())?
}

/// Opens the journal of a planned part, on the blocking pool.
async fn open_journal(
    plan: Option<&crate::session::Plan>,
) -> Result<Option<(reagent_agent::journal::Journal, crate::session::Plan)>, String> {
    let Some(plan) = plan else {
        return Ok(None);
    };
    let dir = plan.dir.clone();
    let header = plan.header.clone();
    let recorded = plan.resumed.as_ref().map(|resumed| resumed.recorded);
    let journal = tokio::task::spawn_blocking(move || match recorded {
        Some(recorded) => reagent_agent::journal::Journal::resumed(&dir, header, recorded),
        None => reagent_agent::journal::Journal::create(&dir, header),
    })
    .await
    .map_err(|_| "the session store failed".to_string())?
    .map_err(|error| format!("could not open the session: {error}"))?;
    Ok(Some((journal, clone_plan(plan))))
}

/// The plan a run keeps after its journal took the header.
fn clone_plan(plan: &crate::session::Plan) -> crate::session::Plan {
    crate::session::Plan {
        folder: plan.folder.clone(),
        dir: plan.dir.clone(),
        id: plan.id.clone(),
        header: plan.header.clone(),
        resumed: None,
    }
}

/// The bytes of JSON lines the drain loop takes from the channel before one
/// write and one flush.
const JSON_BATCH: usize = 8 * 1024;

/// The most events one receive takes from the channel.
const EVENT_BATCH: usize = 64;

/// The most events a steered run holds for its writer while the tracker takes
/// more.
const PRINTED_EVENTS: usize = 256;

/// What the lines of a run name and price: the session it writes, the socket
/// it takes clients on, the prices it fills in and the context window its end
/// line reports.
struct Report {
    json: bool,
    /// Whether the run writes the lines of a steered run: the ids of the
    /// approvals and questions it answers and where each message it takes
    /// lands. A served run takes clients but writes a plain run's lines.
    steered: bool,
    session: Option<String>,
    socket: Option<String>,
    billing: Option<Billing>,
    context_window: Option<u64>,
}

/// The next thing the drain loop takes: a batch of events, or one answer line
/// of the run's stdin client.
enum Step {
    Events(usize),
    Line(Option<Vec<u8>>),
}

/// Consumes the agent's event channel, prints it and returns how the run ended.
///
/// Owns stdout: in plain mode only the final answer is printed; in JSON mode a
/// start line comes first, every event is one line and a summary follows the
/// run end. `session` is the session the run writes, named by the start line.
/// `billing` fills the cost of a usage the provider did not price, and
/// `context_window` is the window the end line reports.
///
/// A run that serves a session takes `answers` and `last`: the lines the
/// answers of its stdin client carry join the ones of its events, it prints
/// every run the session takes, and `last` is the end its session reports, sent
/// once the session loop has ended.
async fn drain<W: AsyncWrite + Unpin>(
    mut rx: mpsc::Receiver<Event>,
    mut answers: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    last: Option<oneshot::Receiver<TurnEndReason>>,
    mut out: Out<W>,
    report: Report,
) -> TurnEndReason {
    let json = report.json;
    // A session takes one run after another, so its end is the close of the
    // channel, not the end of a run.
    let steered = last.is_some();
    let mut answer = String::new();
    let mut total = Usage::default();
    let mut turns = 0u32;
    let mut reason = TurnEndReason::EndTurn;
    let mut stop = None;
    let mut ended = false;
    // The prompt size of the last turn: how full the context is now.
    let mut context_tokens = 0u64;

    let mut buf = Vec::with_capacity(512);
    // Whether the start line has gone into a batch; it is written once, with
    // the first batch, so it costs no extra write.
    let mut started = false;
    // The events one receive takes. A reply sends one event right after
    // another, so in JSON mode they share one write and one flush instead of
    // taking one per event; the ready events after a batch join it, so a write
    // covers the whole 8 KiB. `recv_many` returns as soon as one event is
    // there, so an idle channel still writes at once and turn end and run end
    // keep their latency.
    let mut batch: Vec<Event> = Vec::with_capacity(EVENT_BATCH);
    // A steered run may wait for a client before any event, and a client finds
    // its socket in this line, so it goes out at once.
    if json && steered {
        crate::json::write_start(
            &mut buf,
            report.session.as_deref(),
            report.socket.as_deref(),
        );
        let _ = out.write(&buf).await;
        buf.clear();
        started = true;
    }
    'run: loop {
        let step = {
            let events = rx.recv_many(&mut batch, EVENT_BATCH);
            match answers.as_mut() {
                Some(lines) => tokio::select! {
                    taken = events => Step::Events(taken),
                    line = lines.recv() => Step::Line(line),
                },
                None => Step::Events(events.await),
            }
        };
        match step {
            // The line of a stdin client goes out at once: the client waits for
            // it, and the batch of events that follows shares no write with it.
            Step::Line(Some(line)) => {
                if json {
                    if !started {
                        crate::json::write_start(
                            &mut buf,
                            report.session.as_deref(),
                            report.socket.as_deref(),
                        );
                        started = true;
                    }
                    buf.extend_from_slice(&line);
                    let _ = out.write(&buf).await;
                    buf.clear();
                }
                continue 'run;
            }
            Step::Line(None) => {
                answers = None;
                continue 'run;
            }
            Step::Events(0) => break 'run,
            Step::Events(_) => {}
        }
        if json {
            buf.clear();
            if !started {
                crate::json::write_start(
                    &mut buf,
                    report.session.as_deref(),
                    report.socket.as_deref(),
                );
                started = true;
            }
        }
        loop {
            for event in batch.drain(..) {
                let event = priced(event, report.billing);
                match &event {
                    Event::TurnStart => {
                        turns += 1;
                        answer.clear();
                    }
                    // A retried attempt starts the reply again, so its partial text goes.
                    Event::Retry { .. } => answer.clear(),
                    // A plain run prints the answer at the end; JSON mode writes the
                    // text on its own line, so it copies nothing.
                    Event::TextDelta(text) if !json => keep_answer(&mut answer, text),
                    Event::Usage(usage) => {
                        context_tokens = crate::json::context_tokens(usage);
                        sum(&mut total, usage);
                    }
                    Event::Error(message) if !json => eprintln!("reagent: {message}"),
                    Event::TurnEnd(r) => reason = *r,
                    Event::RunEnd(r) => {
                        reason = *r;
                        stop = stop_string(r);
                        // A session takes another run after the one that ended.
                        ended = !steered;
                    }
                    _ => {}
                }
                if json {
                    // A steered run's caller answers its approvals, so it needs
                    // their ids, and sees where each message it sent lands; a
                    // plain run prints the lines it always printed.
                    if report.steered
                        && matches!(
                            event,
                            Event::ApprovalRequest(_)
                                | Event::ApprovalResolved { .. }
                                | Event::QuestionRequest(_)
                                | Event::QuestionResolved { .. }
                                | Event::UserMessage { .. }
                                | Event::MessageState { .. }
                        )
                    {
                        crate::json::write_line(&mut buf, &event);
                    } else {
                        crate::json::write_event(&mut buf, &event);
                    }
                    if ended || buf.len() >= JSON_BATCH {
                        let _ = out.write(&buf).await;
                        buf.clear();
                    }
                }
                if ended {
                    break 'run;
                }
            }
            match rx.try_recv() {
                Ok(event) => batch.push(event),
                Err(_) => break,
            }
        }
        if json && !buf.is_empty() {
            let _ = out.write(&buf).await;
            buf.clear();
        }
    }

    // The end of a session is its own, not the end of one of its runs: a
    // session that stopped with no run open has no `run_end` to name it.
    if let Some(last) = last
        && let Ok(seen) = last.await
    {
        reason = seen;
        stop = stop_string(&reason);
    }

    if json {
        buf.clear();
        if !started {
            crate::json::write_start(
                &mut buf,
                report.session.as_deref(),
                report.socket.as_deref(),
            );
        }
        crate::json::write_summary(
            &mut buf,
            &total,
            turns,
            &reason,
            stop,
            context_tokens,
            report.context_window,
        );
        let _ = out.write(&buf).await;
    } else {
        let _ = out.line(&answer).await;
        // A pipe or a file keeps its output clean; a terminal gets the line.
        if std::io::stderr().is_terminal() {
            let line = end_line(
                turns,
                context_tokens,
                report.context_window,
                total.cost,
                crate::json::cache_hit_rate(&total),
            );
            eprintln!("reagent: {line}");
        }
    }

    reason
}

#[cfg(test)]
thread_local! {
    /// The bytes copied into the answer of a run on this thread, so a test can
    /// prove that JSON mode copies none.
    static ANSWER_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Appends one text delta to the answer a plain run prints at the end.
fn keep_answer(answer: &mut String, text: &str) {
    #[cfg(test)]
    ANSWER_BYTES.with(|bytes| bytes.set(bytes.get() + text.len()));
    answer.push_str(text);
}

/// Fills the cost of a usage event from the model price, at the peak rule of
/// the price, when the provider reported none.
fn priced(event: Event, billing: Option<Billing>) -> Event {
    // The clock is read only for an unpriced usage under a peak rule, not for
    // every text delta.
    let timed = billing.is_some_and(|billing| billing.peak.is_some())
        && matches!(&event, Event::Usage(usage) if usage.cost.is_none());
    priced_at(event, billing, if timed { now_secs() } else { 0 })
}

/// [`priced`] at the UTC second `at`: a models.dev price multiplies inside its
/// peak windows. Chinese public holidays are off-peak and not modeled, so a
/// holiday run shows the peak price.
fn priced_at(event: Event, billing: Option<Billing>, at: u64) -> Event {
    let Event::Usage(usage) = &event else {
        return event;
    };
    let Some(billing) = billing.filter(|_| usage.cost.is_none()) else {
        return event;
    };
    let mut usage = *usage;
    let factor = billing.peak.map_or(1.0, |peak| peak.factor_at(at));
    usage.cost = Some(billing.price.cost(&usage) * factor);
    Event::Usage(usage)
}

/// Adds `delta` into the running `total`.
///
/// A cost is known when at least one part of the run reported one, and the
/// parts that did are added up.
fn sum(total: &mut Usage, delta: &Usage) {
    total.input_tokens += delta.input_tokens;
    total.output_tokens += delta.output_tokens;
    total.cache_read_tokens += delta.cache_read_tokens;
    total.cache_write_tokens += delta.cache_write_tokens;
    total.reasoning_tokens += delta.reasoning_tokens;
    total.cost = match (total.cost, delta.cost) {
        (Some(sum), Some(cost)) => Some(sum + cost),
        (Some(sum), None) => Some(sum),
        (None, cost) => cost,
    };
}

/// The provider stop reason as a string, when the run stopped for one.
fn stop_string(reason: &TurnEndReason) -> Option<&'static str> {
    match reason {
        TurnEndReason::Stopped(inner) => Some(match inner {
            StopReason::EndTurn => "end_turn",
            StopReason::ToolUse => "tool_use",
            StopReason::MaxTokens => "max_tokens",
            StopReason::Refusal => "refusal",
            StopReason::Error => "error",
        }),
        _ => None,
    }
}

/// Why a run that did not finish stopped, for a plain run's stderr: these
/// endings send no error event, and the last turn may have left no answer, so
/// nothing else says it. An error printed its own message, and a person who
/// pressed Ctrl+C knows.
fn stop_note(reason: &TurnEndReason, cause: Cause, session: Option<&str>) -> Option<String> {
    let note = match (reason, cause) {
        (TurnEndReason::EndTurn | TurnEndReason::Error, _)
        | (TurnEndReason::Cancelled, Cause::CtrlC) => return None,
        (TurnEndReason::MaxTurns, _) => {
            let mut note =
                format!("stopped at the limit of {MAX_TURNS} turns; the task may not be done");
            if let Some(session) = session {
                note.push_str(&format!(
                    "; continue with: reagent --resume {session} -p continue"
                ));
            }
            return Some(note);
        }
        (TurnEndReason::Cancelled, Cause::Deadline) => {
            "stopped at the deadline; the task may not be done"
        }
        (TurnEndReason::Cancelled, _) => "the run was cancelled",
        (TurnEndReason::Stopped(StopReason::MaxTokens), _) => {
            "stopped: the reply hit the model's output limit"
        }
        (TurnEndReason::Stopped(StopReason::Refusal), _) => {
            "stopped: the model refused the request"
        }
        (TurnEndReason::Stopped(_), _) => "stopped: the provider ended the reply with an error",
    };
    Some(note.to_owned())
}

/// Maps the run outcome to the documented exit code. The cause counts only
/// when the run really ended cancelled, so a late timer cannot turn a
/// finished run into 124.
fn exit_code(reason: &TurnEndReason, cause: Cause) -> ExitCode {
    let code: u8 = match (reason, cause) {
        (TurnEndReason::EndTurn, _) => 0,
        (TurnEndReason::Cancelled, Cause::Deadline) => 124,
        (TurnEndReason::Cancelled, Cause::CtrlC) => 130,
        (
            TurnEndReason::Cancelled
            | TurnEndReason::MaxTurns
            | TurnEndReason::Error
            | TurnEndReason::Stopped(_),
            _,
        ) => 1,
    };
    ExitCode::from(code)
}

/// Where the credential of a run came from, as the start line names it.
///
/// It never holds the key itself, so a line or an error that prints it is
/// safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// The named variable held a key.
    EnvVar(String),
    /// The provider table held the key itself.
    ConfigKey,
    /// A saved login of that name held the key.
    Login(String),
    /// The wire's default variable held a key.
    DefaultEnv(String),
}

impl Source {
    /// The source as the start line writes it: `env <VAR>`, `config key`,
    /// `login <name>` or `env <VAR> (default)`.
    pub(crate) fn text(&self) -> String {
        match self {
            Source::EnvVar(name) => format!("env {name}"),
            Source::ConfigKey => "config key".to_string(),
            Source::Login(name) => format!("login {name}"),
            Source::DefaultEnv(name) => format!("env {name} (default)"),
        }
    }
}

/// The API key and where it came from: the chosen variable when set and not
/// empty, else the key from the config, else the saved login `login`, else the
/// wire's default variable. `var` names the variable read, the chosen one or
/// the wire default.
fn resolve_key(
    chosen: Option<String>,
    var: &str,
    file: Option<&Secret>,
    saved: Option<&str>,
    login: Option<&str>,
    default: Option<String>,
) -> Option<(String, Source)> {
    if let Some(key) = chosen.filter(|key| !key.is_empty()) {
        return Some((key, Source::EnvVar(var.to_string())));
    }
    if let Some(key) = file {
        return Some((key.expose().to_string(), Source::ConfigKey));
    }
    if let Some(key) = saved.filter(|key| !key.is_empty()) {
        return Some((
            key.to_string(),
            Source::Login(login.unwrap_or_default().to_string()),
        ));
    }
    default
        .filter(|key| !key.is_empty())
        .map(|key| (key, Source::DefaultEnv(var.to_string())))
}

/// The key of a saved login `name` in the config folder `dir`.
fn saved_login(
    dir: Option<&std::path::Path>,
    name: Option<&str>,
) -> Result<Option<String>, String> {
    let (Some(dir), Some(name)) = (dir, name) else {
        return Ok(None);
    };
    let logins = reagent_config::auth::read(dir)?;
    Ok(logins
        .get(name)
        .and_then(|login| login.key())
        .map(|key| key.expose().to_string()))
}

/// Whether a key the user gave signs in before a saved browser login.
///
/// The named variable comes first, then the config key. The wire's default
/// variable is not in this list: a saved ChatGPT login beats it, so a key the
/// environment happens to hold does not hide a login made on purpose.
fn key_beats_session(named: Option<&str>, file: Option<&Secret>) -> bool {
    named.is_some_and(|key| !key.is_empty()) || file.is_some()
}

/// The session of a saved ChatGPT login, when nothing else signs in.
///
/// A named variable and a config key both come before it, and the wire's
/// default variable comes after, which is the precedence of every other key.
/// The auth file is read only here, so a broken file still costs a run only
/// when nothing else signs in.
pub(crate) fn saved_session(
    named: Option<&str>,
    file: Option<&Secret>,
    dir: Option<&std::path::Path>,
    name: Option<&str>,
    token_url: &str,
    sleep: Sleeper,
) -> Result<Option<Arc<reagent_ai::oauth::session::Session>>, String> {
    if key_beats_session(named, file) {
        return Ok(None);
    }
    let (Some(dir), Some(name)) = (dir, name) else {
        return Ok(None);
    };
    // A file this build cannot read is no login here: the key path reads it
    // again and reports the problem only when no key is left either.
    let Ok(logins) = reagent_config::auth::read(dir) else {
        return Ok(None);
    };
    if !matches!(
        logins.get(name),
        Some(
            reagent_config::auth::Login::Oauth { .. } | reagent_config::auth::Login::Device { .. }
        )
    ) {
        return Ok(None);
    }
    let session = reagent_ai::oauth::session::Session::load(dir, name, token_url, sleep)?;
    Ok(Some(Arc::new(session)))
}

/// The wait a saved login sleeps on: the timer of the runtime this process
/// runs, handed to `reagent-ai`, which has no timer of its own.
pub(crate) fn sleeper() -> Sleeper {
    Arc::new(|duration: Duration| -> Wait { Box::pin(tokio::time::sleep(duration)) })
}

/// The API key of a provider and where it came from, or `missing <VAR>`
/// naming the variable read.
///
/// `login` names the saved login of `--model name/id`, which is read only when
/// the variable and the config key have nothing, so a refused or broken auth
/// file costs a run only when the key would have come from it.
pub(crate) fn api_key(
    api_key_env: Option<&str>,
    file: Option<&Secret>,
    login: Option<&str>,
    wire: &crate::args::Wire,
) -> Result<(String, Source), String> {
    let var = api_key_env.unwrap_or(wire.default_api_key_env());
    let read = |name: &str| std::env::var(name).ok();
    let chosen = api_key_env.and_then(read);
    let wired = api_key_env.is_none().then(|| read(var)).flatten();
    let saved = match (chosen.as_deref(), file) {
        (Some(key), _) if !key.is_empty() => Ok(None),
        (_, Some(_)) => Ok(None),
        _ => match login.map(|_| reagent_config::auth::dir()) {
            Some(Ok(dir)) => saved_login(Some(&dir), login),
            Some(Err(error)) => Err(error),
            None => Ok(None),
        },
    };
    key_from(chosen, var, file, saved, login, wired)
}

/// The reason a provider with no key variable of its own gives instead of a
/// missing variable: its only credential is a saved login. Any other error,
/// such as a refused auth file, passes through.
pub(crate) fn login_hint(
    preset: Option<&reagent_ai::Preset>,
    api_key_env: Option<&str>,
    name: Option<&str>,
    login: Option<&str>,
    error: String,
) -> String {
    let (Some(preset), Some("")) = (preset, api_key_env) else {
        return error;
    };
    if !error.starts_with("missing ") {
        return error;
    }
    let name = name.unwrap_or(preset.name);
    let login = login.unwrap_or(name);
    format!(
        "{name} needs a login; run {}",
        login_command(preset.name, login)
    )
}

/// The command that saves the login `login` of the preset `preset`.
pub(crate) fn login_command(preset: &str, login: &str) -> String {
    if login == preset {
        format!("reagent login {preset}")
    } else {
        format!("reagent login {preset} --as {login}")
    }
}

/// The key the sources give and where it came from, or the reason there is
/// none.
///
/// A saved login this build cannot read costs the run only when no source
/// gives a key: the wire default variable still answers, and the auth file
/// error comes back only when it is the last source left.
fn key_from(
    chosen: Option<String>,
    var: &str,
    file: Option<&Secret>,
    saved: Result<Option<String>, String>,
    login: Option<&str>,
    wired: Option<String>,
) -> Result<(String, Source), String> {
    match saved {
        Ok(saved) => resolve_key(chosen, var, file, saved.as_deref(), login, wired)
            .ok_or_else(|| format!("missing {var}")),
        Err(error) => resolve_key(chosen, var, file, None, login, wired).ok_or(error),
    }
}

/// The line a run prints before its first request: the profile, the model and
/// where the credential came from, never the key, and the reasoning level it
/// picked when the model took another one.
fn start_line(
    profile: Option<&str>,
    provider: Option<&str>,
    model: &str,
    source: Option<&Source>,
    note: Option<&str>,
) -> String {
    let target = model_text(provider, model);
    let mut line = format!(
        "reagent: profile {}, model {target}, credential {}",
        profile.unwrap_or("none"),
        source.map_or_else(|| "none".to_string(), Source::text)
    );
    if let Some(note) = note {
        line.push_str(", ");
        line.push_str(note);
    }
    line
}

/// Builds the provider named on the command line, or reports why not, with the
/// saved login the run may have to warn about and where the credential came
/// from. The scripted testing provider has no credential, so it gives none.
pub(crate) async fn build_provider(
    args: &Args,
) -> Result<
    (
        Box<dyn Provider>,
        Option<Arc<reagent_ai::oauth::session::Session>>,
        Option<Source>,
    ),
    String,
> {
    #[cfg(feature = "testing")]
    if let crate::args::Wire::Fake(spec) = &args.provider {
        return build_fake(spec)
            .await
            .map(|provider| (Box::new(provider) as Box<dyn Provider>, None, None));
    }

    let base_url = args
        .base_url
        .clone()
        .unwrap_or_else(|| args.provider.base_url());
    // A saved ChatGPT login replaces the key string. A key from the named
    // variable or the config keeps precedence over it.
    let named = args
        .api_key_env
        .as_deref()
        .and_then(|name| std::env::var(name).ok());
    let session = saved_session(
        named.as_deref(),
        args.api_key.as_ref(),
        reagent_config::auth::dir().ok().as_deref(),
        args.login.as_deref(),
        reagent_ai::oauth::chatgpt::TOKEN,
        sleeper(),
    )?;
    let credential = match &session {
        Some(session) => (
            reagent_ai::Credential::Session(Arc::clone(session)),
            Source::Login(args.login.clone().unwrap_or_default()),
        ),
        None => {
            let (api_key, source) = api_key(
                args.api_key_env.as_deref(),
                args.api_key.as_ref(),
                args.login.as_deref(),
                &args.provider,
            )
            .map_err(|error| {
                login_hint(
                    args.preset,
                    args.api_key_env.as_deref(),
                    args.provider_name.as_deref(),
                    args.login.as_deref(),
                    error,
                )
            })?;
            let credential = match (&args.provider, args.auth) {
                (crate::args::Wire::Anthropic, reagent_ai::Auth::Wire) => {
                    reagent_ai::Credential::x_api_key(&api_key)?
                }
                _ => reagent_ai::Credential::bearer(&api_key)?,
            };
            (credential, source)
        }
    };
    let (credential, source) = credential;
    let header = args.header.clone();

    let provider: Box<dyn Provider> = match &args.provider {
        crate::args::Wire::Anthropic => Box::new(
            reagent_ai::anthropic::Anthropic::new("", base_url)
                .with_credential(credential)
                .with_headers(header),
        ),
        crate::args::Wire::OpenAiChat => {
            let chat = reagent_ai::openai::chat::OpenAiChat::new("", base_url)
                .with_credential(credential)
                .with_headers(header);
            Box::new(match args.compat {
                Some(compat) => chat.with_compat(compat),
                None => chat,
            })
        }
        crate::args::Wire::PiMessages => Box::new(
            reagent_ai::pi::PiMessages::new("", base_url)
                .with_credential(credential)
                .with_headers(header),
        ),
        crate::args::Wire::OpenAiResponses => {
            let responses = reagent_ai::openai::responses::OpenAiResponses::new("", base_url)
                .with_credential(credential)
                .with_headers(header);
            Box::new(match args.compat {
                Some(compat) => responses.with_compat(compat),
                None => responses,
            })
        }
        #[cfg(feature = "testing")]
        crate::args::Wire::Fake(_) => unreachable!("the testing provider is handled above"),
    };
    Ok((provider, session, Some(source)))
}

/// Reads a scripted FakeProvider from a JSON file, under the testing feature.
#[cfg(feature = "testing")]
async fn build_fake(file: &str) -> Result<reagent_ai::fake::FakeProvider, String> {
    let text = tokio::fs::read_to_string(file)
        .await
        .map_err(|error| format!("cannot read {file}: {error}"))?;
    fake::from_script(&text)
}

/// A JSON script that builds a FakeProvider for the headless tests.
#[cfg(feature = "testing")]
mod fake {
    use serde::Deserialize;
    use serde_json::Value;

    use reagent_ai::fake::FakeProvider;
    use reagent_ai::{Error, StopReason, StreamEvent, Text};

    /// Each ending that sends no error names itself, and the others stay quiet.
    #[test]
    fn a_plain_run_says_why_it_stopped_for_each_ending() {
        use super::{Cause, TurnEndReason};

        let note = |reason, cause| super::stop_note(&reason, cause, Some("abc"));
        assert_eq!(note(TurnEndReason::EndTurn, Cause::None), None);
        assert_eq!(note(TurnEndReason::Error, Cause::None), None, "it printed");
        assert_eq!(note(TurnEndReason::Cancelled, Cause::CtrlC), None);
        let limit = note(TurnEndReason::MaxTurns, Cause::None).expect("the limit");
        assert!(limit.contains("limit of 200"), "{limit}");
        assert!(limit.contains("--resume abc"), "{limit}");
        let unsaved = super::stop_note(&TurnEndReason::MaxTurns, Cause::None, None);
        assert!(!unsaved.expect("the limit").contains("--resume"));
        let words = [
            (TurnEndReason::Cancelled, Cause::Deadline, "deadline"),
            (TurnEndReason::Cancelled, Cause::None, "cancelled"),
            (
                TurnEndReason::Stopped(StopReason::MaxTokens),
                Cause::None,
                "output limit",
            ),
            (
                TurnEndReason::Stopped(StopReason::Refusal),
                Cause::None,
                "refused",
            ),
            (
                TurnEndReason::Stopped(StopReason::Error),
                Cause::None,
                "error",
            ),
        ];
        for (reason, cause, word) in words {
            let text = note(reason, cause).expect("a note");
            assert!(text.contains(word), "{word}: {text}");
        }
    }

    /// One parsed reply: assistant text, any tool calls it makes and optional
    /// token usage.
    #[derive(Debug, Deserialize)]
    struct ScriptedReply {
        #[serde(default)]
        text: String,
        /// More text, each item streamed as its own delta after `text`.
        #[serde(default)]
        deltas: Vec<String>,
        #[serde(default)]
        calls: Vec<ScriptedCall>,
        #[serde(default)]
        usage: Option<ScriptedUsage>,
        /// When set, the reply streams its text and then fails with this
        /// retryable error instead of finishing.
        #[serde(default)]
        fail: Option<String>,
        /// When true, the reply is a thinking block cut off by a max_tokens
        /// stop, with no signature, text or tool call.
        #[serde(default)]
        max_tokens: bool,
    }

    /// Token counts a reply reports to exercise the usage line.
    #[derive(Debug, Deserialize)]
    struct ScriptedUsage {
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        reasoning: u64,
    }

    #[derive(Debug, Deserialize)]
    struct ScriptedCall {
        #[serde(default)]
        id: String,
        name: String,
        #[serde(default)]
        arguments: Value,
    }

    /// The top-level script document.
    #[derive(Debug, Deserialize)]
    struct Script {
        replies: Vec<ScriptedReply>,
    }

    impl Script {
        /// Plays the scripted replies through a FakeProvider.
        fn into_provider(self) -> FakeProvider {
            let provider = FakeProvider::default();
            for reply in self.replies {
                match reply.fail {
                    Some(error) => {
                        let mut partial = Vec::new();
                        if !reply.text.is_empty() {
                            partial.push(StreamEvent::TextDelta(Text::from(reply.text)));
                        }
                        provider.push_failing_reply(partial, Error::Transient(error));
                    }
                    None => provider.push_reply(events(reply)),
                }
            }
            provider
        }
    }

    /// Builds the stream events for one scripted reply.
    fn events(reply: ScriptedReply) -> Vec<StreamEvent> {
        if reply.max_tokens {
            return vec![
                StreamEvent::ThinkingDelta(Text::from("")),
                StreamEvent::Stop(StopReason::MaxTokens),
            ];
        }
        let mut events = Vec::with_capacity(reply.deltas.len() + reply.calls.len() * 3 + 3);
        if !reply.text.is_empty() {
            events.push(StreamEvent::TextDelta(Text::from(reply.text)));
        }
        events.extend(
            reply
                .deltas
                .into_iter()
                .map(|delta| StreamEvent::TextDelta(Text::from(delta))),
        );
        let mut used = false;
        for (index, call) in reply.calls.into_iter().enumerate() {
            let id = if call.id.is_empty() {
                format!("call_{index}")
            } else {
                call.id
            };
            events.push(StreamEvent::ToolCallStart {
                id: Text::from(id.clone()),
                name: Text::from(call.name),
            });
            let arguments = serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".into());
            events.push(StreamEvent::ToolCallDelta {
                id: Text::from(id.clone()),
                partial_json: Text::from(arguments),
            });
            events.push(StreamEvent::ToolCallEnd { id: Text::from(id) });
            used = true;
        }
        if let Some(usage) = reply.usage {
            events.push(StreamEvent::Usage(reagent_ai::Usage {
                input_tokens: usage.input,
                output_tokens: usage.output,
                cache_read_tokens: usage.cache_read,
                cache_write_tokens: usage.cache_write,
                reasoning_tokens: usage.reasoning,
                cost: None,
            }));
        }
        events.push(StreamEvent::Stop(if used {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        }));
        events
    }

    /// Parses a scripted fake provider from JSON.
    pub(super) fn from_script(text: &str) -> Result<FakeProvider, String> {
        let script: Script =
            serde_json::from_str(text).map_err(|error| format!("invalid fake script: {error}"))?;
        Ok(script.into_provider())
    }
}

/// Refuses a rule that names no tool reagent has: a typo would make a deny
/// silently do nothing. A tool that the run's list leaves out is still a tool.
fn check_rule_tools(rules: &Rules, registry: &Registry) -> Result<(), String> {
    for (name, rule, file) in rules.tool_names() {
        // A server's tool is a tool the run offers in a script, not directly.
        let known = TOOL_NAMES
            .iter()
            .any(|tool| tool.eq_ignore_ascii_case(name))
            || name.starts_with(reagent_mcp::EXPOSED_PREFIX);
        if !known && registry.resolve(name).is_none() {
            let tools = registry.names().collect::<Vec<_>>().join(", ");
            return Err(format!(
                "{file}: rule {rule} names no tool; the tools are: {tools}, plus Read, Edit and *"
            ));
        }
    }
    Ok(())
}

/// The helper command that fences bash in, or none when the sandbox is off or
/// this program cannot name itself.
fn launcher(args: &Args, cwd: &std::path::Path) -> Result<Option<Launcher>, String> {
    let fallback = match args.sandbox {
        SandboxMode::Off => return Ok(None),
        SandboxMode::Auto => Fallback::Auto,
        SandboxMode::On => Fallback::Required,
    };
    // The helper is this binary; without its path the sandbox cannot start.
    let program = match std::env::current_exe() {
        Ok(program) => program,
        Err(error) if fallback == Fallback::Auto => {
            eprintln!(
                "reagent: the sandbox is not available here ({error}); bash commands run without it"
            );
            return Ok(None);
        }
        Err(error) => return Err(format!("the sandbox could not start ({error})")),
    };
    let mut helper = vec!["--sandbox-exec".to_string()];
    // Resolved once here instead of on every bash call: the shell the helper
    // runs, and the write folders that exist, canonical. A folder that is not
    // there yet keeps the old `--write` form.
    if let Some(shell) = bash_path() {
        helper.push("--shell".into());
        helper.push(shell.to_string_lossy().into_owned());
    }
    let folders = [cwd.to_path_buf(), std::env::temp_dir()];
    for dir in folders.iter().chain(&args.write) {
        match canonical(dir).filter(|dir| dir.is_dir()) {
            Some(dir) => {
                helper.push("--write-resolved".into());
                helper.push(dir.to_string_lossy().into_owned());
            }
            None => {
                helper.push("--write".into());
                helper.push(dir.to_string_lossy().into_owned());
            }
        }
    }
    if args.network {
        helper.push("--network".into());
    }
    Ok(Some(Launcher {
        program,
        args: helper,
        fallback,
    }))
}

/// The `bash` on `PATH`, resolved once per run so the helper runs it without
/// a search of its own.
fn bash_path() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("bash"))
        .find(|candidate| executable(candidate))
        .and_then(|candidate| canonical(&candidate))
}

/// Whether `path` is a file someone may run, as a PATH search requires.
#[expect(
    clippy::disallowed_methods,
    reason = "once per run, before the tools start"
)]
fn executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// `path` as the file system resolves it, or none when it is not there.
#[expect(
    clippy::disallowed_methods,
    reason = "once per run, before the tools start"
)]
fn canonical(path: &std::path::Path) -> Option<std::path::PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// The environment probe, when the policy list asks for one. A run without the
/// `snapshot` policy starts nothing: no git command runs and no task exists.
fn probe_for(policy: &[String], cwd: &std::path::Path) -> Option<crate::snapshot::Probe> {
    policy
        .iter()
        .any(|name| name == "snapshot")
        .then(|| crate::snapshot::start(cwd))
}

/// The longest one `codemode` script may run unless the `[codemode]` table
/// says otherwise, and that unless `--deadline` is shorter.
const CODEMODE_WALL: Duration = Duration::from_secs(10 * 60);

/// What the `codemode` tool needs: the rules its script's calls are checked
/// against, and the longest one script may run.
struct CodemodeSetup {
    rules: Rules,
    wall: Duration,
    /// The server set a script's late calls resolve through, when a run has one.
    servers: Option<reagent_mcp::Servers>,
    /// The exposed names the request offers directly, taken once so the
    /// script and the request agree on them.
    direct: Vec<String>,
}

/// The wall time a script gets: the table's cap, or the deadline when that is
/// shorter. The deadline timer cancels the run's token too, which stops a
/// running script.
pub(crate) fn codemode_wall(wall: Option<u64>, deadline: Option<u64>) -> Duration {
    let wall = wall.map_or(CODEMODE_WALL, Duration::from_secs);
    deadline.map_or(wall, |seconds| Duration::from_secs(seconds).min(wall))
}

/// Adds the direct tools `selected` names to `registry`.
fn direct_tools(
    registry: &mut Registry,
    cwd: &std::path::Path,
    skip: &[String],
    touched: reagent_tools::repair::Touched,
    launcher: Option<Launcher>,
    bash_reads: bool,
    selected: &[String],
) {
    if chosen(selected, "read") {
        registry.add(Read::new(cwd).with_touched(touched.clone()));
    }
    if chosen(selected, "write") {
        registry.add(Write::new(cwd).with_touched(touched.clone()));
    }
    if chosen(selected, "edit") {
        registry.add(Edit::new(cwd).with_touched(touched));
    }
    if chosen(selected, "bash") {
        let bash = Bash::new(cwd).with_reads(bash_reads);
        registry.add(match launcher {
            Some(launcher) => bash.with_launcher(launcher),
            None => bash,
        });
    }
    if chosen(selected, "grep") {
        registry.add(Grep::new(cwd).with_skip(skip.iter().cloned()));
    }
    if chosen(selected, "find") {
        registry.add(Find::new(cwd).with_skip(skip.iter().cloned()));
    }
}

/// Whether a run offers the question tool: its list names `ask` and a person
/// can answer it, which only a worker or a run that takes its requests on
/// stdin or a socket can.
pub(crate) fn asks(selected: &[String], answering: bool) -> bool {
    answering && chosen(selected, reagent_agent::question::NAME)
}

/// Whether a resolved tool list holds `name`.
fn chosen(selected: &[String], name: &str) -> bool {
    selected.iter().any(|chosen| chosen == name)
}

/// How long a run waits for its MCP servers before it starts without them.
const STARTUP_WAIT: Duration = Duration::from_secs(10);

/// How many direct MCP tools a run adds before it warns once about the tokens.
const MANY_DIRECT: usize = 75;

/// The line a run prints when it adds `count` direct MCP tools, if it warns.
fn many_direct_warning(count: usize, warn: bool) -> Option<String> {
    (warn && count >= MANY_DIRECT).then(|| {
        format!(
            "reagent: {count} MCP tools are direct; each costs prompt tokens (set warn_many_direct = false in [settings] to hide this)"
        )
    })
}

/// The MCP tools the request offers directly, with a line for each name a
/// server does not have and one when there are many.
fn direct_adapters(
    args: &Args,
    servers: Option<&reagent_mcp::Servers>,
) -> Vec<reagent_mcp::McpTool> {
    let Some(servers) = servers else {
        return Vec::new();
    };
    let known = servers.warnings().len();
    let adapters = servers.direct_adapters();
    for warning in &servers.warnings()[known..] {
        eprintln!("reagent: {warning}");
    }
    if let Some(line) = many_direct_warning(adapters.len(), args.warn_many_direct) {
        eprintln!("{line}");
    }
    adapters
}

/// Starts the run's MCP servers, waits for them, and prints what each one did.
/// A signal during the wait ends it early.
async fn start_servers(args: &Args, cancel: &CancellationToken) -> Option<reagent_mcp::Servers> {
    if args.mcp_servers.is_empty() {
        return None;
    }
    let auth_dir = reagent_config::auth::dir().ok();
    let servers = reagent_mcp::Servers::start(&args.mcp_servers, auth_dir);
    tokio::select! {
        () = servers.wait_ready(STARTUP_WAIT) => {}
        () = cancel.cancelled() => {}
    }
    for state in servers.snapshot() {
        match &state.status {
            reagent_mcp::ServerStatus::Ready(tools) => {
                eprintln!("reagent: mcp {} ready ({} tools)", state.name, tools.len());
            }
            reagent_mcp::ServerStatus::Failed(reason) => {
                eprintln!("reagent: mcp {} failed: {reason}", state.name);
            }
            reagent_mcp::ServerStatus::Starting => {
                // A server still starting offers no direct tool this run.
                match args
                    .mcp_servers
                    .iter()
                    .find(|(name, _)| name == &state.name)
                    .map(|(_, server)| &server.direct)
                {
                    Some(direct) if direct != &McpDirect::None => eprintln!(
                        "reagent: mcp {} starting; its direct tools are left out of this run",
                        state.name
                    ),
                    _ => eprintln!("reagent: mcp {} starting", state.name),
                }
            }
        }
    }
    Some(servers)
}

/// Stops the servers a run started, when it started any.
async fn stop_servers(servers: Option<reagent_mcp::Servers>) {
    if let Some(servers) = servers {
        servers.stop().await;
    }
}

/// The run's servers, for a script call to a tool that is still starting.
struct McpSource {
    servers: reagent_mcp::Servers,
}

impl ToolSource for McpSource {
    fn resolve<'a>(
        &'a self,
        name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<Result<Box<dyn Tool>, String>>> + Send + 'a>> {
        Box::pin(async move {
            match self.servers.tool(name).await {
                Ok(tool) => Some(Ok(Box::new(tool) as Box<dyn Tool>)),
                Err(reason) => Some(Err(reason)),
            }
        })
    }

    fn names(&self) -> Vec<String> {
        self.servers.names()
    }

    fn generation(&self) -> u64 {
        self.servers.generation()
    }

    fn refresh(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(self.servers.refresh())
    }
}

/// The tools a headless run offers, all rooted at `cwd`, as `selected` names
/// them.
fn tools(
    cwd: &std::path::Path,
    skip: &[String],
    archive: Option<&Arc<Archive>>,
    launcher: Option<Launcher>,
    bash_reads: bool,
    selected: &[String],
    codemode: Option<CodemodeSetup>,
) -> Registry {
    let mut registry = Registry::new();
    // One list of touched files, so an edit can name the file a read found,
    // and a script's reads count as the run's own.
    let touched = reagent_tools::repair::Touched::default();
    // A script gets a registry of its own, so the outer one never offers a tool
    // a copy of itself.
    let script = codemode.map(|setup| {
        let mut inner = Registry::new();
        direct_tools(
            &mut inner,
            cwd,
            skip,
            touched.clone(),
            launcher.clone(),
            bash_reads,
            selected,
        );
        // A server's tools are in the script's registry, hidden from the model,
        // so its description names them only by signature. A direct tool is
        // named as a callable one, since the request offers it itself.
        let mut hidden = Vec::new();
        if let Some(servers) = &setup.servers {
            for adapter in servers.adapters() {
                if !setup.direct.contains(&adapter.spec().name) {
                    hidden.push(adapter.spec().name.clone());
                }
                inner.add(adapter);
            }
        }
        let mut script = Codemode::new(Arc::new(inner))
            .with_guard(Guard::new(setup.rules))
            .with_wall_time(setup.wall)
            .with_hidden(hidden);
        if let Some(servers) = &setup.servers {
            script = script.with_source(Arc::new(McpSource {
                servers: servers.clone(),
            }));
        }
        script
    });
    direct_tools(
        &mut registry,
        cwd,
        skip,
        touched,
        launcher,
        bash_reads,
        selected,
    );
    if let Some(archive) = archive {
        registry.add(Expand::new(Arc::clone(archive)));
    }
    if let Some(script) = script {
        registry.add(script);
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_policy_appends_the_block_to_the_first_message() {
        let block = "[environment]\nos: macos arm64, 12 cpu, 32G mem";
        assert_eq!(
            crate::snapshot::first_message("task".into(), block),
            format!("task\n\n{block}")
        );
    }

    #[test]
    fn snapshot_policy_off_starts_no_probe() {
        let cwd = std::path::Path::new("/");
        assert!(probe_for(&["budget".into()], cwd).is_none());
        assert!(probe_for(&[], cwd).is_none());
    }

    #[tokio::test]
    async fn snapshot_policy_on_starts_the_probe() {
        let cwd = std::path::Path::new("/");
        assert!(probe_for(&["budget".into(), "snapshot".into()], cwd).is_some());
    }

    #[test]
    fn snapshot_policy_off_leaves_the_task_alone() {
        assert_eq!(crate::snapshot::first_message("task".into(), ""), "task");
    }

    /// The deepseek preset, whose price peaks in the working day.
    fn deepseek() -> &'static reagent_ai::providers::Preset {
        reagent_ai::providers::get("deepseek").expect("the deepseek preset")
    }

    /// A usage of 1000 input tokens and no output tokens.
    fn usage_again() -> Usage {
        Usage {
            input_tokens: 1000,
            ..Usage::default()
        }
    }

    #[test]
    fn named_account_login_hint_replaces_only_a_missing_key() {
        let preset = reagent_ai::providers::get("chatgpt");
        let hint = login_hint(
            preset,
            Some(""),
            Some("codex-sub"),
            Some("codex-sub"),
            "missing ".into(),
        );
        assert_eq!(
            hint,
            "codex-sub needs a login; run reagent login chatgpt --as codex-sub"
        );
        let hint = login_hint(preset, Some(""), None, None, "missing ".into());
        assert_eq!(hint, "chatgpt needs a login; run reagent login chatgpt");
        let refused = "/s/auth.json: readable by others; run chmod 600 /s/auth.json".to_string();
        assert_eq!(
            login_hint(preset, Some(""), Some("chatgpt"), None, refused.clone()),
            refused
        );
    }

    #[test]
    fn named_account_a_table_login_reads_that_login() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut logins = reagent_config::auth::Logins::default();
        logins.set(
            "codex-sub",
            reagent_config::auth::Login::Key {
                key: Secret::from("sk-sub"),
            },
        );
        reagent_config::auth::write(dir.path(), &logins).expect("writes");
        assert_eq!(
            saved_login(Some(dir.path()), Some("codex-sub")),
            Ok(Some("sk-sub".to_string()))
        );
        assert_eq!(saved_login(Some(dir.path()), Some("codex-api")), Ok(None));
    }

    /// The key alone, from a key and where it came from.
    fn key(value: Option<(String, Source)>) -> Option<String> {
        value.map(|(key, _)| key)
    }

    /// The key alone, from a result of a key and where it came from.
    fn from_key(value: Result<(String, Source), String>) -> Result<String, String> {
        value.map(|(key, _)| key)
    }

    #[test]
    fn the_env_api_key_beats_the_file_api_key() {
        let file = Secret::from("from-file");
        assert_eq!(
            key(resolve_key(
                Some("from-env".into()),
                "FI_KEY",
                Some(&file),
                None,
                None,
                None
            ))
            .as_deref(),
            Some("from-env")
        );
    }

    #[test]
    fn the_file_api_key_is_used_when_the_env_is_unset_or_empty() {
        let file = Secret::from("from-file");
        for var in [None, Some(String::new())] {
            assert_eq!(
                key(resolve_key(var, "FI_KEY", Some(&file), None, None, None)).as_deref(),
                Some("from-file")
            );
        }
    }

    #[test]
    fn no_api_key_anywhere_gives_none() {
        assert_eq!(resolve_key(None, "FI_KEY", None, None, None, None), None);
        assert_eq!(
            resolve_key(Some(String::new()), "FI_KEY", None, None, None, None),
            None
        );
    }

    #[test]
    fn login_precedence_is_env_then_config_then_saved_then_default() {
        let file = Secret::from("from-file");
        let default = || Some("from-default".to_string());
        assert_eq!(
            key(resolve_key(
                Some("from-env".into()),
                "FI_KEY",
                Some(&file),
                Some("from-saved"),
                Some("work"),
                default()
            ))
            .as_deref(),
            Some("from-env")
        );
        assert_eq!(
            key(resolve_key(
                Some(String::new()),
                "FI_KEY",
                Some(&file),
                Some("from-saved"),
                Some("work"),
                default()
            ))
            .as_deref(),
            Some("from-file")
        );
        assert_eq!(
            key(resolve_key(
                None,
                "FI_KEY",
                None,
                Some("from-saved"),
                Some("work"),
                default()
            ))
            .as_deref(),
            Some("from-saved")
        );
        assert_eq!(
            key(resolve_key(
                None,
                "FI_KEY",
                None,
                Some(""),
                Some("work"),
                default()
            ))
            .as_deref(),
            Some("from-default")
        );
        assert_eq!(
            key(resolve_key(None, "FI_KEY", None, None, None, default())).as_deref(),
            Some("from-default")
        );
    }

    #[test]
    fn start_line_the_key_sources_are_named_without_the_key() {
        let file = Secret::from("sk-secret-file");
        let source = |value: Option<(String, Source)>| value.map(|(_, source)| source);
        assert_eq!(
            source(resolve_key(
                Some("sk-secret-env".into()),
                "FI_KEY",
                None,
                None,
                None,
                None
            )),
            Some(Source::EnvVar("FI_KEY".into()))
        );
        assert_eq!(
            source(resolve_key(None, "FI_KEY", Some(&file), None, None, None)),
            Some(Source::ConfigKey)
        );
        assert_eq!(
            source(resolve_key(
                None,
                "FI_KEY",
                None,
                Some("sk-secret-login"),
                Some("codex-sub"),
                None
            )),
            Some(Source::Login("codex-sub".into()))
        );
        assert_eq!(
            source(resolve_key(
                None,
                "FI_KEY",
                None,
                None,
                None,
                Some("sk-secret-default".into())
            )),
            Some(Source::DefaultEnv("FI_KEY".into()))
        );
        // The texts the start line writes never hold the key either.
        for source in [
            Source::EnvVar("FI_KEY".into()),
            Source::ConfigKey,
            Source::Login("codex-sub".into()),
            Source::DefaultEnv("FI_KEY".into()),
        ] {
            let text = source.text();
            assert!(!text.contains("sk-secret"), "{text}");
        }
        assert_eq!(Source::EnvVar("FI_KEY".into()).text(), "env FI_KEY");
        assert_eq!(Source::ConfigKey.text(), "config key");
        assert_eq!(Source::Login("work".into()).text(), "login work");
        assert_eq!(
            Source::DefaultEnv("FI_KEY".into()).text(),
            "env FI_KEY (default)"
        );
    }

    #[test]
    fn device_login_is_a_saved_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut logins = reagent_config::auth::Logins::default();
        logins.set(
            "radius",
            reagent_config::auth::Login::Device {
                provider: "radius".into(),
                client_id: "pi-gateway".into(),
                token_url: "http://token".into(),
                access: Secret::from("access-1"),
                refresh: Secret::from("refresh-1"),
                expires_at: 1_800_000_000,
            },
        );
        reagent_config::auth::write(dir.path(), &logins).expect("writes");
        let saved = saved_session(
            None,
            None,
            Some(dir.path()),
            Some("radius"),
            "http://token",
            sleeper(),
        )
        .expect("reads the login");
        assert!(saved.is_some(), "a device login signs in");
        // A key the user named still comes first.
        let named = saved_session(
            Some("sk-key"),
            None,
            Some(dir.path()),
            Some("radius"),
            "http://token",
            sleeper(),
        )
        .expect("reads the login");
        assert!(named.is_none());
    }

    #[test]
    fn login_chatgpt_oauth_login_beats_the_default_env_var() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut logins = reagent_config::auth::Logins::default();
        logins.set(
            "chatgpt",
            reagent_config::auth::Login::Oauth {
                client_id: "oaiapp_1".into(),
                host_id: "urn:uuid:00000000-0000-4000-8000-000000000000".into(),
                access: Secret::from("access-1"),
                refresh: Secret::from("refresh-1"),
                id_token: Secret::from("id-1"),
                expires_at: 1_800_000_000,
                subject: "user-1".into(),
                email: None,
            },
        );
        reagent_config::auth::write(dir.path(), &logins).expect("writes");
        let session = |named: Option<&str>, file: Option<&Secret>| {
            saved_session(
                named,
                file,
                Some(dir.path()),
                Some("chatgpt"),
                "http://token",
                sleeper(),
            )
            .expect("reads the login")
        };
        // Nothing else signs in, so the saved login does, even though the
        // wire's default variable answers with a key of its own.
        let saved = session(None, None).expect("the saved login");
        assert!(format!("{saved:?}").contains("chatgpt"), "the session");
        // An empty named variable is no key either.
        assert!(session(Some(""), None).is_some());
        // A named variable and the config key both come first.
        assert!(session(Some("sk-named"), None).is_none());
        let file = Secret::from("sk-file");
        assert!(session(None, Some(&file)).is_none());
        // With no saved login there is no session to fall back to.
        let none = saved_session(
            None,
            None,
            Some(dir.path()),
            Some("openrouter"),
            "http://token",
            sleeper(),
        )
        .expect("reads the login");
        assert!(none.is_none(), "a key login is no session");
    }

    #[test]
    fn login_a_broken_auth_file_falls_back_to_the_default_variable() {
        let refused = || Err("auth.json: readable by others; run chmod 600 auth.json".to_string());
        assert_eq!(
            from_key(key_from(
                None,
                "X",
                None,
                refused(),
                None,
                Some("from-default".into())
            ))
            .as_deref(),
            Ok("from-default")
        );
        // An empty variable is no key either, so the auth file error comes back.
        let message = key_from(None, "X", None, refused(), None, Some(String::new())).unwrap_err();
        assert!(message.contains("chmod 600"), "{message}");
        let message = key_from(None, "X", None, refused(), None, None).unwrap_err();
        assert!(message.contains("auth.json"), "{message}");
        // A file that did read gives its login before the default variable.
        assert_eq!(
            from_key(key_from(
                None,
                "X",
                None,
                Ok(Some("from-saved".into())),
                Some("work"),
                Some("from-default".into())
            ))
            .as_deref(),
            Ok("from-saved")
        );
        assert_eq!(
            from_key(key_from(
                Some("from-env".into()),
                "X",
                None,
                Ok(None),
                None,
                Some("from-default".into())
            ))
            .as_deref(),
            Ok("from-env")
        );
    }

    #[test]
    fn login_the_saved_login_comes_from_the_auth_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut logins = reagent_config::auth::Logins::default();
        logins.set(
            "openrouter",
            reagent_config::auth::Login::Key {
                key: Secret::from("sk-or-1"),
            },
        );
        reagent_config::auth::write(dir.path(), &logins).expect("writes");
        assert_eq!(
            saved_login(Some(dir.path()), Some("openrouter"))
                .expect("reads")
                .as_deref(),
            Some("sk-or-1")
        );
        assert_eq!(
            saved_login(Some(dir.path()), Some("nope")).expect("reads"),
            None
        );
        assert_eq!(saved_login(Some(dir.path()), None).expect("reads"), None);
        assert_eq!(saved_login(None, Some("openrouter")).expect("reads"), None);
    }

    #[test]
    #[cfg(unix)]
    fn login_a_refused_auth_file_is_an_error_that_names_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut logins = reagent_config::auth::Logins::default();
        logins.set(
            "openrouter",
            reagent_config::auth::Login::Key {
                key: Secret::from("sk-or-1"),
            },
        );
        reagent_config::auth::write(dir.path(), &logins).expect("writes");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            reagent_config::auth::path(dir.path()),
            std::fs::Permissions::from_mode(0o644),
        )
        .expect("chmods");
        let error = saved_login(Some(dir.path()), Some("openrouter")).unwrap_err();
        assert!(error.contains("auth.json"), "{error}");
        assert!(!error.contains("sk-or-1"), "{error}");
    }

    #[test]
    fn tools_are_bash_edit_find_grep_read_and_write() {
        let registry = tools(
            std::path::Path::new("."),
            &[],
            None,
            None,
            true,
            &default_tools(),
            None,
        );
        let names: Vec<&str> = registry.names().collect();
        assert_eq!(names, ["bash", "edit", "find", "grep", "read", "write"]);
    }

    #[test]
    fn the_context_window_comes_from_the_models_cache() {
        let temp = tempfile::tempdir().expect("makes a temp folder");
        let dir = temp.path();
        let entry = |id: &str, window| reagent_ai::catalog::Entry {
            id: id.into(),
            context_window: window,
            max_output: None,
            price: None,
            price_source: None,
            reasoning: None,
            images: None,
        };
        let cache = reagent_ai::catalog::Cache {
            fetched_at: 1,
            models: vec![entry("m", Some(128_000)), entry("n", None)],
        };
        reagent_ai::catalog::write(dir, "fi", &cache).expect("writes the cache");
        let window =
            |name, model| cached_entry(dir, name, model).and_then(|entry| entry.context_window);
        let window_of = window(Some("fi"), "m");
        assert_eq!(window_of, Some(128_000));
        assert_eq!(compact_budget(window_of), 96_000);
        assert_eq!(window(Some("fi"), "n"), None);
        assert_eq!(window(Some("fi"), "x"), None);
        assert_eq!(window(Some("other"), "m"), None);
        assert_eq!(window(None, "m"), None);
    }

    #[test]
    fn usage_cost_from_cached_price() {
        use reagent_ai::catalog::{Cache, Entry, Price};

        let temp = tempfile::tempdir().expect("makes a temp folder");
        let dir = temp.path();
        let price = Price {
            input: 0.000_001,
            output: 0.000_002,
            cache_read: 0.0,
            cache_write: 0.0,
        };
        let cache = Cache {
            fetched_at: 1,
            models: vec![Entry {
                id: "m".into(),
                context_window: Some(128_000),
                max_output: None,
                price: Some(price),
                price_source: Some(reagent_ai::catalog::PriceSource::Provider),
                reasoning: None,
                images: None,
            }],
        };
        reagent_ai::catalog::write(dir, "fi", &cache).expect("writes the cache");
        let entry = cached_entry(dir, Some("fi"), "m").expect("the entry");
        assert_eq!(entry.context_window, Some(128_000));
        let price = entry.price.expect("the cached price");

        // 1000 input tokens at 0.000001 and 500 output tokens at 0.000002.
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 500,
            ..Usage::default()
        };
        let cost = price.cost(&usage);
        assert!((cost - 0.002).abs() < 1e-12, "{cost}");

        // A provider that prices its own reply wins over the cache.
        let own = Usage {
            input_tokens: 1000,
            output_tokens: 500,
            cost: Some(0.5),
            ..Usage::default()
        };
        let filled = priced(Event::Usage(own), Some(Billing { price, peak: None }));
        assert!(matches!(&filled, Event::Usage(usage) if usage.cost == Some(0.5)));

        let unpriced = Usage {
            input_tokens: 1000,
            output_tokens: 500,
            ..Usage::default()
        };
        let filled = priced(Event::Usage(unpriced), Some(Billing { price, peak: None }));
        let Event::Usage(usage) = filled else {
            panic!("a usage event");
        };
        let cost = usage.cost.expect("a filled cost");
        assert!((cost - 0.002).abs() < 1e-12, "{cost}");
    }

    #[test]
    fn price_rule_a_config_price_is_usd_per_million_in_the_run() {
        let config = reagent_config::ModelPrice {
            input: 1.0,
            output: 2.0,
            cached: 0.1,
            cache_write: None,
        };
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 1000,
            ..Usage::default()
        };
        let billed = billing(Some(config), None, None).expect("a config price bills");
        let filled = priced_at(Event::Usage(usage), Some(billed), 439_200);
        let Event::Usage(usage) = filled else {
            panic!("a usage event");
        };
        let cost = usage.cost.expect("a filled cost");
        assert!((cost - 0.003).abs() < 1e-12, "{cost}");
        assert!((billed.price.cache_read - 1e-7).abs() < 1e-15);
        assert_eq!(billed.price.cache_write, 0.0);
    }

    #[test]
    fn price_rule_a_config_price_wins_over_the_list_price() {
        let cached = reagent_ai::catalog::Entry {
            id: "m".into(),
            context_window: None,
            max_output: None,
            price: Some(reagent_ai::catalog::Price {
                input: 0.000_005,
                output: 0.000_005,
                cache_read: 0.0,
                cache_write: 0.0,
            }),
            price_source: Some(reagent_ai::catalog::PriceSource::ModelsDev),
            reasoning: None,
            images: None,
        };
        let config = reagent_config::ModelPrice {
            input: 1.0,
            output: 2.0,
            cached: 0.1,
            cache_write: None,
        };
        let billed = billing(Some(config), Some(deepseek()), Some(&cached)).expect("a price");
        assert!((billed.price.input - 1e-6).abs() < 1e-15, "{billed:?}");
        assert!(billed.peak.is_none(), "a config price never peaks");
        // Without a config price the list price bills, and its source decides
        // whether the peak rule applies.
        let billed = billing(None, Some(deepseek()), Some(&cached)).expect("a price");
        assert!((billed.price.input - 5e-6).abs() < 1e-15);
        assert_eq!(billed.peak, deepseek().peak);
        assert!(billing(None, Some(deepseek()), None).is_none());
    }

    #[test]
    fn price_rule_a_models_dev_price_doubles_at_peak_and_a_config_price_does_not() {
        let cached = reagent_ai::catalog::Entry {
            id: "m".into(),
            context_window: None,
            max_output: None,
            price: Some(reagent_ai::catalog::Price {
                input: 1e-6,
                output: 2e-6,
                cache_read: 0.0,
                cache_write: 0.0,
            }),
            price_source: Some(reagent_ai::catalog::PriceSource::ModelsDev),
            reasoning: None,
            images: None,
        };
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 0,
            ..Usage::default()
        };
        // 02:00 UTC on a Tuesday is a peak hour of the deepseek preset.
        let tuesday_two = 5 * 86_400 + 2 * 3_600;
        let billed = billing(None, Some(deepseek()), Some(&cached)).expect("a price");
        let doubled = priced_at(Event::Usage(usage), Some(billed), tuesday_two);
        let Event::Usage(usage) = doubled else {
            panic!("a usage event");
        };
        assert!(
            (usage.cost.expect("a cost") - 0.002).abs() < 1e-12,
            "{usage:?}"
        );
        // Outside the window the price stands as the catalogue listed it.
        let off = priced_at(
            Event::Usage(usage_again()),
            Some(billed),
            tuesday_two + 3 * 3_600,
        );
        let Event::Usage(usage) = off else {
            panic!("a usage event");
        };
        assert!(
            (usage.cost.expect("a cost") - 0.001).abs() < 1e-12,
            "{usage:?}"
        );
        // A config price is a price the user wrote, so it never doubles.
        let config = reagent_config::ModelPrice {
            input: 1.0,
            output: 2.0,
            cached: 0.1,
            cache_write: None,
        };
        let billed = billing(Some(config), Some(deepseek()), Some(&cached)).expect("a price");
        let filled = priced_at(Event::Usage(usage_again()), Some(billed), tuesday_two);
        let Event::Usage(usage) = filled else {
            panic!("a usage event");
        };
        assert!(
            (usage.cost.expect("a cost") - 0.001).abs() < 1e-12,
            "{usage:?}"
        );
        // A price the provider listed carries no peak rule either.
        let listed = reagent_ai::catalog::Entry {
            price_source: Some(reagent_ai::catalog::PriceSource::Provider),
            ..cached
        };
        let billed = billing(None, Some(deepseek()), Some(&listed)).expect("a price");
        assert!(billed.peak.is_none());
    }

    #[test]
    fn price_rule_the_deepseek_peak_covers_its_weekday_hours() {
        let peak = deepseek().peak.expect("the deepseek peak");
        assert_eq!(peak.factor_at(439_200), 2.0, "02:00 UTC Tuesday");
        assert_eq!(peak.factor_at(439_200 + 3 * 3_600), 1.0, "05:00 Tuesday");
        assert_eq!(peak.factor_at(439_200 + 4 * 86_400), 1.0, "02:00 Saturday");
    }

    #[test]
    fn usage_cost_sums_known_parts() {
        let mut total = Usage::default();
        sum(&mut total, &Usage::default());
        assert_eq!(total.cost, None);

        sum(
            &mut total,
            &Usage {
                output_tokens: 5,
                cost: Some(0.25),
                ..Usage::default()
            },
        );
        assert_eq!(total.cost, Some(0.25));

        sum(
            &mut total,
            &Usage {
                output_tokens: 3,
                ..Usage::default()
            },
        );
        assert_eq!(total.cost, Some(0.25), "an unknown part adds nothing");
        assert_eq!(total.output_tokens, 8);

        sum(
            &mut total,
            &Usage {
                cost: Some(0.125),
                ..Usage::default()
            },
        );
        assert_eq!(total.cost, Some(0.375));
    }

    #[test]
    fn end_line_known_window_and_cost() {
        assert_eq!(
            end_line(12, 84_000, Some(200_000), Some(0.03), 0.96),
            "12 turns, 84k/200k context, $0.0300, 96% cached"
        );
        assert_eq!(
            end_line(1, 0, Some(8_000), Some(0.0), 0.0),
            "1 turn, 0k/8k context, $0.0000, 0% cached"
        );
    }

    #[test]
    fn end_line_unknown_window_and_cost() {
        assert_eq!(
            end_line(3, 1_500, None, None, 0.5),
            "3 turns, 2k context, 50% cached"
        );
        assert_eq!(
            end_line(2, 128_000, Some(128_000), None, 1.0),
            "2 turns, 128k/128k context, 100% cached"
        );
    }

    #[test]
    fn compact_budget_fits_the_context_window() {
        assert_eq!(compact_budget(Some(128_000)), 96_000);
        assert_eq!(compact_budget(Some(1_000_000)), 150_000);
        assert_eq!(compact_budget(None), 150_000);
    }

    #[test]
    fn a_rule_for_a_tool_left_out_of_the_list_is_kept() {
        let layer = reagent_config::Layer {
            path: std::path::PathBuf::from("/c.toml"),
            settings: reagent_config::Settings {
                permissions: reagent_config::permissions::Lists {
                    deny: vec!["Bash(git push:*)".into()],
                    ..Default::default()
                },
                ..Default::default()
            },
        };
        let rules = Rules::from_layers(&[layer], std::path::Path::new("/w"), None).unwrap();
        let selected = vec!["grep".to_string(), "read".to_string()];
        let registry = tools(
            std::path::Path::new("."),
            &[],
            None,
            None,
            true,
            &selected,
            None,
        );
        assert!(check_rule_tools(&rules, &registry).is_ok());
    }

    #[test]
    fn a_rule_for_an_unknown_tool_is_refused() {
        let layer = reagent_config::Layer {
            path: std::path::PathBuf::from("/c.toml"),
            settings: reagent_config::Settings {
                permissions: reagent_config::permissions::Lists {
                    deny: vec!["Bsh(git push:*)".into()],
                    ..Default::default()
                },
                ..Default::default()
            },
        };
        let rules = Rules::from_layers(&[layer], std::path::Path::new("/w"), None).unwrap();
        let registry = tools(
            std::path::Path::new("."),
            &[],
            None,
            None,
            true,
            &default_tools(),
            None,
        );
        let error = check_rule_tools(&rules, &registry).unwrap_err();
        assert!(error.contains("Bsh(git push:*)"), "{error}");
        let ok = reagent_config::Layer {
            path: std::path::PathBuf::from("/c.toml"),
            settings: reagent_config::Settings {
                permissions: reagent_config::permissions::Lists {
                    deny: vec!["Bash(git push:*)".into(), "Edit(x)".into(), "*(y)".into()],
                    ..Default::default()
                },
                ..Default::default()
            },
        };
        let rules = Rules::from_layers(&[ok], std::path::Path::new("/w"), None).unwrap();
        assert!(check_rule_tools(&rules, &registry).is_ok());
    }

    #[test]
    fn compact_policy_offers_expand() {
        let archive = Archive::new();
        let registry = tools(
            std::path::Path::new("."),
            &[],
            Some(&archive),
            None,
            true,
            &default_tools(),
            None,
        );
        assert!(registry.names().any(|name| name == "expand"));
    }

    /// A writer that keeps the bytes it is given and counts the writes and the
    /// flushes, so a test can see how many blocking hops a batch costs.
    #[derive(Clone, Default)]
    struct Sink {
        bytes: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
        writes: std::rc::Rc<std::cell::Cell<usize>>,
        flushes: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl tokio::io::AsyncWrite for Sink {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.bytes.borrow_mut().extend_from_slice(buf);
            self.writes.set(self.writes.get() + 1);
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.flushes.set(self.flushes.get() + 1);
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Queues `count` text deltas of `text`, closes the channel and drains it.
    async fn drain_text(count: usize, text: &str, json: bool, sink: &Sink) {
        let (tx, rx) = mpsc::channel(count + 8);
        for _ in 0..count {
            tx.send(Event::TextDelta(Text::from(text)))
                .await
                .expect("the channel takes the event");
        }
        drop(tx);
        let report = Report {
            json,
            steered: false,
            session: None,
            socket: None,
            billing: None,
            context_window: None,
        };
        drain(rx, None, None, Out::with_writer(sink.clone()), report).await;
    }

    #[tokio::test]
    async fn perf_json_batch_keeps_every_line_and_cuts_the_flushes() {
        let text = "x".repeat(40);
        let sink = Sink::default();
        let (tx, rx) = mpsc::channel(1024);
        let mut expected = Vec::new();
        crate::json::write_start(&mut expected, None, None);
        for _ in 0..1000 {
            let event = Event::TextDelta(Text::from(text.as_str()));
            crate::json::write_event(&mut expected, &event);
            tx.send(event).await.expect("the channel takes the event");
        }
        let end = Event::RunEnd(TurnEndReason::EndTurn);
        crate::json::write_event(&mut expected, &end);
        tx.send(end).await.expect("the channel takes the event");
        drop(tx);
        crate::json::write_summary(
            &mut expected,
            &Usage::default(),
            0,
            &TurnEndReason::EndTurn,
            None,
            0,
            None,
        );

        let report = Report {
            json: true,
            steered: false,
            session: None,
            socket: None,
            billing: None,
            context_window: None,
        };
        let reason = drain(rx, None, None, Out::with_writer(sink.clone()), report).await;
        assert_eq!(reason, TurnEndReason::EndTurn);
        assert_eq!(
            sink.bytes.borrow().as_slice(),
            expected.as_slice(),
            "the lines changed"
        );
        let lines = sink
            .bytes
            .borrow()
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        assert_eq!(
            lines, 1003,
            "the start line, 1000 events and the run end and the summary"
        );
        let flushes = sink.flushes.get();
        assert_eq!(flushes, sink.writes.get(), "one flush per write");
        // One write and one flush per 8 KiB batch: ten for these 1003 lines,
        // against one each (1003) before the batch.
        assert!(flushes <= 16, "1000 events cost {flushes} flushes");
    }

    #[tokio::test]
    async fn perf_json_batch_answer_is_copied_only_in_plain_mode() {
        let text = "y".repeat(64);
        let sink = Sink::default();
        ANSWER_BYTES.with(|bytes| bytes.set(0));
        drain_text(1000, &text, true, &sink).await;
        assert_eq!(
            ANSWER_BYTES.with(|bytes| bytes.get()),
            0,
            "JSON mode copied the reply text"
        );

        // A plain run still prints the whole answer.
        let sink = Sink::default();
        drain_text(1000, &text, false, &sink).await;
        assert_eq!(ANSWER_BYTES.with(|bytes| bytes.get()), 1000 * text.len());
        let mut printed = text.repeat(1000);
        printed.push('\n');
        assert_eq!(sink.bytes.borrow().as_slice(), printed.as_bytes());
    }

    /// A cached reasoning plan: `levels` ascending, with the rest as given.
    fn plan(
        levels: &[Level],
        off: Option<bool>,
        mandatory: bool,
        default: Option<Level>,
    ) -> Reasoning {
        let mut reasoning = Reasoning::default();
        for level in levels {
            reasoning.levels.insert(*level);
        }
        Reasoning {
            off,
            mandatory,
            default,
            ..reasoning
        }
    }

    #[test]
    fn reasoning_map_exact_budget_and_unknown_words_pass_through() {
        let cached = plan(&[Level::Low, Level::Medium, Level::High], None, false, None);
        // A level the model takes is sent as asked.
        assert_eq!(
            map_reasoning("medium", Some(&cached)),
            Mapped {
                sent: Some("medium".into()),
                note: None
            }
        );
        // A token budget is the provider's own number.
        assert_eq!(
            map_reasoning("4096", Some(&cached)),
            Mapped {
                sent: Some("4096".into()),
                note: None
            }
        );
        // An unknown word may be the provider's own.
        assert_eq!(
            map_reasoning("median", Some(&cached)),
            Mapped {
                sent: Some("median".into()),
                note: None
            }
        );
        // No data about the model changes nothing.
        assert_eq!(
            map_reasoning("medium", None),
            Mapped {
                sent: Some("medium".into()),
                note: None
            }
        );
        // Nothing asked, nothing sent.
        assert_eq!(
            map_reasoning("  ", Some(&cached)),
            Mapped {
                sent: None,
                note: None
            }
        );
    }

    #[test]
    fn reasoning_map_a_level_the_model_lacks_moves_up_then_down() {
        let cached = plan(&[Level::Low, Level::Medium, Level::High], None, false, None);
        // xhigh is above the top, so it falls to the next lower level.
        let mapped = map_reasoning("xhigh", Some(&cached));
        assert_eq!(mapped.sent.as_deref(), Some("high"));
        assert_eq!(mapped.note.as_deref(), Some("reasoning xhigh -> high"));
        // minimal is below the bottom, so it rises to the lowest level.
        let mapped = map_reasoning("minimal", Some(&cached));
        assert_eq!(mapped.sent.as_deref(), Some("low"));
        assert_eq!(mapped.note.as_deref(), Some("reasoning minimal -> low"));
        // Between two levels the next higher one wins over the next lower.
        let gaps = plan(&[Level::Low, Level::Medium], None, false, None);
        let mapped = map_reasoning("high", Some(&gaps));
        assert_eq!(mapped.sent.as_deref(), Some("medium"));
        assert_eq!(mapped.note.as_deref(), Some("reasoning high -> medium"));
        // A plan with no level at all leaves the text alone.
        let off_only = plan(&[], Some(true), false, None);
        assert_eq!(
            map_reasoning("high", Some(&off_only)),
            Mapped {
                sent: Some("high".into()),
                note: None
            }
        );
    }

    #[test]
    fn reasoning_map_off_turns_into_a_level_when_thinking_is_mandatory() {
        // The model's own default level, when it names one.
        let cached = plan(
            &[Level::Low, Level::Medium],
            None,
            true,
            Some(Level::Medium),
        );
        let mapped = map_reasoning("off", Some(&cached));
        assert_eq!(mapped.sent.as_deref(), Some("medium"));
        assert_eq!(
            mapped.note.as_deref(),
            Some("reasoning off -> medium (thinking is mandatory)")
        );
        // Else the lowest level it takes, whatever word asked for off.
        let lowest = plan(&[Level::High, Level::Max], None, true, None);
        let mapped = map_reasoning("none", Some(&lowest));
        assert_eq!(mapped.sent.as_deref(), Some("high"));
        assert_eq!(
            mapped.note.as_deref(),
            Some("reasoning off -> high (thinking is mandatory)")
        );
        // A list that says thinking can never be disabled counts as mandatory.
        let never = plan(&[Level::Low, Level::High], Some(false), false, None);
        let mapped = map_reasoning("off", Some(&never));
        assert_eq!(mapped.sent.as_deref(), Some("low"));
        assert_eq!(
            mapped.note.as_deref(),
            Some("reasoning off -> low (thinking is mandatory)")
        );
    }

    #[test]
    fn reasoning_map_a_zero_budget_is_off() {
        // Every wire reads a budget of 0 as off, so a mandatory thinker maps it
        // like `off`; any other budget passes through.
        let mandatory = plan(&[Level::Low, Level::High], None, true, Some(Level::High));
        let mapped = map_reasoning("0", Some(&mandatory));
        assert_eq!(mapped.sent.as_deref(), Some("high"));
        assert_eq!(
            mapped.note.as_deref(),
            Some("reasoning off -> high (thinking is mandatory)")
        );
        assert_eq!(
            map_reasoning("2048", Some(&mandatory)),
            Mapped {
                sent: Some("2048".into()),
                note: None
            }
        );
    }

    #[test]
    fn reasoning_map_off_stays_when_thinking_may_be_disabled() {
        let may = plan(&[Level::Low, Level::High], Some(true), false, None);
        assert_eq!(
            map_reasoning("off", Some(&may)),
            Mapped {
                sent: Some("off".into()),
                note: None
            }
        );
        // A source that does not say whether thinking may be disabled leaves
        // the off text to the provider.
        let unknown = plan(&[Level::Low, Level::High], None, false, None);
        assert_eq!(
            map_reasoning("none", Some(&unknown)),
            Mapped {
                sent: Some("none".into()),
                note: None
            }
        );
    }

    #[test]
    fn reasoning_map_the_start_line_names_the_level_it_picked() {
        let mapped = map_reasoning(
            "medium",
            Some(&plan(&[Level::Low, Level::High], None, false, None)),
        );
        assert_eq!(
            start_line(Some("work"), Some("fi"), "m", None, mapped.note.as_deref()),
            "reagent: profile work, model fi/m, credential none, reasoning medium -> high"
        );
        // A run with nothing to say keeps the old line exactly.
        assert_eq!(
            start_line(Some("work"), Some("fi"), "m", None, None),
            "reagent: profile work, model fi/m, credential none"
        );
    }

    fn with_unknown(mut reasoning: Reasoning, words: &[&str]) -> Reasoning {
        reasoning.unknown = words.iter().map(|word| word.to_string()).collect();
        reasoning
    }

    #[test]
    fn reasoning_map_unknown_word_the_model_lists_is_sent_as_written() {
        let model = with_unknown(
            plan(&[Level::Low, Level::High], None, false, None),
            &["ultra"],
        );
        let mapped = map_reasoning("Ultra", Some(&model));
        assert_eq!(mapped.sent.as_deref(), Some("Ultra"));
        assert_eq!(mapped.note, None);
    }

    #[test]
    fn reasoning_map_unknown_words_are_named_beside_another_level() {
        let model = with_unknown(
            plan(&[Level::Low, Level::High], None, false, None),
            &["ultra"],
        );
        let mapped = map_reasoning("medium", Some(&model));
        assert_eq!(mapped.sent.as_deref(), Some("high"));
        assert_eq!(
            start_line(None, Some("fi"), "m", None, mapped.note.as_deref()),
            "reagent: profile none, model fi/m, credential none, reasoning medium -> high, unknown levels ultra"
        );
        let exact = map_reasoning("low", Some(&model));
        assert_eq!(exact.sent.as_deref(), Some("low"));
        assert_eq!(exact.note.as_deref(), Some("unknown levels ultra"));
        // A budget is not a level, so the words are not named.
        assert_eq!(map_reasoning("2048", Some(&model)).note, None);
    }

    #[test]
    fn mcp_direct_many_warning() {
        assert_eq!(many_direct_warning(MANY_DIRECT - 1, true), None);
        assert_eq!(many_direct_warning(MANY_DIRECT, false), None);
        assert_eq!(many_direct_warning(80, false), None);
        assert_eq!(
            many_direct_warning(80, true).as_deref(),
            Some(
                "reagent: 80 MCP tools are direct; each costs prompt tokens (set warn_many_direct = false in [settings] to hide this)"
            )
        );
    }

    #[test]
    fn reasoning_map_unknown_words_alone_send_the_asked_level() {
        let model = with_unknown(Reasoning::default(), &["deep", "wide"]);
        let mapped = map_reasoning("high", Some(&model));
        assert_eq!(mapped.sent.as_deref(), Some("high"));
        assert_eq!(mapped.note.as_deref(), Some("unknown levels deep wide"));
    }

    #[test]
    fn bash_reads_off_keeps_every_command_in_bash() {
        let cwd = std::path::Path::new(".");
        let args = r#"{"command":"cat Cargo.toml"}"#;
        let on = tools(cwd, &[], None, None, true, &default_tools(), None);
        assert!(
            on.resolve("bash")
                .and_then(|bash| bash.reroute(args))
                .is_some()
        );
        let off = tools(cwd, &[], None, None, false, &default_tools(), None);
        assert!(
            off.resolve("bash")
                .and_then(|bash| bash.reroute(args))
                .is_none()
        );
    }

    /// The tools a run's list names, sorted as a resolved list is.
    fn named(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    fn default_tools() -> Vec<String> {
        named(&["bash", "edit", "find", "grep", "read", "write"])
    }

    /// The rules and wall time a run that lists `codemode` gives it.
    fn codemode_setup() -> CodemodeSetup {
        CodemodeSetup {
            rules: Rules::none(),
            wall: CODEMODE_WALL,
            servers: None,
            direct: Vec::new(),
        }
    }

    /// A registry of the direct tools, built the way `tools` built one before
    /// the `codemode` tool existed.
    fn hand_built(cwd: &std::path::Path) -> Registry {
        let mut registry = Registry::new();
        let touched = reagent_tools::repair::Touched::default();
        registry.add(Read::new(cwd).with_touched(touched.clone()));
        registry.add(Write::new(cwd).with_touched(touched.clone()));
        registry.add(Edit::new(cwd).with_touched(touched));
        registry.add(Bash::new(cwd).with_reads(true));
        registry.add(Grep::new(cwd).with_skip(std::iter::empty::<String>()));
        registry.add(Find::new(cwd).with_skip(std::iter::empty::<String>()));
        registry
    }

    #[test]
    fn codemode_tools_off_by_default() {
        let cwd = std::path::Path::new(".");
        let registry = tools(cwd, &[], None, None, true, &default_tools(), None);
        let names: Vec<&str> = registry.names().collect();
        assert_eq!(names, ["bash", "edit", "find", "grep", "read", "write"]);
        let before = hand_built(cwd);
        assert_eq!(
            serde_json::to_string(registry.specs()).unwrap(),
            serde_json::to_string(before.specs()).unwrap()
        );
    }

    #[tokio::test]
    async fn codemode_tools_on_runs_read() {
        let tmp = tempfile::tempdir().expect("temp dir");
        tokio::fs::write(tmp.path().join("a.txt"), "hello")
            .await
            .expect("write the file");
        let selected = named(&["codemode", "read"]);
        let registry = tools(
            tmp.path(),
            &[],
            None,
            None,
            true,
            &selected,
            Some(codemode_setup()),
        );
        assert!(
            registry
                .specs()
                .iter()
                .any(|spec| spec.name == reagent_script::codemode::NAME)
        );
        let tool = registry
            .get(reagent_script::codemode::NAME)
            .expect("the codemode tool");
        let args =
            serde_json::json!({ "code": "return await tools.read({path: \"a.txt\"})" }).to_string();
        let output = tool.run(&args, &CancellationToken::new()).await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("hello"), "{}", output.content);
    }

    /// A tool a server would expose, for the guard and the rule tests.
    struct McpLike {
        spec: reagent_ai::ToolSpec,
    }

    impl McpLike {
        fn new(name: &str) -> Self {
            Self {
                spec: reagent_ai::ToolSpec {
                    name: name.into(),
                    description: "A server tool.".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            }
        }
    }

    impl reagent_agent::tool::Tool for McpLike {
        fn spec(&self) -> &reagent_ai::ToolSpec {
            &self.spec
        }

        fn run<'a>(
            &'a self,
            _args: &'a str,
            _cancel: &CancellationToken,
        ) -> Pin<Box<dyn Future<Output = reagent_agent::tool::ToolOutput> + Send + 'a>> {
            Box::pin(async { reagent_agent::tool::ToolOutput::ok("deleted") })
        }
    }

    /// The rules of one deny list.
    fn deny_rule(rule: &str) -> Rules {
        let layer = reagent_config::Layer {
            path: std::path::PathBuf::from("/c.toml"),
            settings: reagent_config::Settings {
                permissions: reagent_config::permissions::Lists {
                    deny: vec![rule.into()],
                    ..Default::default()
                },
                ..Default::default()
            },
        };
        Rules::from_layers(&[layer], std::path::Path::new("/w"), None).expect("the rules")
    }

    #[tokio::test]
    async fn mcp_codemode_rule_names_mcp_tool() {
        // A rule may name a server tool, which the run's registry never holds.
        let rules = deny_rule("mcp__gh__delete_repo");
        let registry = tools(
            std::path::Path::new("."),
            &[],
            None,
            None,
            true,
            &default_tools(),
            None,
        );
        assert!(check_rule_tools(&rules, &registry).is_ok());

        let mut inner = Registry::new();
        inner.add(McpLike::new("mcp__gh__delete_repo"));
        let codemode = Codemode::new(Arc::new(inner)).with_guard(Guard::new(rules));
        let args = serde_json::json!({
            "code": "await tools.mcp__gh__delete_repo({repo: \"x\"})"
        })
        .to_string();
        let output = codemode.run(&args, &CancellationToken::new()).await;
        assert!(output.is_error, "{}", output.content);
        assert!(
            output
                .content
                .contains("denied by rule mcp__gh__delete_repo"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn mcp_codemode_no_server_same_specs() {
        let cwd = std::path::Path::new(".");
        // A run with no `mcp.toml` picks no server, so its request is the one
        // that had no server support at all.
        let servers = reagent_mcp::Servers::start(&[], None);
        let plain = tools(cwd, &[], None, None, true, &default_tools(), None);
        let started = tools(cwd, &[], None, None, true, &default_tools(), None);
        assert_eq!(
            serde_json::to_string(plain.specs()).unwrap(),
            serde_json::to_string(started.specs()).unwrap()
        );
        let selected = named(&["codemode", "read"]);
        let before = tools(
            cwd,
            &[],
            None,
            None,
            true,
            &selected,
            Some(codemode_setup()),
        );
        let after = tools(
            cwd,
            &[],
            None,
            None,
            true,
            &selected,
            Some(CodemodeSetup {
                servers: Some(servers),
                ..codemode_setup()
            }),
        );
        let description = |registry: &Registry| {
            registry
                .get(reagent_script::codemode::NAME)
                .expect("the codemode tool")
                .spec()
                .description
                .clone()
        };
        assert_eq!(description(&before), description(&after));
        assert!(
            !description(&after).contains("mcp__"),
            "no server tool reaches the description"
        );
    }

    #[tokio::test]
    async fn codemode_tools_follow_the_list() {
        // The list `+codemode,-write` leaves `write` out of both the outer
        // registry and the one a script's calls land in.
        let tmp = tempfile::tempdir().expect("temp dir");
        let selected = named(&["bash", "codemode", "edit", "find", "grep", "read"]);
        let registry = tools(
            tmp.path(),
            &[],
            None,
            None,
            true,
            &selected,
            Some(codemode_setup()),
        );
        assert!(registry.get("write").is_none());
        let tool = registry
            .get(reagent_script::codemode::NAME)
            .expect("the codemode tool");
        let args = serde_json::json!({
            "code": "return await tools.write({path: \"a.txt\", content: \"x\"})"
        })
        .to_string();
        let output = tool.run(&args, &CancellationToken::new()).await;
        assert!(output.is_error, "{}", output.content);
        assert!(
            output.content.contains("unknown tool"),
            "{}",
            output.content
        );
    }
}
