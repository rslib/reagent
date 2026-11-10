//! The requests a run takes on its standard input instead of a socket.

use std::path::Path;

use reagent_agent::live;
use reagent_agent::live::Sender;
use reagent_protocol::message;
use tokio::io::AsyncRead;
use tokio::sync::mpsc;

use super::LINE;
use super::Read;
use super::Reader;
use super::Shared;
use super::handle;

/// Answers the requests a run reads on `input`, as the client `client` of
/// `shared`.
///
/// One request per line: `send`, `cancel`, `approve`, `answer`, `set` and
/// `compact` go to the session, and the answer to each goes on `answers` as one
/// line ending in a newline. `initialize`, `attach` and `detach` are refused,
/// since a run that reads its requests on stdin has no handshake and no
/// connection, and a line that is not JSON gets the error of the decoder.
///
/// The task ends at the end of `input` and when `answers` is closed, and never
/// ends the run: what the end of the input means for the run is the caller's
/// to decide.
pub(crate) async fn serve<R: AsyncRead + Unpin>(
    mut input: R,
    answers: mpsc::UnboundedSender<Vec<u8>>,
    shared: &Shared,
    sender: Option<&Sender>,
    client: live::ClientId,
    cwd: &Path,
) {
    let mut reader = Reader::new();
    let mut out = Vec::with_capacity(LINE);
    loop {
        let next = tokio::select! {
            next = reader.fetch(&mut input) => next,
            // The run stopped reading the answers: nothing is left to say.
            () = answers.closed() => return,
        };
        let line = match next {
            Ok(Read::Line(line)) => line,
            Ok(Read::More) => continue,
            Ok(Read::End) | Err(_) => return,
        };
        let (id, request) = match message::decode(line) {
            Ok(decoded) => decoded,
            Err(error) => {
                out.clear();
                message::error(&mut out, None, &error);
                if answers.send(out.clone()).is_err() {
                    return;
                }
                continue;
            }
        };
        handle(shared, sender, client, cwd, id, request, &mut out).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use reagent_agent::live::Command;
    use reagent_agent::live::SendMode;

    use super::*;
    use crate::worker::Model;
    use crate::worker::commands;

    /// A session with no socket, its command channel and the commands that
    /// reach it.
    fn session() -> (Arc<Shared>, Sender, mpsc::Receiver<Command>) {
        let shared = Shared::new(Model::new(Arc::from("m"), None, None, None, None));
        let (sender, commands) = mpsc::channel(live::COMMANDS);
        (Arc::new(shared), sender, commands)
    }

    /// The answer lines of one `serve` run over `input`, and the command that
    /// reached the session.
    async fn answer(input: &[u8]) -> (Vec<String>, Option<Command>) {
        let (shared, sender, mut commands) = session();
        let (answers, mut lines) = mpsc::unbounded_channel();
        let client = shared.steering(answers.clone());
        serve(
            input,
            answers,
            &shared,
            Some(&sender),
            client,
            Path::new("."),
        )
        .await;
        let mut out = Vec::new();
        while let Ok(line) = lines.try_recv() {
            match String::from_utf8(line) {
                Ok(line) => out.push(line),
                Err(error) => panic!("an answer that is not text: {error}"),
            }
        }
        (out, commands.try_recv().ok())
    }

    #[tokio::test]
    async fn steer_stdin_send_reaches_the_session_and_is_answered() {
        let (answers, command) = answer(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"send","params":{"text":"go","mode":"steer"}}"#,
                "\n"
            )
            .as_bytes(),
        )
        .await;
        assert_eq!(answers.len(), 1, "{answers:?}");
        assert!(answers[0].contains("\"id\":1"), "{}", answers[0]);
        assert!(answers[0].ends_with('\n'), "{}", answers[0]);
        match command {
            Some(Command::Send { mode, text, .. }) => {
                assert_eq!(mode, SendMode::Steer);
                assert_eq!(&*text, "go");
            }
            other => panic!("the send did not reach the session: {other:?}"),
        }
    }

    #[tokio::test]
    async fn steer_stdin_the_task_answers_a_request_of_its_client() {
        let (shared, sender, mut sent) = session();
        let (answers, mut lines) = mpsc::unbounded_channel();
        let (client, task) = commands(
            &shared,
            Some(sender),
            Path::new("."),
            b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"compact\",\"params\":{}}\n".as_slice(),
            answers,
        );
        task.await.expect("the task ends at the end of its input");
        assert!(client > 0, "the client takes an id of the session");
        let line = lines.try_recv().expect("an answer");
        match String::from_utf8(line) {
            Ok(line) => assert!(line.contains("\"id\":3"), "{line}"),
            Err(error) => panic!("an answer that is not text: {error}"),
        }
        assert!(
            matches!(sent.try_recv(), Ok(Command::Compact { .. })),
            "the compact did not reach the session"
        );
    }

    #[tokio::test]
    async fn steer_stdin_cancel_reaches_the_session_and_is_answered() {
        let (answers, command) =
            answer(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"cancel\"}\n").await;
        assert_eq!(answers.len(), 1, "{answers:?}");
        assert!(answers[0].contains("\"id\":2"), "{}", answers[0]);
        assert!(
            matches!(command, Some(Command::Cancel { .. })),
            "the cancel did not reach the session: {command:?}"
        );
    }

    #[tokio::test]
    async fn steer_stdin_refuses_initialize_and_the_next_line_works() {
        let (answers, command) = answer(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"major":1,"#,
                r#""client":{"name":"a","version":"1"},"capabilities":[],"token":"t"}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"method":"cancel"}"#,
                "\n",
            )
            .as_bytes(),
        )
        .await;
        assert_eq!(answers.len(), 2, "{answers:?}");
        assert!(answers[0].contains("\"id\":1"), "{}", answers[0]);
        assert!(answers[0].contains("\"error\""), "{}", answers[0]);
        assert!(answers[1].contains("\"id\":2"), "{}", answers[1]);
        assert!(answers[1].contains("\"result\""), "{}", answers[1]);
        assert!(
            matches!(command, Some(Command::Cancel { .. })),
            "the cancel did not reach the session: {command:?}"
        );
    }

    #[tokio::test]
    async fn steer_stdin_answers_a_line_that_is_not_json() {
        let (answers, command) = answer(b"not json\n").await;
        assert_eq!(answers.len(), 1, "{answers:?}");
        assert!(answers[0].contains("\"error\""), "{}", answers[0]);
        assert!(answers[0].contains("parse error"), "{}", answers[0]);
        assert!(command.is_none(), "nothing reached the session");
    }

    #[tokio::test]
    async fn steer_stdin_ends_at_the_end_of_its_input() {
        let (answers, command) = answer(b"").await;
        assert!(answers.is_empty(), "{answers:?}");
        assert!(command.is_none(), "nothing reached the session");
    }

    #[tokio::test]
    async fn steer_stdin_ends_when_the_answers_are_gone() {
        let (shared, sender, _commands) = session();
        let (answers, lines) = mpsc::unbounded_channel();
        let client = shared.steering(answers.clone());
        drop(lines);
        // The input stays open: the task ends on the closed channel alone.
        let (mut input, _open) = tokio::io::duplex(64);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            serve(
                &mut input,
                answers,
                &shared,
                Some(&sender),
                client,
                Path::new("."),
            ),
        )
        .await
        .expect("the closed channel ends the task");
    }
}
