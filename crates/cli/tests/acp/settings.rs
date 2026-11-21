//! The session settings: the reasoning level, the permission mode and the updates a change sends.

use super::*;

impl Acp {
    /// Loads the session `session` in `cwd` and answers the answer of the
    /// request.
    async fn load(&mut self, session: &str, cwd: &Path) -> Value {
        let id = self
            .request(
                "session/load",
                json!({"sessionId": session, "cwd": path(cwd), "mcpServers": []}),
            )
            .await;
        self.answer(id).await
    }

    /// Sets the option `config` of the session to `value` and answers the
    /// answer of the request.
    async fn set_option(&mut self, session: &str, config: &str, value: &str) -> Value {
        let id = self
            .request(
                "session/set_config_option",
                json!({"sessionId": session, "configId": config, "value": value}),
            )
            .await;
        self.answer(id).await
    }

    /// Sets the mode of the session and answers the answer of the request.
    async fn set_mode(&mut self, session: &str, mode: &str) -> Value {
        let id = self
            .request(
                "session/set_mode",
                json!({"sessionId": session, "modeId": mode}),
            )
            .await;
        self.answer(id).await
    }
}

/// The ids of the `configOptions` of an answer or of an update.
fn option_ids(options: &Value) -> Vec<&str> {
    options
        .as_array()
        .unwrap_or_else(|| panic!("configOptions: {options}"))
        .iter()
        .map(|option| option["id"].as_str().unwrap_or_default())
        .collect()
}

/// The config option `id` of `options`.
fn option_of<'a>(options: &'a Value, id: &str) -> &'a Value {
    options
        .as_array()
        .and_then(|options| options.iter().find(|option| option["id"] == id))
        .unwrap_or_else(|| panic!("no option {id} in {options}"))
}

/// The values of the options of the config option `id`.
fn values_of<'a>(options: &'a Value, id: &str) -> Vec<&'a str> {
    option_of(options, id)["options"]
        .as_array()
        .unwrap_or_else(|| panic!("the options of {id}: {options}"))
        .iter()
        .map(|option| option["value"].as_str().unwrap_or_default())
        .collect()
}

/// The `currentValue` of the config option `id`.
fn current_of<'a>(options: &'a Value, id: &str) -> &'a str {
    option_of(options, id)["currentValue"]
        .as_str()
        .unwrap_or_else(|| panic!("the current value of {id}: {options}"))
}

/// The `modes` of the answer of a `session/new` or a `session/load`.
fn modes_of(result: &Value) -> &Value {
    result
        .get("modes")
        .unwrap_or_else(|| panic!("modes in {result}"))
}

/// The ids of the `availableModes` of `modes`.
fn mode_ids(modes: &Value) -> Vec<&str> {
    modes["availableModes"]
        .as_array()
        .unwrap_or_else(|| panic!("availableModes: {modes}"))
        .iter()
        .map(|mode| mode["id"].as_str().unwrap_or_default())
        .collect()
}

/// A new session answers the three config options and the modes it starts with.
#[tokio::test]
async fn acp_new_session_names_the_level_and_mode_options() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"hi"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let id = acp
        .request("session/new", json!({"cwd": path(&cwd), "mcpServers": []}))
        .await;
    let answer = acp.answer(id).await;
    let result = &answer["result"];
    let options = &result["configOptions"];
    assert_eq!(
        option_ids(options),
        ["model", "thought_level", "mode"],
        "the options in order: {answer}"
    );

    let model = option_of(options, "model");
    assert_eq!(model["category"], "model", "{model}");
    assert_eq!(model["type"], "select", "{model}");
    assert!(!current_of(options, "model").is_empty(), "{model}");

    let level = option_of(options, "thought_level");
    assert_eq!(level["category"], "thought_level", "{level}");
    assert_eq!(level["type"], "select", "{level}");
    assert_eq!(current_of(options, "thought_level"), "off", "{level}");
    assert_eq!(
        values_of(options, "thought_level"),
        ["off", "low", "medium", "high"],
        "the level of a model whose own levels are not known: {level}"
    );
    assert!(
        level["options"].as_array().is_some_and(|options| options
            .iter()
            .all(|option| option["name"].as_str().is_some_and(|name| !name.is_empty()))),
        "every level has a name: {level}"
    );

    let mode = option_of(options, "mode");
    assert_eq!(mode["category"], "mode", "{mode}");
    assert_eq!(mode["type"], "select", "{mode}");
    assert_eq!(current_of(options, "mode"), "default", "{mode}");
    assert_eq!(
        values_of(options, "mode"),
        ["default", "accept-edits", "bypass", "auto"],
        "{mode}"
    );

    let modes = modes_of(result);
    assert_eq!(modes["currentModeId"], "default", "{modes}");
    assert_eq!(
        mode_ids(modes),
        ["default", "accept-edits", "bypass", "auto"],
        "{modes}"
    );
    assert!(
        modes["availableModes"].as_array().is_some_and(|modes| modes
            .iter()
            .all(|mode| mode["name"].as_str().is_some_and(|name| !name.is_empty()))),
        "every mode has a name: {modes}"
    );
}

/// The editor sets the level and the mode through `session/set_config_option`,
/// and each answer names what it set. The change it made itself sends no
/// update, since its answer already named it.
#[tokio::test]
async fn acp_set_config_option_sets_the_level_and_the_mode() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"hi"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let answer = acp.set_option(&session, "thought_level", "high").await;
    assert!(answer.get("error").is_none(), "{answer}");
    assert_eq!(
        current_of(&answer["result"]["configOptions"], "thought_level"),
        "high",
        "{answer}"
    );

    let answer = acp.set_option(&session, "mode", "auto").await;
    assert!(answer.get("error").is_none(), "{answer}");
    assert_eq!(
        current_of(&answer["result"]["configOptions"], "mode"),
        "auto",
        "{answer}"
    );

    // A set answers what it changed, so the worker's notice of the same change
    // sends no second update.
    let mut lines = Vec::new();
    while let Ok(line) = tokio::time::timeout(Duration::from_secs(2), acp.line()).await {
        lines.push(line);
    }
    let sent = updates(&lines)
        .into_iter()
        .filter_map(|update| update["sessionUpdate"].as_str())
        .any(|kind| kind == "config_option_update" || kind == "current_mode_update");
    assert!(!sent, "no update for its own set: {lines:?}");
}

/// `session/set_mode` sets the mode, answers `{}` and refuses a mode this agent
/// does not know.
#[tokio::test]
async fn acp_set_mode_sets_the_mode() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"hi"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let answer = acp.set_mode(&session, "accept-edits").await;
    assert!(answer.get("error").is_none(), "{answer}");
    assert_eq!(answer["result"], json!({}), "{answer}");

    let answer = acp.set_mode(&session, "turbo").await;
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
}

/// A value outside the options of a select, and a config id this agent has no
/// option for, are invalid params.
#[tokio::test]
async fn acp_a_bad_level_or_mode_is_invalid_params() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"hi"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let answer = acp.set_option(&session, "thought_level", "turbo").await;
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
    let answer = acp.set_option(&session, "mode", "turbo").await;
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
    let answer = acp.set_option(&session, "flavor", "sweet").await;
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
    // A model takes any text, as the model sheet does.
    let answer = acp.set_option(&session, "model", "fake/other-model").await;
    assert!(answer.get("error").is_none(), "{answer}");
}

/// A second process that loads the session reads the level the first one set,
/// and the modes it answers name the mode to set.
#[tokio::test]
async fn acp_load_reads_the_level_a_set_changed() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"hi"}]}"#).await;
    let mut first = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    first.initialize().await;
    let session = first.new_session(&cwd).await;
    let answer = first.set_option(&session, "thought_level", "high").await;
    assert!(answer.get("error").is_none(), "{answer}");

    let mut second = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    second.initialize().await;
    let answer = second.load(&session, &cwd).await;
    assert!(answer.get("error").is_none(), "{answer}");
    let result = &answer["result"];
    assert_eq!(
        current_of(&result["configOptions"], "thought_level"),
        "high",
        "{answer}"
    );
    assert_eq!(
        values_of(&result["configOptions"], "mode"),
        ["default", "accept-edits", "bypass", "auto"],
        "{answer}"
    );
    assert!(
        !mode_ids(modes_of(result)).is_empty(),
        "a load answers the modes: {answer}"
    );
}

/// A change another client makes reaches this editor as a config option update,
/// and as a mode update when it is the mode that changed.
#[tokio::test]
async fn acp_a_change_of_another_client_reaches_the_editor() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"hi"}]}"#).await;
    let mut first = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    first.initialize().await;
    let session = first.new_session(&cwd).await;

    let mut second = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    second.initialize().await;
    let answer = second.load(&session, &cwd).await;
    assert!(answer.get("error").is_none(), "{answer}");

    let answer = second.set_mode(&session, "accept-edits").await;
    assert!(answer.get("error").is_none(), "{answer}");
    let lines = first
        .until(|line| line["params"]["update"]["sessionUpdate"] == "current_mode_update")
        .await;
    let mode = updates(&lines)
        .into_iter()
        .find(|update| update["sessionUpdate"] == "current_mode_update")
        .expect("a mode update");
    assert_eq!(mode["currentModeId"], "accept-edits", "{mode}");
    let options = updates(&lines)
        .into_iter()
        .find(|update| update["sessionUpdate"] == "config_option_update")
        .unwrap_or_else(|| panic!("a config option update: {lines:?}"));
    assert_eq!(
        current_of(&options["configOptions"], "mode"),
        "accept-edits",
        "{options}"
    );
    assert_eq!(
        option_ids(&options["configOptions"]),
        ["model", "thought_level", "mode"],
        "{options}"
    );

    // A level change of another client is a config option update alone.
    let answer = second.set_option(&session, "thought_level", "low").await;
    assert!(answer.get("error").is_none(), "{answer}");
    let low = |line: &Value| {
        line["params"]["update"]["sessionUpdate"] == "config_option_update"
            && line["params"]["update"]["configOptions"]
                .as_array()
                .and_then(|options| {
                    options
                        .iter()
                        .find(|option| option["id"] == "thought_level")
                })
                .is_some_and(|level| level["currentValue"] == "low")
    };
    let lines = first.until(low).await;
    assert!(
        updates(&lines)
            .into_iter()
            .all(|update| update["sessionUpdate"] != "current_mode_update"),
        "a level is no mode: {lines:?}"
    );
}

/// The update that names the level `value`, for `until`.
fn at_level(value: &'static str) -> impl Fn(&Value) -> bool {
    move |line| {
        line["params"]["update"]["sessionUpdate"] == "config_option_update"
            && line["params"]["update"]["configOptions"]
                .as_array()
                .and_then(|options| {
                    options
                        .iter()
                        .find(|option| option["id"] == "thought_level")
                })
                .is_some_and(|level| level["currentValue"] == value)
    }
}

/// A set of the value the session already runs changes nothing, so the worker
/// reports nothing for it. The change another client makes back to that value
/// is not that set's notice, so the editor hears it.
#[tokio::test]
async fn acp_a_set_of_the_same_value_hides_no_later_change() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"hi"}]}"#).await;
    let mut first = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    first.initialize().await;
    let session = first.new_session(&cwd).await;

    let mut second = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    second.initialize().await;
    assert!(second.load(&session, &cwd).await.get("error").is_none());

    // The first editor sets the level twice: the second set changes nothing.
    let answer = first.set_option(&session, "thought_level", "high").await;
    assert!(answer.get("error").is_none(), "{answer}");
    let answer = first.set_option(&session, "thought_level", "high").await;
    assert!(answer.get("error").is_none(), "{answer}");

    // Another client moves the level away and back to the value this editor
    // set. Both changes reach this editor: the answer of its own set named the
    // value, and no notice of it follows.
    let answer = second.set_option(&session, "thought_level", "low").await;
    assert!(answer.get("error").is_none(), "{answer}");
    first.until(at_level("low")).await;
    let answer = second.set_option(&session, "thought_level", "high").await;
    assert!(answer.get("error").is_none(), "{answer}");
    first.until(at_level("high")).await;
}

/// A mode the run refuses keeps the mode of the session: the options this agent
/// answers name the mode it kept, not the mode the editor asked for.
#[tokio::test]
async fn acp_a_refused_mode_leaves_the_options_naming_the_mode_kept() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"hi"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    // This run did not enable `bypass`, so its guard keeps `default`: the mode
    // the run kept comes back as an update, with the reason in the transcript.
    let _ = acp.set_mode(&session, "bypass").await;
    let lines = acp
        .until(|line| {
            line["params"]["update"]["sessionUpdate"] == "current_mode_update"
                && line["params"]["update"]["currentModeId"] == "default"
        })
        .await;
    assert!(
        updates(&lines)
            .into_iter()
            .any(|update| update["sessionUpdate"] == "config_option_update"
                && current_of(&update["configOptions"], "mode") == "default"),
        "the options name the mode kept: {lines:?}"
    );
    assert!(
        updates(&lines).into_iter().any(|update| {
            update["sessionUpdate"] == "agent_thought_chunk"
                && update["content"]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("bypass"))
        }),
        "the reason shows: {lines:?}"
    );

    // The next answer names every option as it stands.
    let answer = acp.set_option(&session, "thought_level", "medium").await;
    assert!(answer.get("error").is_none(), "{answer}");
    assert_eq!(
        current_of(&answer["result"]["configOptions"], "mode"),
        "default",
        "the session kept `default`: {answer}"
    );
}

/// The answer to a set sent right after a mode the run refuses names the mode
/// the session kept, not the mode the editor asked for.
#[tokio::test]
async fn acp_a_refused_mode_is_never_the_mode_the_options_name() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"hi"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let _ = acp.set_mode(&session, "bypass").await;
    let answer = acp.set_option(&session, "thought_level", "medium").await;
    assert!(answer.get("error").is_none(), "{answer}");
    assert_eq!(
        current_of(&answer["result"]["configOptions"], "mode"),
        "default",
        "the session kept `default`: {answer}"
    );
}

/// A client that loads a live session gets the mode in force, not the mode
/// every run starts with.
#[tokio::test]
async fn acp_load_reports_the_mode_in_force() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"hi"}]}"#).await;
    let mut first = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    first.initialize().await;
    let session = first.new_session(&cwd).await;
    // An observer confirms that the worker applied the change before the load.
    let mut observer = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    observer.initialize().await;
    observer.load(&session, &cwd).await;
    assert!(
        first
            .set_mode(&session, "auto")
            .await
            .get("error")
            .is_none()
    );
    observer
        .until(|line| {
            line["params"]["update"]["sessionUpdate"] == "current_mode_update"
                && line["params"]["update"]["currentModeId"] == "auto"
        })
        .await;
    let mut second = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    second.initialize().await;
    let answer = second.load(&session, &cwd).await;
    assert_eq!(
        answer["result"]["modes"]["currentModeId"], "auto",
        "{answer}"
    );
    assert_eq!(
        current_of(&answer["result"]["configOptions"], "mode"),
        "auto",
        "{answer}"
    );
}
