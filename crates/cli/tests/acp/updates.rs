//! The updates a session sends: commands, title, usage, notices and message ids.

use super::*;

/// The updates of `kind` in `lines`.
fn updates_of<'a>(lines: &'a [Value], kind: &str) -> Vec<&'a Value> {
    updates(lines)
        .into_iter()
        .filter(|update| update["sessionUpdate"] == kind)
        .collect()
}

/// Whether `line` is the update of `kind`.
fn is_update(line: &Value, kind: &str) -> bool {
    line["method"] == "session/update" && line["params"]["update"]["sessionUpdate"] == kind
}

/// Writes the skill `name` into the global skills folder of `root`.
async fn skill(root: &Path, name: &str, description: &str) {
    let folder = root
        .join("config")
        .join("reagent")
        .join("skills")
        .join(name);
    tokio::fs::create_dir_all(&folder)
        .await
        .expect("the skill folder");
    tokio::fs::write(
        folder.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n"),
    )
    .await
    .expect("the skill file");
}

/// Writes the plugin `folder` into the plugins folder of `root`.
async fn plugin(root: &Path, folder: &str, entry: &str) {
    let dir = root
        .join("config")
        .join("reagent")
        .join("plugins")
        .join(folder);
    tokio::fs::create_dir_all(&dir)
        .await
        .expect("the plugin dir");
    tokio::fs::write(
        dir.join("package.json"),
        format!(r#"{{"name": "{folder}", "reagent": {{"entry": "index.js"}}}}"#),
    )
    .await
    .expect("the manifest");
    tokio::fs::write(dir.join("index.js"), entry)
        .await
        .expect("the entry");
}

/// The commands the session offers name the skill it found and the command its
/// plugin registered, each with the line the editor shows.
#[tokio::test]
async fn acp_available_commands_list_a_skill_and_a_plugin_command() {
    let state = state();
    let cwd = work(state.path()).await;
    skill(state.path(), "tidy", "tidy the files").await;
    plugin(
        state.path(),
        "deployer",
        r#"
export default function (r) {
  r.registerCommand("deploy", {
    description: "ship it",
    run: () => "shipped",
  });
}
"#,
    )
    .await;
    let provider = script(state.path(), r#"{"replies":[{"text":"ok"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
    acp.initialize().await;
    let _session = acp.new_session(&cwd).await;

    let lines = acp
        .until(|line| {
            is_update(line, "available_commands_update")
                && line["params"]["update"]["availableCommands"]
                    .as_array()
                    .is_some_and(|list| list.iter().any(|command| command["name"] == "deploy"))
        })
        .await;
    // The first list names the skills, and the one the plugins add follows, so
    // the last list is the one an editor shows.
    let update = updates_of(&lines, "available_commands_update")
        .pop()
        .unwrap_or_else(|| panic!("no list of commands in {lines:?}"));
    let commands = update["availableCommands"]
        .as_array()
        .unwrap_or_else(|| panic!("a list of commands: {update}"));
    let named = |name: &str| commands.iter().find(|command| command["name"] == name);
    let tidy = named("tidy").unwrap_or_else(|| panic!("no tidy command: {update}"));
    assert!(
        tidy["description"]
            .as_str()
            .is_some_and(|line| line.contains("skill")),
        "the skill's line: {tidy}"
    );
    let deploy = named("deploy").unwrap_or_else(|| panic!("no deploy command: {update}"));
    assert_eq!(deploy["description"], "ship it", "{deploy}");
    assert_eq!(deploy["input"]["hint"], "arguments", "{deploy}");
    assert!(named("tidy").expect("tidy").get("input").is_none());
}

/// The first prompt names the title of the session.
#[tokio::test]
async fn acp_session_info_names_the_title_of_the_first_prompt() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"ok"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let id = acp.prompt(&session, "Fix the login bug").await;
    let lines = acp
        .until(|line| is_update(line, "session_info_update"))
        .await;
    let update = update_of(&lines, "session_info_update");
    assert_eq!(update["title"], "Fix the login bug", "{update}");
    let answer = acp.answer(id).await;
    assert_eq!(
        answer["result"]["stopReason"], "end_turn",
        "the run ends on its own: {answer}"
    );
}

/// A `usage` event sends the tokens the request used and the window they used.
#[tokio::test]
async fn acp_usage_update_names_the_tokens_and_the_window() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"text":"ok","usage":{"input":10,"output":5,"cache_read":2,"cache_write":0,"reasoning":0}}]}"#,
    )
    .await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let id = acp.prompt(&session, "count them").await;
    let lines = acp.until(|line| is_update(line, "usage_update")).await;
    let update = update_of(&lines, "usage_update");
    assert_eq!(update["used"], 17, "{update}");
    assert!(
        update["size"].as_u64().is_some_and(|size| size > 0),
        "a window and not a word: {update}"
    );
    let answer = acp.answer(id).await;
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
}

/// An editor that advertises notices takes a retried request as one, and one
/// that does not still sees the wait as a thought.
#[tokio::test]
async fn acp_a_notices_client_takes_a_retry_as_a_notice() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"text":"half","fail":"429 too many requests"},{"text":"ok"}]}"#,
    )
    .await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
    acp.initialize_with(json!({"session": {"notices": {}}}))
        .await;
    let session = acp.new_session(&cwd).await;

    let id = acp.prompt(&session, "go on").await;
    let lines = acp.until(|line| is_update(line, "notice")).await;
    let update = update_of(&lines, "notice");
    assert_eq!(update["severity"], "warning", "{update}");
    assert_eq!(update["title"], "retry 1", "{update}");
    assert!(
        update["description"]
            .as_str()
            .is_some_and(|text| text.contains("429")),
        "the error that caused it: {update}"
    );
    let answer = acp.answer(id).await;
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    // The notice is the only place the wait shows, not the reply.
    assert!(
        !chunks(&lines, "agent_thought_chunk").contains("retry 1"),
        "the wait is a notice alone: {lines:?}"
    );
}

/// Every chunk of one message names it, so the editor groups them.
#[tokio::test]
async fn acp_chunks_carry_the_message_id() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"text":"one ","deltas":["two"]}]}"#,
    )
    .await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let id = acp.prompt(&session, "write two words").await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let answer = lines.last().expect("the answer");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");

    let said = updates_of(&lines, "agent_message_chunk");
    assert_eq!(said.len(), 2, "{lines:?}");
    let first = said[0]["messageId"].as_str().expect("an id");
    for chunk in &said {
        assert_eq!(chunk["messageId"], first, "{chunk}");
    }
    assert_eq!(chunks(&lines, "agent_message_chunk"), "one two");

    let asked = updates_of(&lines, "user_message_chunk");
    let id = asked
        .first()
        .expect("the prompt")
        .get("messageId")
        .and_then(Value::as_str)
        .expect("an id");
    assert_ne!(id, first, "the user's message is its own");
}

/// A tool call names the tool it runs, and its last update carries the text the
/// call produced.
#[tokio::test]
async fn acp_tool_call_names_the_tool_and_its_output() {
    let state = state();
    let cwd = work(state.path()).await;
    let file = cwd.join("note.txt");
    tokio::fs::write(&file, "the file's text\n")
        .await
        .expect("the file");
    let provider = script(
        state.path(),
        &format!(
            r#"{{"replies":[{{"calls":[{{"id":"c1","name":"read","arguments":{{"path":"{}"}}}}]}},{{"text":"done"}}]}}"#,
            path(&file)
        ),
    )
    .await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let id = acp.prompt(&session, "read the note").await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let answer = lines.last().expect("the answer");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");

    let call = update_of(&lines, "tool_call");
    assert_eq!(call["name"], "read", "{call}");
    assert_eq!(call["kind"], "read", "{call}");
    assert_eq!(call["toolCallId"], "c1", "{call}");

    let done = updates_of(&lines, "tool_call_update")
        .into_iter()
        .find(|update| update["status"] == "completed")
        .unwrap_or_else(|| panic!("no completed update in {lines:?}"));
    assert_eq!(done["toolCallId"], "c1", "{done}");
    assert!(
        done["rawOutput"]
            .as_str()
            .is_some_and(|text| text.contains("the file's text")),
        "the text the call produced: {done}"
    );
}

/// A reply the model's output limit stopped answers `max_tokens`, not the end
/// of the turn.
#[tokio::test]
async fn acp_a_max_tokens_reply_answers_max_tokens() {
    let state = state();
    let cwd = work(state.path()).await;
    // Four replies: the run recovers from three of them and the fourth ends it
    // at the cap, which is the reason the editor is told.
    let provider = script(
        state.path(),
        r#"{"replies":[{"max_tokens":true},{"max_tokens":true},{"max_tokens":true},{"max_tokens":true}]}"#,
    )
    .await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let id = acp.prompt(&session, "write a lot").await;
    let answer = acp.answer(id).await;
    assert_eq!(
        answer["result"]["stopReason"], "max_tokens",
        "the cap stopped the reply: {answer}"
    );
    assert!(answer["error"].is_null(), "no error: {answer}");
}

/// A prompt that starts with the slash name of a plugin command runs that
/// command and never reaches the model; the prompt ends with `end_turn` once the
/// command is done, and the text it printed shows in the reply.
#[tokio::test]
async fn acp_a_plugin_command_prompt_runs_the_command_not_the_model() {
    let state = state();
    let cwd = work(state.path()).await;
    plugin(
        state.path(),
        "deployer",
        r#"
export default function (r) {
  r.registerCommand("deploy", {
    description: "ship it",
    run: (args) => "shipped " + args,
  });
}
"#,
    )
    .await;
    let provider = script(state.path(), r#"{"replies":[{"text":"model was asked"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    acp.until(|line| {
        is_update(line, "available_commands_update")
            && line["params"]["update"]["availableCommands"]
                .as_array()
                .is_some_and(|list| list.iter().any(|command| command["name"] == "deploy"))
    })
    .await;

    let id = acp.prompt(&session, "/deploy to staging").await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let answer = lines.last().expect("an answer");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    let said = chunks(&lines, "agent_message_chunk");
    assert!(said.contains("shipped to staging"), "{said}");
    assert!(!said.contains("model was asked"), "{said}");
}

/// A plugin command that starts a run answers its prompt when that run ends.
#[tokio::test]
async fn acp_a_plugin_command_that_starts_a_run_ends_with_that_run() {
    let state = state();
    let cwd = work(state.path()).await;
    plugin(
        state.path(),
        "talker",
        r#"
export default function (r) {
  r.registerCommand("talk", {
    description: "asks the model",
    run: () => {
      r.send("say it");
      return "talking";
    },
  });
}
"#,
    )
    .await;
    let provider = script(state.path(), r#"{"replies":[{"text":"said it"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    acp.until(|line| {
        is_update(line, "available_commands_update")
            && line["params"]["update"]["availableCommands"]
                .as_array()
                .is_some_and(|list| list.iter().any(|command| command["name"] == "talk"))
    })
    .await;

    let id = acp.prompt(&session, "/talk").await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let answer = lines.last().expect("an answer");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    let said = chunks(&lines, "agent_message_chunk");
    assert!(said.contains("talking"), "the command ran: {said}");
    assert!(
        said.contains("said it"),
        "the run ended before the answer: {said}"
    );
}

/// The title update names when the session last changed, as ISO 8601.
#[tokio::test]
async fn acp_session_info_names_when_the_session_was_updated() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"ok"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    acp.prompt(&session, "Fix the login bug").await;
    let lines = acp
        .until(|line| is_update(line, "session_info_update"))
        .await;
    let update = update_of(&lines, "session_info_update");
    let stamp = update["updatedAt"]
        .as_str()
        .unwrap_or_else(|| panic!("no updatedAt in {update}"));
    let bytes = stamp.as_bytes();
    assert!(
        stamp.len() >= 20
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes[10] == b'T'
            && stamp.ends_with('Z'),
        "not ISO 8601: {stamp}"
    );
}

/// The title is the first user text as the store cuts it: a newline inside it
/// becomes a space, not the end of the title.
#[tokio::test]
async fn acp_the_title_is_cut_as_the_store_cuts_it() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"ok"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let id = acp
        .prompt(&session, "Fix the login bug\nand the rest of it")
        .await;
    let lines = acp
        .until(|line| is_update(line, "session_info_update"))
        .await;
    let update = update_of(&lines, "session_info_update");
    assert_eq!(
        update["title"], "Fix the login bug and the rest of it",
        "the store cuts the first user text, newlines and all: {update}"
    );
    let answer = acp.answer(id).await;
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
}

/// The title holds at most as many characters as the store keeps.
#[tokio::test]
async fn acp_the_title_holds_the_characters_the_store_keeps() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"ok"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    acp.prompt(&session, &"x".repeat(70)).await;
    let lines = acp
        .until(|line| is_update(line, "session_info_update"))
        .await;
    let update = update_of(&lines, "session_info_update");
    assert_eq!(update["title"], "x".repeat(60), "{update}");
}

/// A session that is loaded, or resumed, names the title of its first user text,
/// and a prompt after it does not rename the session.
#[tokio::test]
async fn acp_a_loaded_or_resumed_session_keeps_the_title_of_its_first_prompt() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"text":"ok"},{"text":"ok"},{"text":"ok"}]}"#,
    )
    .await;
    let mut first = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
    first.initialize().await;
    let session = first.new_session(&cwd).await;
    let id = first.prompt(&session, "first task").await;
    first.until(|line| line["id"].as_u64() == Some(id)).await;

    for method in ["session/load", "session/resume"] {
        let mut second = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
        second.initialize().await;
        let open = second
            .request(
                method,
                json!({"sessionId": session, "cwd": path(&cwd), "mcpServers": []}),
            )
            .await;
        let mut opened = second.until(|line| line["id"].as_u64() == Some(open)).await;
        // The title follows the answer.
        opened.extend(
            second
                .until(|line| is_update(line, "session_info_update"))
                .await,
        );
        let named = updates_of(&opened, "session_info_update");
        assert!(
            named.iter().any(|update| update["title"] == "first task"),
            "{method} names the title: {opened:?}"
        );

        let id = second.prompt(&session, "second task").await;
        let lines = second.until(|line| line["id"].as_u64() == Some(id)).await;
        for update in updates_of(&lines, "session_info_update") {
            assert_eq!(
                update["title"], "first task",
                "{method}: the title is the first user text, not this prompt's: {update}"
            );
        }
    }
}

/// A plugin command that the editor cancels while it runs ends its prompt with
/// `cancelled`, not `end_turn`.
#[tokio::test]
async fn acp_a_cancelled_plugin_command_ends_with_cancelled() {
    let state = state();
    let cwd = work(state.path()).await;
    plugin(
        state.path(),
        "slowpoke",
        r#"
export default function (r) {
  r.registerCommand("slow", {
    description: "waits",
    async run() {
      await new Promise((done) => setTimeout(done, 700));
      return "late";
    },
  });
}
"#,
    )
    .await;
    let provider = script(state.path(), r#"{"replies":[{"text":"never asked"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", &provider]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    acp.until(|line| {
        is_update(line, "available_commands_update")
            && line["params"]["update"]["availableCommands"]
                .as_array()
                .is_some_and(|list| list.iter().any(|command| command["name"] == "slow"))
    })
    .await;

    let id = acp.prompt(&session, "/slow").await;
    acp.notify("session/cancel", json!({"sessionId": session}))
        .await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let answer = lines.last().expect("an answer");
    assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
}

/// A worker refusal of another request while a plugin command runs leaves the
/// command's prompt open, and the prompt ends with the command.
#[tokio::test]
async fn acp_a_refused_set_during_a_plugin_command_leaves_the_command_open() {
    let state = state();
    let cwd = work(state.path()).await;
    plugin(
        state.path(),
        "slowpoke",
        r#"
export default function (r) {
  r.registerCommand("slow", {
    description: "waits",
    async run() {
      await new Promise((done) => setTimeout(done, 700));
      return "late";
    },
  });
}
"#,
    )
    .await;
    // The session runs the scripted provider `one`, and the provider `keyless`
    // reads a key variable this test never sets.
    let provider = script(state.path(), r#"{"replies":[{"text":"never asked"}]}"#).await;
    let config = state.path().join("config").join("reagent");
    tokio::fs::create_dir_all(&config)
        .await
        .expect("the config folder");
    tokio::fs::write(
        config.join("config.toml"),
        format!(
            "[providers.one]\nwire = \"{provider}\"\n\n\
             [providers.keyless]\nwire = \"anthropic\"\nbase_url = \"http://127.0.0.1:1\"\n\
             api_key_env = \"REAGENT_TEST_ACP_KEYLESS_KEY\"\n"
        ),
    )
    .await
    .expect("the config");
    let mut acp = Acp::start(state.path(), &cwd, &["--model", "one/model-a"]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    acp.until(|line| {
        is_update(line, "available_commands_update")
            && line["params"]["update"]["availableCommands"]
                .as_array()
                .is_some_and(|list| list.iter().any(|command| command["name"] == "slow"))
    })
    .await;

    let id = acp.prompt(&session, "/slow").await;
    // The worker refuses the switch to a provider with no key.
    acp.request(
        "session/set_config_option",
        json!({"sessionId": session, "configId": "model", "value": "keyless/model-b"}),
    )
    .await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let answer = lines.last().expect("an answer");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert!(
        updates(&lines).into_iter().any(|update| {
            update["sessionUpdate"] == "agent_message_chunk" && update["content"]["text"] == "late"
        }),
        "the command's text comes first: {lines:?}"
    );
}
