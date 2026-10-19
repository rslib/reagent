//! The reagent binary: a headless coding agent.

mod args;
mod json;
mod prompt;
mod run;
mod snapshot;

/// The estimated-token budget for the system prompt and tool specs.
#[cfg(test)]
const PROMPT_BUDGET: usize = 1000;

fn main() -> std::process::ExitCode {
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
        args::Outcome::Run(args) => *args,
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("builds the tokio runtime");
    runtime.block_on(run::run(args))
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
    ];
    let serialized = serde_json::to_string(&specs).expect("tool specs serialize");
    let prompt = prompt::system("examples/");
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
