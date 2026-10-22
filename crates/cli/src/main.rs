//! The reagent binary: a headless coding agent.

mod args;
mod json;
mod login;
mod models;
mod prompt;
mod run;
mod sandbox;
mod snapshot;

/// The estimated-token budget for the system prompt and tool specs.
#[cfg(test)]
const PROMPT_BUDGET: usize = 1000;

fn main() -> std::process::ExitCode {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if argv.first().is_some_and(|arg| *arg == "--sandbox-exec") {
        return sandbox::helper(argv.into_iter().skip(1));
    }
    if let Some(command) = argv.first().and_then(|arg| arg.to_str())
        && matches!(command, "login" | "logout")
    {
        let runtime = runtime();
        let code = runtime.block_on(login::run(command, argv[1..].to_vec()));
        // The pasted-URL reader holds a blocking stdin thread, and dropping a
        // runtime waits for it, so `login` would hang until Enter after a
        // browser login. Shut the runtime down without waiting instead.
        runtime.shutdown_background();
        return code;
    }
    let outcome = match args::parse() {
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

    runtime().block_on(run::run(args))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("builds the tokio runtime")
}

/// The serialized tool specs plus the filled system prompt, in bytes.
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
}
