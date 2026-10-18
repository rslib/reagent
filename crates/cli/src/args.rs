//! Command line argument parsing with `lexopt`.

use std::path::PathBuf;

/// The default model for headless runs.
pub const DEFAULT_MODEL: &str = "claude-sonnet-5-5";

/// The parsed command line.
pub struct Args {
    /// The task the agent should complete.
    pub task: String,
    /// Print one JSON object per event.
    pub json: bool,
    /// The provider model id.
    pub model: String,
    /// A hard deadline in seconds, if any.
    pub deadline: Option<u64>,
    /// The working directory the tools run in.
    pub cwd: PathBuf,
    /// The `--provider` value, `fake:<file>`, testing builds only.
    #[cfg(feature = "testing")]
    pub provider: Option<String>,
}

/// Either the parsed arguments or an immediate exit decision.
pub enum Outcome {
    /// Run the agent with these arguments.
    Run(Args),
    /// Print the help text and exit cleanly.
    Help,
    /// Print the version and exit cleanly.
    Version,
}

fn usage() -> &'static str {
    concat!(
        "usage: reagent [OPTIONS] -p <task>\n",
        "\n",
        "Runs the agent headless (no terminal UI) and prints the result.\n",
        "\n",
        "Options:\n",
        "  -p, --print <task>   the task for the agent to complete\n",
        "      --json           print one JSON object per event on stdout\n",
        "      --model <id>     the provider model id\n",
        "      --deadline <s>   stop after this many seconds\n",
        "      --cwd <dir>      the working directory for the tools (default: current dir)\n",
        "      --version        print the version and exit\n"
    )
}

#[cfg(feature = "testing")]
fn extra_usage() -> &'static str {
    "      --provider <spec>  fake:<file>, a JSON script of replies (testing only)\n"
}

#[cfg(not(feature = "testing"))]
fn extra_usage() -> &'static str {
    ""
}

fn bad(field: &str) -> String {
    format!("invalid value for {field}")
}

/// Parses the process arguments, or returns an exit decision.
pub fn parse() -> Result<Outcome, String> {
    let mut parser = lexopt::Parser::from_env();

    let mut task: Option<String> = None;
    let mut json = false;
    let mut model = None;
    let mut deadline = None;
    let mut cwd = None;
    #[cfg(feature = "testing")]
    let mut provider = None;

    while let Some(arg) = parser.next().map_err(|error| error.to_string())? {
        match arg {
            lexopt::Arg::Short('p') | lexopt::Arg::Long("print") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --print".to_string())?;
                task = Some(
                    value
                        .into_string()
                        .map_err(|_| "task must be UTF-8".to_string())?,
                );
            }
            lexopt::Arg::Long("json") => json = true,
            lexopt::Arg::Long("model") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --model".to_string())?;
                model = Some(value.into_string().map_err(|_| bad("--model"))?);
            }
            lexopt::Arg::Long("deadline") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --deadline".to_string())?;
                let text = value.into_string().map_err(|_| bad("--deadline"))?;
                deadline = Some(text.parse().map_err(|_| bad("--deadline"))?);
            }
            lexopt::Arg::Long("cwd") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --cwd".to_string())?;
                cwd = Some(PathBuf::from(value));
            }
            #[cfg(feature = "testing")]
            lexopt::Arg::Long("provider") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --provider".to_string())?;
                provider = Some(value.into_string().map_err(|_| bad("--provider"))?);
            }
            lexopt::Arg::Long("help") => return Ok(Outcome::Help),
            lexopt::Arg::Short('h') => return Ok(Outcome::Help),
            lexopt::Arg::Long("version") => return Ok(Outcome::Version),
            _ => return Err(format!("unknown argument: {arg:?}")),
        }
    }

    let Some(task) = task else {
        if json {
            return Err("the --json flag needs a task; use -p <task>".into());
        }
        return Err("missing task; use -p <task>".into());
    };

    Ok(Outcome::Run(Args {
        task,
        json,
        model: model.unwrap_or_else(|| DEFAULT_MODEL.into()),
        deadline,
        cwd: cwd.unwrap_or_else(|| PathBuf::from(".")),
        #[cfg(feature = "testing")]
        provider,
    }))
}

/// The help text, with testing-only flags when built for tests.
pub fn help() -> String {
    let mut help = usage().to_string();
    help.push_str(extra_usage());
    help
}
