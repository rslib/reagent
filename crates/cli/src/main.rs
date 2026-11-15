//! The reagent binary: a headless coding agent.

mod answer;
mod args;
mod attach;
#[cfg(feature = "tui")]
mod gateway;
mod gc;
mod json;
mod login;
mod mcp;
mod models;
mod net_proxy;
mod permissions;
mod plugin;
mod prompt;
mod rpc;
mod run;
mod sandbox;
mod session;
mod skills;
mod snapshot;
#[cfg(feature = "tui")]
mod tui;
mod tui_host;
mod worker;

/// The estimated-token budget for the system prompt and tool specs: 1000,
/// raised to 1024 for the edit tool's `line` argument and to 1040 for the
/// `read` sentence on images.
#[cfg(test)]
const PROMPT_BUDGET: usize = 1040;

/// Runs the gateway, which needs the terminal UI's worker and file watcher.
#[cfg(feature = "tui")]
fn gateway(
    argv: Vec<std::ffi::OsString>,
    runtime: &tokio::runtime::Runtime,
) -> std::process::ExitCode {
    runtime.block_on(gateway::run(argv))
}

/// The gateway is part of the default build; without it the command says so.
#[cfg(not(feature = "tui"))]
fn gateway(
    _argv: Vec<std::ffi::OsString>,
    _runtime: &tokio::runtime::Runtime,
) -> std::process::ExitCode {
    eprintln!("reagent: gateway needs the default build");
    std::process::ExitCode::from(2)
}

fn main() -> std::process::ExitCode {
    let mut argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if argv.first().is_some_and(|arg| *arg == "--sandbox-exec") {
        return sandbox::helper(argv.into_iter().skip(1));
    }
    if let Some(command) = argv.first().and_then(|arg| arg.to_str())
        && matches!(
            command,
            "answer"
                | "gateway"
                | "login"
                | "logout"
                | "mcp"
                | "permissions"
                | "plugin"
                | "rpc"
                | "session"
        )
    {
        let runtime = runtime();
        let code = match command {
            "answer" => runtime.block_on(answer::run(argv[1..].to_vec())),
            "gateway" => gateway(argv[1..].to_vec(), &runtime),
            "mcp" => runtime.block_on(mcp::run(argv[1..].to_vec())),
            "permissions" => runtime.block_on(permissions::run(argv[1..].to_vec())),
            "plugin" => runtime.block_on(plugin::run(argv[1..].to_vec())),
            "rpc" => runtime.block_on(rpc::run(argv[1..].to_vec())),
            "session" => runtime.block_on(session::command(argv[1..].to_vec())),
            _ => runtime.block_on(login::run(command, argv[1..].to_vec())),
        };
        // The pasted-URL reader holds a blocking stdin thread, and dropping a
        // runtime waits for it, so `login` would hang until Enter after a
        // browser login. Shut the runtime down without waiting instead.
        runtime.shutdown_background();
        return code;
    }
    // `worker` runs the same path as `-p`, with a flag that makes it serve the
    // session instead of running it once.
    let worker = args::worker_word(&mut argv);
    let outcome = match args::parse(argv, worker, cfg!(feature = "tui")) {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("reagent: {error}");
            return std::process::ExitCode::from(2);
        }
    };
    let args = match outcome {
        args::Outcome::Help => {
            print!("{}", args::help());
            return std::process::ExitCode::SUCCESS;
        }
        args::Outcome::Version => {
            println!("reagent {}", env!("CARGO_PKG_VERSION"));
            return std::process::ExitCode::SUCCESS;
        }
        args::Outcome::Models(models) => return runtime().block_on(models::run(*models)),
        args::Outcome::Run(args) => *args,
    };

    // With no task for a terminal, the run is the UI: a worker for the new or
    // the continued session, and the app attached to it. Every other shape,
    // `--json` and a run with no terminal included, keeps the plain path. A run
    // that takes its requests while it runs is headless, and its stdin and its
    // socket are not the editors of the UI.
    #[cfg(feature = "tui")]
    if tui::wanted(&args) && !args.commands && !args.listen {
        let code = runtime().block_on(tui::run(args));
        reagent_tools::spill::cleanup();
        return code;
    }
    let commands = args.commands;
    let runtime = runtime();
    let code = runtime.block_on(run::run(args));
    if commands {
        // The client of stdin holds a blocking read of it, and dropping the
        // runtime waits for that thread, so a run that reads its requests there
        // shuts down without waiting for the input to end.
        runtime.shutdown_background();
    } else {
        drop(runtime);
    }
    // Every way out of a run comes back here, Ctrl-C and the deadline too, so
    // the spill folder of this run goes at its end, once the runtime is gone.
    reagent_tools::spill::cleanup();
    code
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("builds the tokio runtime")
}

/// The serialized tool specs plus the filled system prompt, in bytes.
///
/// These are the specs of a plain `-p` run, which never offers `ask`.
#[cfg(test)]
fn budget_bytes() -> usize {
    // The free `spec()` builders give the exact specs a run offers the model.
    let specs = [
        reagent_tools::read::spec(),
        reagent_tools::edit::spec(),
        reagent_tools::write::spec(),
        reagent_tools::bash::spec(),
        reagent_tools::grep::spec(),
        reagent_tools::find::spec(),
        reagent_agent::archive::spec(),
    ];
    let serialized = serde_json::to_string(&specs).expect("tool specs serialize");
    let prompt = prompt::system_with("examples/", "");
    serialized.len() + prompt.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rough token estimate: one token is about four bytes, so the serialized
    /// tool specs must stay under the budget.
    #[test]
    fn prompt_budget() {
        let bytes = budget_bytes();
        let estimated = crate::prompt::estimate_tokens(bytes);
        assert!(
            estimated < PROMPT_BUDGET,
            "prompt and tool specs estimated at {estimated} tokens ({bytes} bytes), over {PROMPT_BUDGET}"
        );
    }

    /// The budget above covers the specs of a plain `-p` run, which offers no
    /// `ask`: only a run a person can answer through keeps it.
    #[test]
    fn ask_the_plain_run_offers_no_ask() {
        let defaults: Vec<String> = crate::args::DEFAULT_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        assert!(
            !crate::run::asks(&defaults, false),
            "a plain run offers no ask"
        );
        assert!(
            crate::run::asks(&defaults, true),
            "a steered run keeps the ask of the default list"
        );
        let plain = ["bash".to_string(), "read".to_string()];
        assert!(
            !crate::run::asks(&plain, true),
            "a plain list without ask has none"
        );
    }
}
