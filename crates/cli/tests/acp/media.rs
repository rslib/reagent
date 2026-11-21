//! The image blocks of a prompt, and the elicitation form a question set becomes when the editor advertises one.

use super::*;

impl Acp {
    /// Loads `session` in `cwd` and answers the id of its request.
    async fn load_request(&mut self, session: &str, cwd: &Path) -> u64 {
        self.request(
            "session/load",
            json!({"sessionId": session, "cwd": path(cwd), "mcpServers": []}),
        )
        .await
    }
}

/// The text each tool call update of `lines` carries, joined.
fn tool_output(lines: &[Value]) -> String {
    let mut text = String::new();
    for update in updates(lines) {
        for content in update["content"].as_array().unwrap_or(&Vec::new()) {
            text.push_str(content["content"]["text"].as_str().unwrap_or_default());
        }
    }
    text
}

/// The files under `dir`, deepest, in name order.
async fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&next).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let found = entry.path();
            if tokio::fs::metadata(&found)
                .await
                .map(|meta| meta.is_dir())
                .unwrap_or(false)
            {
                pending.push(found);
            } else {
                out.push(found);
            }
        }
    }
    out.sort();
    out
}

/// The one image file the state folder holds for `session`, and its bytes.
async fn only_image(state: &Path, session: &str) -> PathBuf {
    let sessions = state.join("reagent").join("sessions");
    let images: Vec<PathBuf> = files(&sessions)
        .await
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".png"))
        })
        .collect();
    assert_eq!(images.len(), 1, "one image file for {session}: {images:?}");
    images.into_iter().next().expect("one image")
}

/// An image block of a prompt is written to the session's own folder in the
/// state folder and reaches the worker as an attachment.
#[tokio::test]
async fn acp_image_prompt_reaches_the_worker_as_an_attachment() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"seen"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    let answer = acp.initialize().await;
    assert_eq!(
        answer["result"]["agentCapabilities"]["promptCapabilities"]["image"], true,
        "the agent takes images: {answer}"
    );
    let session = acp.new_session(&cwd).await;

    let id = acp
        .prompt_blocks(
            &session,
            json!([
                {"type": "text", "text": "look at this"},
                {"type": "image", "data": shot(), "mimeType": "image/png"},
            ]),
        )
        .await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let answer = lines.last().expect("an answer");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");

    // The image is the session's own file, in the state folder and not the work
    // tree, and it holds the bytes the editor sent.
    let image = only_image(state.path(), &session).await;
    assert_eq!(
        tokio::fs::read(&image).await.expect("the image"),
        SHOT,
        "{}",
        image.display()
    );
    let mut entries: Vec<String> = Vec::new();
    let mut read = tokio::fs::read_dir(&cwd).await.expect("the work folder");
    while let Ok(Some(entry)) = read.next_entry().await {
        entries.push(entry.file_name().to_string_lossy().to_string());
    }
    assert_eq!(entries, [".git"], "the work tree holds no image");

    // A second editor that loads the session reads the note line the worker
    // wrote beside the text, which only an attached image makes.
    let mut second = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    second.initialize().await;
    let load = second.load_request(&session, &cwd).await;
    let replayed = second.until(|line| line["id"].as_u64() == Some(load)).await;
    let said = chunks(&replayed, "user_message_chunk");
    assert!(said.contains("look at this"), "{said}");
    assert!(
        said.contains("[image "),
        "the worker attached the image: {said}"
    );
}

/// An audio block is refused, since this agent advertises no audio.
#[tokio::test]
async fn acp_audio_block_is_invalid_params() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"never asked"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    let id = acp
        .prompt_blocks(
            &session,
            json!([{"type": "audio", "data": shot(), "mimeType": "audio/wav"}]),
        )
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(answer["error"]["code"], -32602, "invalid params: {answer}");
}

/// A blob that is not an image is refused.
#[tokio::test]
async fn acp_resource_blob_of_text_is_invalid_params() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"never asked"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;
    let id = acp
        .prompt_blocks(
            &session,
            json!([{
                "type": "resource",
                "resource": {
                    "uri": "file:///tmp/notes.bin",
                    "mimeType": "application/octet-stream",
                    "blob": shot(),
                },
            }]),
        )
        .await;
    let answer = acp.answer(id).await;
    assert_eq!(answer["error"]["code"], -32602, "invalid params: {answer}");
}

/// The questions of one `ask` call are one elicitation form when the editor
/// advertises that it shows one, and the answers reach the worker.
#[tokio::test]
async fn acp_question_set_is_one_elicitation_form() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"calls":[{"name":"ask","arguments":{"questions":[{"q":"Which database?","options":[{"label":"sqlite"},{"label":"postgres"}]},{"q":"Which extras?","multi":true,"options":[{"label":"cache"},{"label":"queue"}]}]}}]},{"text":"done"}]}"#,
    )
    .await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize_with(json!({"elicitation": {"form": {}}}))
        .await;
    let session = acp.new_session(&cwd).await;

    let id = acp.prompt(&session, "ask me").await;
    let asked = acp
        .until(|line| line["method"] == "elicitation/create")
        .await;
    let ask = asked.last().expect("an elicitation request");
    let params = &ask["params"];
    assert_eq!(params["mode"], "form", "{ask}");
    assert_eq!(params["sessionId"], session.as_str(), "{ask}");
    assert_eq!(params["message"], "Which database?", "{ask}");
    let properties = &params["requestedSchema"]["properties"];
    assert_eq!(
        properties["q0"]["oneOf"][1]["const"], "postgres",
        "the first question is one choice: {ask}"
    );
    assert_eq!(
        properties["q1"]["type"], "array",
        "the second question takes several: {ask}"
    );
    assert_eq!(
        properties["q1"]["items"]["anyOf"][0]["const"], "cache",
        "{ask}"
    );
    assert_eq!(properties.as_object().map(|held| held.len()), Some(2));

    let request = ask["id"].as_u64().expect("a request id");
    acp.send(json!({
        "jsonrpc": "2.0",
        "id": request,
        "result": {"action": "accept", "content": {"q0": "postgres", "q1": ["cache"]}},
    }))
    .await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let answer = lines.last().expect("an answer");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    let output = tool_output(&lines);
    assert!(output.contains("A: postgres"), "{output}");
    assert!(output.contains("A: cache"), "{output}");
}

/// A question set is one permission request when the editor advertises no
/// elicitation, which is what this agent did before the form.
#[tokio::test]
async fn acp_question_without_elicitation_is_a_permission_request() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"calls":[{"name":"ask","arguments":{"questions":[{"q":"Which database?","options":[{"label":"sqlite"},{"label":"postgres"}]}]}}]},{"text":"done"}]}"#,
    )
    .await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let id = acp.prompt(&session, "ask me").await;
    let asked = acp
        .until(|line| line["method"] == "session/request_permission")
        .await;
    let ask = asked.last().expect("a permission request");
    assert_eq!(ask["params"]["options"][1]["name"], "postgres", "{ask}");
    let request = ask["id"].as_u64().expect("a request id");
    acp.send(json!({
        "jsonrpc": "2.0",
        "id": request,
        "result": {"outcome": {"outcome": "selected", "optionId": "1"}},
    }))
    .await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let answer = lines.last().expect("an answer");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert!(
        tool_output(&lines).contains("A: postgres"),
        "the answer reached the worker: {:?}",
        tool_output(&lines)
    );
}

/// A prompt of only an image names no title: the `@"path"` word that attaches
/// it is no title. The next prompt with words names it.
#[tokio::test]
async fn acp_an_image_only_prompt_names_no_title() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"text":"seen"},{"text":"ok"}]}"#,
    )
    .await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    let id = acp
        .prompt_blocks(
            &session,
            json!([{"type": "image", "data": shot(), "mimeType": "image/png"}]),
        )
        .await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let titles: Vec<&Value> = updates(&lines)
        .into_iter()
        .filter(|update| update["sessionUpdate"] == "session_info_update")
        .collect();
    assert!(titles.is_empty(), "no title from an attachment: {titles:?}");

    let id = acp
        .prompt_blocks(&session, json!([{"type": "text", "text": "explain it"}]))
        .await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let titles: Vec<&Value> = updates(&lines)
        .into_iter()
        .filter(|update| update["sessionUpdate"] == "session_info_update")
        .collect();
    assert_eq!(titles.len(), 1, "{lines:?}");
    assert_eq!(titles[0]["title"], "explain it", "{titles:?}");
}

/// An image prompt whose line is over the 1 MiB the worker socket takes is
/// still read and answered.
#[tokio::test]
async fn acp_a_large_image_prompt_is_read_and_answered() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(state.path(), r#"{"replies":[{"text":"seen"}]}"#).await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize().await;
    let session = acp.new_session(&cwd).await;

    // The fixture, and bytes after its last chunk that a decoder ignores.
    let mut bytes = SHOT.to_vec();
    bytes.resize(2 * 1024 * 1024, 0);
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let id = acp
        .prompt_blocks(
            &session,
            json!([{"type": "image", "data": data, "mimeType": "image/png"}]),
        )
        .await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let answer = lines.last().expect("an answer");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
}

/// A line over the cap of the reader is answered with an invalid request and a
/// null id, and the next line is read.
#[tokio::test]
async fn acp_a_line_over_the_cap_is_answered_invalid_request() {
    let state = state();
    let cwd = work(state.path()).await;
    let mut acp = Acp::start(state.path(), &cwd, &[]).await;
    acp.initialize().await;

    let text = "a".repeat(29 * 1024 * 1024);
    acp.send(
        json!({"jsonrpc": "2.0", "id": 99, "method": "session/prompt", "params": {"text": text}}),
    )
    .await;
    let answer = acp.line().await;
    assert_eq!(answer["error"]["code"], -32600, "{answer}");
    assert_eq!(answer["id"], Value::Null, "{answer}");

    let id = acp
        .request("authenticate", json!({"methodId": "none"}))
        .await;
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    assert_eq!(lines.last().expect("an answer")["result"], json!({}));
}

/// A turn that is cancelled while a form waits for the editor takes the form
/// back: the editor hears `$/cancel_request` for it.
#[tokio::test]
async fn acp_cancel_takes_back_the_open_form() {
    let state = state();
    let cwd = work(state.path()).await;
    let provider = script(
        state.path(),
        r#"{"replies":[{"calls":[{"name":"ask","arguments":{"questions":[{"q":"Which database?","options":[{"label":"sqlite"},{"label":"postgres"}]}]}}]},{"text":"done"}]}"#,
    )
    .await;
    let mut acp = Acp::start(state.path(), &cwd, &["--provider", provider.as_str()]).await;
    acp.initialize_with(json!({"elicitation": {"form": {}}}))
        .await;
    let session = acp.new_session(&cwd).await;

    let id = acp.prompt(&session, "ask me").await;
    let asked = acp
        .until(|line| line["method"] == "elicitation/create")
        .await;
    let request = asked.last().expect("an elicitation request")["id"].clone();

    acp.send(json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": {"sessionId": session},
    }))
    .await;
    let lines = acp.until(|line| line["method"] == "$/cancel_request").await;
    let cancel = lines.last().expect("a cancel request");
    assert_eq!(cancel["params"]["requestId"], request, "{cancel}");
    let lines = acp.until(|line| line["id"].as_u64() == Some(id)).await;
    let answer = lines.last().expect("an answer");
    assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
}
