//! Command line argument parsing with `lexopt`.

use std::path::PathBuf;

/// The default model for headless runs.
pub const DEFAULT_MODEL: &str = "claude-sonnet-5-5";

/// The loop policy names a `--policy` value may select.
pub const POLICY_NAMES: [&str; 6] = [
    "completion",
    "stuck",
    "budget",
    "snapshot",
    "mask",
    "compact",
];

/// The policies `all` selects. `stuck` is left out until a benchmark run
/// shows it helps: a loop breaker cost 1.1 points in the Meta-Harness study.
const ALL_POLICIES: [&str; 3] = ["completion", "budget", "snapshot"];

/// The policies with no `--policy` flag. The budget note does nothing without
/// a deadline or a turn cap, and with one it cut timeouts on a slow model.
const DEFAULT_POLICIES: [&str; 1] = ["budget"];

/// The policies with no `--policy` flag on the chat wire. Masking cut prompt
/// tokens by 42% on a glm-5.3-flash dev run with no drop in pass rate.
const DEFAULT_CHAT_POLICIES: [&str; 2] = ["budget", "mask"];

/// The reply token cap when `--max-tokens` is absent. A `write` call carries
/// the whole file, and thinking counts toward the cap, so a low cap cuts the
/// reply.
const DEFAULT_MAX_TOKENS: u32 = 64_000;

/// The provider wire selected by `--provider`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wire {
    /// The Anthropic Messages API.
    Anthropic,
    /// OpenAI-compatible Chat Completions.
    OpenAiChat,
    /// The OpenAI Responses API.
    OpenAiResponses,
    /// A scripted provider read from `fake:<file>`, testing builds only.
    #[cfg(feature = "testing")]
    Fake(String),
}

impl Wire {
    /// The environment variable holding the API key when `--api-key-env` is
    /// absent.
    pub fn default_api_key_env(&self) -> &'static str {
        match self {
            Wire::Anthropic => "ANTHROPIC_API_KEY",
            Wire::OpenAiChat | Wire::OpenAiResponses => "OPENAI_API_KEY",
            #[cfg(feature = "testing")]
            Wire::Fake(_) => "ANTHROPIC_API_KEY",
        }
    }

    /// The endpoint when `--base-url` is absent: the provider environment
    /// variable, else the built-in default.
    pub fn base_url(&self) -> String {
        let (variable, fallback) = match self {
            Wire::Anthropic => ("ANTHROPIC_BASE_URL", "https://api.anthropic.com"),
            Wire::OpenAiChat | Wire::OpenAiResponses => {
                ("OPENAI_BASE_URL", "https://api.openai.com/v1")
            }
            #[cfg(feature = "testing")]
            Wire::Fake(_) => ("ANTHROPIC_BASE_URL", "https://api.anthropic.com"),
        };
        std::env::var(variable)
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback.to_string())
    }
}

/// The parsed command line.
#[derive(Debug)]
pub struct Args {
    /// The task the agent should complete, or where to read it from.
    pub task: TaskSource,
    /// Print one JSON object per event.
    pub json: bool,
    /// The provider model id.
    pub model: String,
    /// The provider wire to use.
    pub provider: Wire,
    /// The endpoint URL, or the provider default when absent.
    pub base_url: Option<String>,
    /// The name of the environment variable holding the API key, or the
    /// provider default when absent.
    pub api_key_env: Option<String>,
    /// Extra request headers from `--header`, in order.
    pub header: Vec<(String, String)>,
    /// The cap on reply tokens, thinking included.
    pub max_tokens: u32,
    /// A hard deadline in seconds, if any.
    pub deadline: Option<u64>,
    /// The working directory the tools run in.
    pub cwd: PathBuf,
    /// The loop policies to enable, expanded and de-duplicated.
    pub policy: Vec<String>,
}

/// Where the task text comes from.
#[derive(Debug)]
pub enum TaskSource {
    /// The task was given on the command line.
    Text(String),
    /// The task is read from standard input (`-p -`).
    Stdin,
}

/// Either the parsed arguments or an immediate exit decision.
#[derive(Debug)]
pub enum Outcome {
    /// Run the agent with these arguments.
    Run(Box<Args>),
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
        "  -p, --print <task>   the task for the agent to complete; `-` reads it from stdin\n",
        "      --json           print one JSON object per event on stdout\n",
        "      --model <id>     the provider model id\n",
        "      --provider <wire>  anthropic, openai-chat or openai-responses (default: anthropic)\n",
        "      --base-url <url>   the endpoint; default from the provider env, else built in\n",
        "      --api-key-env <name>  the env var holding the API key; never the key itself\n",
        "      --header <name:value>  an extra request header; repeatable; reserved names\n",
        "                       (authorization, x-api-key, content-type, anthropic-version) are errors\n",
        "      --max-tokens <n> the reply token cap, thinking included (default: 64000)\n",
        "      --deadline <s>   stop after this many seconds\n",
        "      --cwd <dir>      the working directory for the tools (default: current dir)\n",
        "      --policy <list>  loop policies: completion, stuck, budget, snapshot, mask, compact;\n",
        "                       all = completion, budget, snapshot; none = off\n",
        "                       (default: budget, plus mask on openai-chat)\n",
        "      --version        print the version and exit\n"
    )
}

#[cfg(feature = "testing")]
fn extra_usage() -> &'static str {
    "      --provider fake:<file>  testing only: play a JSON script of replies\n"
}

#[cfg(not(feature = "testing"))]
fn extra_usage() -> &'static str {
    ""
}

fn bad(field: &str) -> String {
    format!("invalid value for {field}")
}

/// Whether the wire replays no reasoning that depends on earlier messages,
/// so compaction may rewrite them. The test-only scripted wire replays none.
fn may_compact(provider: &Wire) -> bool {
    match provider {
        Wire::OpenAiChat => true,
        #[cfg(feature = "testing")]
        Wire::Fake(_) => true,
        _ => false,
    }
}

/// Maps a `--provider` value to a wire, or to `fake:<file>` under testing.
fn parse_provider(value: &str) -> Result<Wire, String> {
    match value {
        "anthropic" => Ok(Wire::Anthropic),
        "openai-chat" => Ok(Wire::OpenAiChat),
        "openai-responses" => Ok(Wire::OpenAiResponses),
        other => {
            #[cfg(feature = "testing")]
            if let Some(spec) = other.strip_prefix("fake:") {
                return Ok(Wire::Fake(spec.to_string()));
            }
            Err(format!("unknown provider: {other}"))
        }
    }
}

/// Splits a `--header` value at its first colon and checks it.
///
/// No error contains the argument or the value: a key pasted into a header
/// must not reach an error message.
fn parse_header(text: &str) -> Result<(String, String), String> {
    let Some((name, value)) = text.split_once(':') else {
        return Err("invalid header, expected name:value".to_string());
    };
    let name = name.trim();
    if name.is_empty() {
        return Err("invalid header: the name is empty".to_string());
    }
    let value = value.trim();
    reagent_ai::check_header(name, value)?;
    Ok((name.to_string(), value.to_string()))
}

/// Expands and validates a `--policy` comma list into sorted, de-duplicated
/// policy names. `all` expands to [`ALL_POLICIES`] and `none`, valid only
/// alone, to no policy. An unknown name is an error, which the binary reports
/// by exiting 2.
fn parse_policy(value: &str) -> Result<Vec<String>, String> {
    if value.trim() == "none" {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for part in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match part {
            "none" => return Err("none must be the only policy".to_string()),
            "all" => names.extend(ALL_POLICIES.iter().map(ToString::to_string)),
            name if POLICY_NAMES.contains(&name) => names.push(name.to_string()),
            other => return Err(format!("unknown policy: {other}")),
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Parses the process arguments, or returns an exit decision.
pub fn parse() -> Result<Outcome, String> {
    parse_from(lexopt::Parser::from_env())
}

/// Parses `args` (the arguments after the program name) into an outcome.
fn parse_from(mut parser: lexopt::Parser) -> Result<Outcome, String> {
    let mut task: Option<TaskSource> = None;
    let mut json = false;
    let mut model = None;
    let mut provider = Wire::Anthropic;
    let mut base_url = None;
    let mut api_key_env = None;
    let mut header = Vec::new();
    let mut max_tokens = DEFAULT_MAX_TOKENS;
    let mut deadline = None;
    let mut cwd = None;
    let mut policy: Option<Vec<String>> = None;

    while let Some(arg) = parser.next().map_err(|error| error.to_string())? {
        match arg {
            lexopt::Arg::Short('p') | lexopt::Arg::Long("print") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --print".to_string())?;
                let text = if value == "-" {
                    TaskSource::Stdin
                } else {
                    TaskSource::Text(
                        value
                            .into_string()
                            .map_err(|_| "task must be UTF-8".to_string())?,
                    )
                };
                task = Some(text);
            }
            lexopt::Arg::Long("json") => json = true,
            lexopt::Arg::Long("model") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --model".to_string())?;
                model = Some(value.into_string().map_err(|_| bad("--model"))?);
            }
            lexopt::Arg::Long("provider") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --provider".to_string())?;
                let text = value.into_string().map_err(|_| bad("--provider"))?;
                provider = parse_provider(&text)?;
            }
            lexopt::Arg::Long("base-url") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --base-url".to_string())?;
                base_url = Some(value.into_string().map_err(|_| bad("--base-url"))?);
            }
            lexopt::Arg::Long("api-key-env") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --api-key-env".to_string())?;
                api_key_env = Some(value.into_string().map_err(|_| bad("--api-key-env"))?);
            }
            lexopt::Arg::Long("header") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --header".to_string())?;
                let text = value.into_string().map_err(|_| bad("--header"))?;
                header.push(parse_header(&text)?);
            }
            lexopt::Arg::Long("max-tokens") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --max-tokens".to_string())?;
                let text = value.into_string().map_err(|_| bad("--max-tokens"))?;
                max_tokens = text
                    .parse()
                    .ok()
                    .filter(|cap| *cap > 0)
                    .ok_or_else(|| bad("--max-tokens"))?;
            }
            lexopt::Arg::Long("deadline") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --deadline".to_string())?;
                let text = value.into_string().map_err(|_| bad("--deadline"))?;
                deadline = Some(text.parse().map_err(|_| bad("--deadline"))?);
            }
            lexopt::Arg::Long("policy") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --policy".to_string())?;
                let text = value.into_string().map_err(|_| bad("--policy"))?;
                let names = policy.get_or_insert_with(Vec::new);
                names.extend(parse_policy(&text)?);
                names.sort();
                names.dedup();
            }
            lexopt::Arg::Long("cwd") => {
                let value = parser
                    .value()
                    .map_err(|_| "missing value for --cwd".to_string())?;
                cwd = Some(PathBuf::from(value));
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
    let policy = policy.unwrap_or_else(|| {
        let names: &[&str] = if provider == Wire::OpenAiChat {
            &DEFAULT_CHAT_POLICIES
        } else {
            &DEFAULT_POLICIES
        };
        names.iter().map(ToString::to_string).collect()
    });
    // Editing old tool results breaks the reasoning the Anthropic and
    // Responses wires replay, so masking runs only on the chat wire.
    if policy.iter().any(|name| name == "mask") && provider != Wire::OpenAiChat {
        return Err("--policy mask needs --provider openai-chat".into());
    }
    if policy.iter().any(|name| name == "compact") && !may_compact(&provider) {
        return Err("--policy compact needs --provider openai-chat".into());
    }

    Ok(Outcome::Run(Box::new(Args {
        task,
        json,
        model: model.unwrap_or_else(|| DEFAULT_MODEL.into()),
        provider,
        base_url,
        api_key_env,
        header,
        max_tokens,
        deadline,
        cwd: cwd.unwrap_or_else(|| PathBuf::from(".")),
        policy,
    })))
}

/// The help text, with testing-only flags when built for tests.
pub fn help() -> String {
    let mut help = usage().to_string();
    help.push_str(extra_usage());
    help
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Outcome, String> {
        let argv = args.iter().map(std::ffi::OsString::from);
        parse_from(lexopt::Parser::from_args(argv))
    }

    fn run(args: &[&str]) -> Args {
        match parse(args) {
            Ok(Outcome::Run(args)) => *args,
            other => panic!("expected a run outcome for {args:?}, got {other:?}"),
        }
    }

    #[test]
    fn policy_default_is_budget_only() {
        assert_eq!(run(&["-p", "task"]).policy, vec!["budget"]);
    }

    #[test]
    fn policy_default_is_replaced_by_a_flag() {
        assert_eq!(
            run(&["-p", "task", "--policy", "stuck"]).policy,
            vec!["stuck"]
        );
    }

    #[test]
    fn policy_default_on_the_chat_wire_is_budget_and_mask() {
        let args = run(&["-p", "task", "--provider", "openai-chat"]);
        assert_eq!(args.policy, vec!["budget", "mask"]);
    }

    #[test]
    fn policy_flag_on_the_chat_wire_replaces_the_default() {
        let args = run(&[
            "-p",
            "task",
            "--provider",
            "openai-chat",
            "--policy",
            "stuck",
        ]);
        assert_eq!(args.policy, vec!["stuck"]);
        let args = run(&[
            "-p",
            "task",
            "--provider",
            "openai-chat",
            "--policy",
            "none",
        ]);
        assert!(args.policy.is_empty());
    }

    #[test]
    fn policy_default_is_turned_off_by_none() {
        assert!(run(&["-p", "task", "--policy", "none"]).policy.is_empty());
    }

    #[test]
    fn policy_default_none_with_a_name_is_an_error() {
        assert!(parse(&["-p", "task", "--policy", "none,budget"]).is_err());
    }

    #[test]
    fn repeated_policy_flags_add_up() {
        let args = run(&[
            "-p",
            "task",
            "--policy",
            "stuck",
            "--policy",
            "budget,stuck",
        ]);
        assert_eq!(args.policy, vec!["budget", "stuck"]);
    }

    #[test]
    fn a_single_policy_is_selected() {
        let args = run(&["-p", "task", "--policy", "stuck"]);
        assert_eq!(args.policy, vec!["stuck"]);
    }

    #[test]
    fn all_expands_to_every_policy_but_stuck_sorted() {
        let args = run(&["-p", "task", "--policy", "all"]);
        assert_eq!(args.policy, vec!["budget", "completion", "snapshot"]);
        let args = run(&["-p", "task", "--policy", "all,stuck"]);
        assert_eq!(
            args.policy,
            vec!["budget", "completion", "snapshot", "stuck"]
        );
    }

    #[test]
    fn a_comma_list_is_sorted_and_de_duplicated() {
        let args = run(&["-p", "task", "--policy", "budget,completion,budget"]);
        assert_eq!(args.policy, vec!["budget", "completion"]);
    }

    #[test]
    fn can_mix_all_with_a_name() {
        let args = run(&["-p", "task", "--policy", "all,snapshot"]);
        assert_eq!(args.policy, vec!["budget", "completion", "snapshot"]);
    }

    #[test]
    fn policy_mask_needs_the_chat_wire() {
        assert!(parse(&["-p", "task", "--policy", "mask"]).is_err());
        assert!(
            parse(&[
                "-p",
                "task",
                "--policy",
                "mask",
                "--provider",
                "openai-responses"
            ])
            .is_err()
        );
    }

    #[test]
    fn compact_policy_needs_the_chat_wire() {
        let error = parse(&["-p", "task", "--policy", "compact"]).unwrap_err();
        assert!(error.contains("openai-chat"), "{error}");
        let chat = run(&[
            "-p",
            "task",
            "--provider",
            "openai-chat",
            "--policy",
            "compact",
        ]);
        assert_eq!(chat.policy, vec!["compact"]);
        for wire in ["anthropic", "openai-responses"] {
            let refused = parse(&["-p", "task", "--policy", "compact", "--provider", wire]);
            assert!(refused.is_err(), "{wire}");
        }
    }

    #[test]
    fn policy_mask_runs_on_the_chat_wire() {
        let args = run(&[
            "-p",
            "task",
            "--provider",
            "openai-chat",
            "--policy",
            "budget,mask",
        ]);
        assert_eq!(args.policy, vec!["budget", "mask"]);
    }

    #[test]
    fn policy_mask_is_not_in_all() {
        let args = run(&["-p", "task", "--provider", "openai-chat", "--policy", "all"]);
        assert!(!args.policy.iter().any(|name| name == "mask"));
    }

    #[test]
    fn an_unknown_policy_is_an_error() {
        assert!(parse(&["-p", "task", "--policy", "nope"]).is_err());
    }

    #[test]
    fn a_missing_policy_value_is_an_error() {
        assert!(parse(&["-p", "task", "--policy"]).is_err());
    }

    #[test]
    fn the_default_provider_is_anthropic() {
        let args = run(&["-p", "task"]);
        assert_eq!(args.provider, Wire::Anthropic);
        assert_eq!(args.base_url, None);
        assert_eq!(args.api_key_env, None);
        assert!(args.header.is_empty());
    }

    #[test]
    fn each_provider_name_selects_its_wire() {
        assert_eq!(
            run(&["-p", "task", "--provider", "anthropic"]).provider,
            Wire::Anthropic
        );
        assert_eq!(
            run(&["-p", "task", "--provider", "openai-chat"]).provider,
            Wire::OpenAiChat
        );
        assert_eq!(
            run(&["-p", "task", "--provider", "openai-responses"]).provider,
            Wire::OpenAiResponses
        );
    }

    #[test]
    fn an_unknown_provider_is_an_error() {
        assert!(parse(&["-p", "task", "--provider", "google"]).is_err());
    }

    #[test]
    fn the_openai_api_key_env_default_differs_from_anthropic() {
        assert_eq!(Wire::Anthropic.default_api_key_env(), "ANTHROPIC_API_KEY");
        assert_eq!(Wire::OpenAiChat.default_api_key_env(), "OPENAI_API_KEY");
        assert_eq!(
            Wire::OpenAiResponses.default_api_key_env(),
            "OPENAI_API_KEY"
        );
    }

    #[test]
    fn a_base_url_flag_overrides_the_default() {
        let args = run(&["-p", "task", "--base-url", "http://localhost:8080"]);
        assert_eq!(args.base_url.as_deref(), Some("http://localhost:8080"));
    }

    #[test]
    fn an_api_key_env_flag_is_remembered() {
        let args = run(&["-p", "task", "--api-key-env", "MY_KEY"]);
        assert_eq!(args.api_key_env.as_deref(), Some("MY_KEY"));
    }

    #[test]
    fn a_header_is_split_at_the_first_colon() {
        let args = run(&["-p", "task", "--header", "x-opencode-session:abc:def"]);
        assert_eq!(
            args.header,
            vec![("x-opencode-session".to_string(), "abc:def".to_string())]
        );
    }

    #[test]
    fn repeated_headers_are_kept_in_order() {
        let args = run(&["-p", "task", "--header", "a:1", "--header", "b:2"]);
        assert_eq!(
            args.header,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string())
            ]
        );
    }

    #[test]
    fn a_header_without_a_colon_is_an_error() {
        let error = parse(&["-p", "task", "--header", "sk-secret"]).unwrap_err();
        assert!(error.contains("name:value"), "{error}");
        assert!(!error.contains("sk-secret"), "{error}");
    }

    #[test]
    fn a_reserved_or_invalid_header_is_an_error() {
        for header in ["authorization:sk-secret", "x api:sk-secret"] {
            let error = parse(&["-p", "task", "--header", header]).unwrap_err();
            assert!(!error.contains("sk-secret"), "{error}");
        }
    }

    #[test]
    fn an_empty_header_name_is_an_error() {
        let error = parse(&["-p", "task", "--header", ":value"]).unwrap_err();
        assert!(error.contains("name"), "{error}");
    }

    #[cfg(feature = "testing")]
    #[test]
    fn a_fake_provider_is_parsed_under_testing() {
        let args = run(&["-p", "task", "--provider", "fake:replies.json"]);
        assert_eq!(args.provider, Wire::Fake("replies.json".to_string()));
    }

    #[test]
    fn max_tokens_flag_defaults_to_64000() {
        assert_eq!(run(&["-p", "task"]).max_tokens, 64_000);
    }

    #[test]
    fn max_tokens_flag_sets_the_cap() {
        assert_eq!(
            run(&["-p", "task", "--max-tokens", "16000"]).max_tokens,
            16_000
        );
    }

    #[test]
    fn max_tokens_flag_rejects_a_missing_zero_or_bad_value() {
        assert!(parse(&["-p", "task", "--max-tokens"]).is_err());
        assert!(parse(&["-p", "task", "--max-tokens", "0"]).is_err());
        assert!(parse(&["-p", "task", "--max-tokens", "many"]).is_err());
    }
}
