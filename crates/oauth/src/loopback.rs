//! The browser loopback callback a login waits on: a listener on this machine,
//! one request with the code in it, or a URL the user pastes instead.

use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::callback_query;

/// How long one callback connection may take to send its request. A browser
/// opens idle connections in advance, so a silent one must not hold the port.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The most of a callback request the login reads.
const REQUEST_LIMIT: usize = 8 * 1024;

/// Opens `url` in the browser, ignoring every failure: the URL is printed, so
/// a paste still works.
#[expect(
    clippy::disallowed_types,
    reason = "the process feature of tokio is not built in; the opener is a fire-and-forget spawn"
)]
pub fn opener(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// The `code` of the callback request, or of the pasted URL, whichever is
/// first.
pub async fn wait_for_code(
    listener: Option<&TcpListener>,
    path: &str,
    input: impl AsyncBufRead + Unpin + Send + 'static,
) -> Result<String, String> {
    let query = wait_for_query(listener, path, false, None, input).await?;
    field(&query, "code")
        .map(str::to_string)
        .ok_or_else(|| "the login callback had no code".to_string())
}

/// The query of the callback request, or of the pasted URL, whichever is
/// first.
///
/// `keep_errors` accepts a callback that carries `error` instead of `/code`,
/// which is how a ChatGPT login is denied. `state`, when given, is the value
/// the callback must echo: a request that names another one is answered `400`
/// and the wait goes on, so a page on this machine cannot end the login with a
/// code of its own. A listener of `None` waits on the paste alone: the port
/// could not be bound.
pub async fn wait_for_query(
    listener: Option<&TcpListener>,
    path: &str,
    keep_errors: bool,
    state: Option<&str>,
    input: impl AsyncBufRead + Unpin + Send + 'static,
) -> Result<Vec<(String, String)>, String> {
    let (sender, mut queries) = mpsc::unbounded_channel();
    let mut lines = Some(line_channel(input));
    loop {
        tokio::select! {
            socket = accepted(listener) => {
                let mut socket = socket?;
                // Each connection is read in its own task with its own
                // timeout, so an idle preconnect or a half-sent request of
                // the browser cannot hold up the next connection or a paste.
                let sender = sender.clone();
                let path = path.to_string();
                let state = state.map(str::to_string);
                tokio::spawn(async move {
                    let query =
                        callback(&mut socket, &path, keep_errors, state.as_deref()).await;
                    if let Some(query) = query {
                        let _ = sender.send(query);
                    }
                });
            }
            Some(query) = queries.recv() => return Ok(query),
            line = pasted(&mut lines) => match line {
                Some(line) => {
                    // A paste is the user's own input, so a mismatched state
                    // is reported at once by the caller's check instead of
                    // being ignored as a callback request is.
                    if let Some(query) = callback_query(line.trim(), path)
                        && carries(&query, keep_errors)
                    {
                        return Ok(query);
                    }
                }
                None => lines = None,
            },
        }
    }
}

/// The next callback connection, or never when there is no listener.
async fn accepted(listener: Option<&TcpListener>) -> Result<TcpStream, String> {
    match listener {
        Some(listener) => listener
            .accept()
            .await
            .map(|(socket, _)| socket)
            .map_err(|error| format!("cannot accept the login callback: {error}")),
        None => std::future::pending().await,
    }
}

/// Whether `query` carries what the login waits for.
fn carries(query: &[(String, String)], keep_errors: bool) -> bool {
    field(query, "code").is_some() || (keep_errors && field(query, "error").is_some())
}

/// The non-empty value of `name` in `query`.
pub fn field<'a>(query: &'a [(String, String)], name: &str) -> Option<&'a str> {
    query
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
}

/// The next pasted line; never finishes once the input is closed.
async fn pasted(lines: &mut Option<mpsc::UnboundedReceiver<String>>) -> Option<String> {
    match lines {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// Reads the lines of `input` in a task, so a select never loses half a line.
fn line_channel(
    input: impl AsyncBufRead + Unpin + Send + 'static,
) -> mpsc::UnboundedReceiver<String> {
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = input.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}

/// Whether `query` echoes the `state` the login sent.
///
/// A page on this machine can reach the callback port, so a request that names
/// another state is refused there and the login keeps waiting for the real
/// one.
fn echoes_state(query: &[(String, String)], state: Option<&str>) -> bool {
    match state {
        Some(state) => field(query, "state") == Some(state),
        None => true,
    }
}

/// Reads one callback request and answers it with the query it carried.
async fn callback(
    socket: &mut TcpStream,
    path: &str,
    keep_errors: bool,
    state: Option<&str>,
) -> Option<Vec<(String, String)>> {
    let Ok(target) = tokio::time::timeout(READ_TIMEOUT, read_target(socket)).await else {
        // A preconnect that sends nothing: leave the port for the next one.
        return None;
    };
    match callback_query(&target.ok()?, path) {
        Some(query) if carries(&query, keep_errors) && echoes_state(&query, state) => {
            reply(
                socket,
                "200 OK",
                "reagent: login done, you can close this tab.",
            )
            .await;
            Some(query)
        }
        _ => {
            reply(socket, "400 Bad Request", "reagent: no login code there.").await;
            None
        }
    }
}

/// The request target of one request off `socket`.
async fn read_target(socket: &mut TcpStream) -> Result<String, String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") || buffer.len() >= REQUEST_LIMIT {
            break;
        }
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
    let text = String::from_utf8_lossy(&buffer);
    let request_line = text.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    parts.next();
    Ok(parts.next().unwrap_or_default().to_string())
}

/// Answers one callback request and closes it.
async fn reply(socket: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}
