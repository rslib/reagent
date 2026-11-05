//! End-to-end tests for `@path` images and for images in `--json`.
//!
//! Each test runs the binary against a fake Anthropic server on a local port
//! and a temp config, state and cache folder, so no test writes into the real
//! ones. The server records each request body: that is where the image has to
//! appear, byte for byte.

use std::path::PathBuf;
use std::process::{Output, Stdio};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::mpsc;

const BIN: &str = env!("CARGO_BIN_EXE_reagent");

/// A reply that ends the turn with one short text.
const REPLY: &str = concat!(
    "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n"
);

/// A reply that calls `read` on `path` and ends the turn with the tool use.
fn read_reply(path: &str) -> String {
    let arguments = serde_json::json!({ "path": path }).to_string();
    format!(
        concat!(
            "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"read\"}}}}\n\n",
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{}}}}}\n\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"output_tokens\":1}}}}\n\n",
            "data: {{\"type\":\"message_stop\"}}\n\n"
        ),
        serde_json::to_string(&arguments).expect("a JSON string")
    )
}

/// A 64x48 RGB PNG, made here so the test holds the exact bytes it sends.
fn shot_png() -> Vec<u8> {
    let (width, height) = (64u32, 48u32);
    let data: Vec<u8> = (0..height)
        .flat_map(|y| (0..width).flat_map(move |x| [(x * 4) as u8, (y * 5) as u8, 90]))
        .collect();
    let mut out = Vec::new();
    {
        let mut info = png::Info::with_size(width, height);
        info.color_type = png::ColorType::Rgb;
        info.bit_depth = png::BitDepth::Eight;
        let mut encoder = png::Encoder::with_info(&mut out, info).expect("png info");
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&data).expect("png data");
        writer.finish().expect("png finish");
    }
    out
}

/// A temp home the command writes its config and state in, and runs from.
struct Home {
    root: TempDir,
}

impl Home {
    fn new() -> Home {
        Home {
            root: tempfile::tempdir().expect("a temp dir"),
        }
    }

    /// Writes a file in the working folder and returns its path.
    async fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.path().join(name);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.expect("a folder");
        }
        tokio::fs::write(&path, bytes).await.expect("a file");
        path
    }

    async fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .current_dir(self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("ANTHROPIC_API_KEY", "test-key")
            .env_remove("REAGENT_PROFILE")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .expect("runs reagent")
    }
}

/// The text of an output stream.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A fake Anthropic server: it answers `replies` in order and records the body
/// of each request it gets.
struct Server {
    base: String,
    bodies: mpsc::UnboundedReceiver<String>,
}

/// Starts a server that answers `replies` in order.
async fn serve(replies: Vec<String>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let base = format!("http://{}", listener.local_addr().expect("the address"));
    let (sender, bodies) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        for reply in replies {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let Some(body) = read_body(&mut socket).await else {
                return;
            };
            let _ = sender.send(body);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(reply.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    Server { base, bodies }
}

impl Server {
    /// The next recorded request body.
    async fn body(&mut self) -> Value {
        let body = self.bodies.recv().await.expect("a recorded request");
        serde_json::from_str(&body).expect("a JSON body")
    }

    /// Whether no request arrived, without waiting for one.
    fn idle(&mut self) -> bool {
        self.bodies.try_recv().is_err()
    }
}

/// Reads one HTTP request and returns its body.
async fn read_body(socket: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.trim() == "content-length")
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buffer.len() >= end + 4 + length {
                return Some(
                    String::from_utf8_lossy(&buffer[end + 4..end + 4 + length]).into_owned(),
                );
            }
        }
        let read = socket.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// The content blocks of the first user message of a request body.
fn first_user_blocks(body: &Value) -> Vec<Value> {
    let content = &body["messages"][0]["content"];
    match content {
        Value::Array(blocks) => blocks.clone(),
        Value::String(text) => vec![serde_json::json!({ "type": "text", "text": text })],
        other => panic!("no first user message: {other}"),
    }
}

#[tokio::test]
async fn at_path_sends_the_png() {
    let home = Home::new();
    let png = shot_png();
    home.write("shot.png", &png).await;
    let mut server = serve(vec![REPLY.to_string()]).await;
    let out = home
        .run(&[
            "--provider",
            "anthropic",
            "--base-url",
            &server.base,
            "-p",
            "why is this cut off? @shot.png",
        ])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let body = server.body().await;
    let blocks = first_user_blocks(&body);
    // The CLI appends its environment block; the task text itself is unchanged.
    assert!(
        blocks[0]["text"]
            .as_str()
            .expect("text")
            .starts_with("why is this cut off? @shot.png"),
        "{}",
        blocks[0]["text"]
    );
    assert_eq!(blocks[1]["text"], "[image 64x48]");
    assert_eq!(blocks[2]["type"], "image");
    assert_eq!(blocks[2]["source"]["media_type"], "image/png");
    assert_eq!(blocks[2]["source"]["data"], STANDARD.encode(&png));
}

#[tokio::test]
async fn a_quoted_at_path_sends_the_png() {
    let home = Home::new();
    let png = shot_png();
    home.write("my shot.png", &png).await;
    let mut server = serve(vec![REPLY.to_string()]).await;
    let out = home
        .run(&[
            "--provider",
            "anthropic",
            "--base-url",
            &server.base,
            "-p",
            "look at @\"my shot.png\"",
        ])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let body = server.body().await;
    let blocks = first_user_blocks(&body);
    assert!(
        blocks[0]["text"]
            .as_str()
            .expect("text")
            .starts_with("look at @\"my shot.png\""),
        "{}",
        blocks[0]["text"]
    );
    assert_eq!(blocks[1]["text"], "[image 64x48]");
    assert_eq!(blocks[2]["source"]["data"], STANDARD.encode(&png));
}

#[tokio::test]
async fn a_text_file_sends_no_image() {
    let home = Home::new();
    home.write("notes.txt", b"notes\n").await;
    let mut server = serve(vec![REPLY.to_string()]).await;
    let out = home
        .run(&[
            "--provider",
            "anthropic",
            "--base-url",
            &server.base,
            "-p",
            "mail me@host.com about @notes.txt",
        ])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let body = server.body().await;
    let blocks = first_user_blocks(&body);
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert!(
        blocks[0]["text"]
            .as_str()
            .expect("text")
            .starts_with("mail me@host.com about @notes.txt"),
        "{}",
        blocks[0]["text"]
    );
}

#[tokio::test]
async fn a_broken_image_exits_one_before_any_request() {
    let home = Home::new();
    // A PNG header and nothing decodable after it.
    let mut broken = b"\x89PNG\r\n\x1a\n".to_vec();
    broken.extend_from_slice(&[0_u8; 24]);
    home.write("bad.png", &broken).await;
    let mut server = serve(vec![REPLY.to_string()]).await;
    let out = home
        .run(&[
            "--provider",
            "anthropic",
            "--base-url",
            &server.base,
            "-p",
            "look at @bad.png",
        ])
        .await;
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("bad.png"), "{stderr}");
    assert!(server.idle(), "no request may be sent");
}

#[tokio::test]
async fn a_continued_run_sends_the_read_png_again() {
    let home = Home::new();
    let png = shot_png();
    home.write("shot.png", &png).await;
    let mut first = serve(vec![read_reply("shot.png"), REPLY.to_string()]).await;
    let out = home
        .run(&[
            "--provider",
            "anthropic",
            "--base-url",
            &first.base,
            "-p",
            "what is in shot.png?",
        ])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    // The tool result of the read holds the PNG on the wire.
    let _tool_use = first.body().await;
    let second = first.body().await;
    assert!(
        second.to_string().contains(&STANDARD.encode(&png)),
        "the read result carries the image"
    );
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let mut next = serve(vec![REPLY.to_string()]).await;
    let out = home
        .run(&[
            "-c",
            "--provider",
            "anthropic",
            "--base-url",
            &next.base,
            "-p",
            "next",
        ])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let body = next.body().await;
    assert!(
        body.to_string().contains(&STANDARD.encode(&png)),
        "the continued run sends the same PNG byte for byte"
    );
}

#[tokio::test]
async fn a_continued_run_sends_the_attached_png_again() {
    let home = Home::new();
    let png = shot_png();
    home.write("shot.png", &png).await;
    let first = serve(vec![REPLY.to_string()]).await;
    let out = home
        .run(&[
            "--provider",
            "anthropic",
            "--base-url",
            &first.base,
            "-p",
            "why is this cut off? @shot.png",
        ])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    // The run has exited, so its session is closed: no wait is needed.
    let mut next = serve(vec![REPLY.to_string()]).await;
    let out = home
        .run(&[
            "-c",
            "--provider",
            "anthropic",
            "--base-url",
            &next.base,
            "-p",
            "next",
        ])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let body = next.body().await;
    let blocks = first_user_blocks(&body);
    assert_eq!(blocks[1]["text"], "[image 64x48]");
    assert_eq!(blocks[2]["type"], "image");
    assert_eq!(blocks[2]["source"]["data"], STANDARD.encode(&png));
}

#[tokio::test]
async fn json_shows_the_image_without_its_base64() {
    let home = Home::new();
    let png = shot_png();
    home.write("shot.png", &png).await;
    let server = serve(vec![read_reply("shot.png"), REPLY.to_string()]).await;
    let out = home
        .run(&[
            "--json",
            "--provider",
            "anthropic",
            "--base-url",
            &server.base,
            "-p",
            "what is in shot.png?",
        ])
        .await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    let line = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| line["type"] == "tool_result")
        .unwrap_or_else(|| panic!("no tool result line in {stdout}"));
    assert_eq!(line["images"][0]["media_type"], "image/png");
    assert_eq!(line["images"][0]["width"], 64);
    assert_eq!(line["images"][0]["height"], 48);
    assert_eq!(line["images"][0]["bytes"], png.len());
    assert!(
        !stdout.contains(&STANDARD.encode(&png)),
        "the base64 text is never printed"
    );
}
