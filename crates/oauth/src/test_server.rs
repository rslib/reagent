//! A one-request HTTP server the tests point a client at.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// One request the test server read, head and body.
#[derive(Debug)]
pub struct Request {
    /// The request line, for example `POST /api/v1/auth/keys HTTP/1.1`.
    pub request_line: String,
    /// The body, as text.
    pub body: String,
}

/// Serves one request, answering `status` with `reply`.
///
/// Returns the base URL and a receiver for the recorded request.
pub async fn serve(status: u16, reply: &'static str) -> (String, oneshot::Receiver<Request>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a port");
    let address = listener.local_addr().expect("the bound address");
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept a connection");
        let request = read_request(&mut socket).await;
        let _ = sender.send(request);
        let response = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
            reply.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    });
    (format!("http://{address}"), receiver)
}

/// The end of the request head, past the blank line.
fn head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// The `content-length` of a request head, or 0.
fn content_length(head: &str) -> usize {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0)
}

/// Reads one request off `socket`, head and body.
async fn read_request(socket: &mut TcpStream) -> Request {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let end = head_end(&buffer);
        if let Some(end) = end {
            let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
            if buffer.len() >= end + 4 + content_length(&head) {
                break;
            }
        }
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
    let text = String::from_utf8_lossy(&buffer).into_owned();
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some((head, body)) => (head, body),
        None => (text.as_str(), ""),
    };
    let request_line = head.lines().next().unwrap_or_default().to_owned();
    Request {
        request_line,
        body: body.to_owned(),
    }
}
