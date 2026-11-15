//! The network proxy of a run whose `[sandbox] network` names hosts: the one
//! way out of the sandbox, since no OS sandbox can allow a list of remote
//! hosts by itself. A sandboxed command gets `HTTP_PROXY`, `ALL_PROXY` and
//! `REAGENT_PROXY` pointing here and may reach nothing else, so a command
//! that ignores the variables gets no network at all.
//!
//! It speaks HTTP `CONNECT`, plain HTTP with an absolute URI, and SOCKS5 with
//! no auth. It allows a target only when its `host:port` is one of the
//! entries, and it resolves a name itself at connect time.

use std::io;
use std::net::{IpAddr, Ipv4Addr};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, copy_bidirectional};
#[cfg(target_os = "linux")]
use tokio::net::UnixListener;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use reagent_config::Host;

/// The longest request head the proxy reads.
const HEAD_LIMIT: usize = 8 * 1024;

/// How long a client has to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// The first byte of a SOCKS5 greeting.
const SOCKS5: u8 = 0x05;

/// The most clients the proxy serves at once; a later one waits for a free
/// slot.
const MAX_CLIENTS: usize = 128;

/// The pause after a failed accept, such as one with no file descriptor left.
const ACCEPT_RETRY: Duration = Duration::from_millis(50);

const BAD_REQUEST: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
const TOO_LARGE: &[u8] =
    b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
const BAD_GATEWAY: &[u8] =
    b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";

/// A running proxy: one listener on a random port of `127.0.0.1`, and on Linux
/// one more on a unix socket in a folder of its own, both ended when the run
/// drops this.
pub struct Proxy {
    port: u16,
    tasks: Vec<JoinHandle<()>>,
    /// The unix socket of a Linux run. The bridge of a network namespace
    /// connects to it, so the proxy is reached from inside a namespace that has
    /// only loopback in it. The folder goes with the proxy.
    #[cfg(target_os = "linux")]
    unix: Option<PathBuf>,
}

impl Proxy {
    /// Starts the proxy for `hosts`, before the first command runs.
    pub async fn start(hosts: &[Host]) -> io::Result<Proxy> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let hosts: Arc<[Host]> = hosts.into();
        // At most this many clients at once, so a command that opens and
        // holds connections cannot take every file descriptor of the run.
        let open = Arc::new(Semaphore::new(MAX_CLIENTS));
        let mut tasks = Vec::with_capacity(2);
        #[cfg(target_os = "linux")]
        let unix = {
            let listener = reagent_sandbox::proxy_socket(port).map_err(io::Error::other)?;
            let listener = UnixListener::from_std(listener)?;
            tasks.push(tokio::spawn(serve_unix(
                listener,
                hosts.clone(),
                open.clone(),
            )));
            Some(reagent_sandbox::proxy_socket_path(port))
        };
        tasks.push(tokio::spawn(serve_tcp(listener, hosts, open)));
        Ok(Proxy {
            port,
            tasks,
            #[cfg(target_os = "linux")]
            unix,
        })
    }

    /// The port the proxy listens on, on `127.0.0.1`.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        #[cfg(target_os = "linux")]
        if self.unix.is_some() {
            reagent_sandbox::remove_proxy_socket(self.port);
        }
    }
}

/// Serves the proxy's TCP listener, one client at a time from a slot of `open`,
/// each on its own task.
async fn serve_tcp(listener: TcpListener, hosts: Arc<[Host]>, open: Arc<Semaphore>) {
    loop {
        let Ok(permit) = Arc::clone(&open).acquire_owned().await else {
            return;
        };
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            // Out of descriptors, or a client that left before it was
            // taken: the listener stays and tries again a bit later.
            Err(_) => {
                drop(permit);
                tokio::time::sleep(ACCEPT_RETRY).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let hosts = Arc::clone(&hosts);
        tokio::spawn(async move {
            // A refused or dropped client is not the run's business: nothing
            // is printed for it.
            let _ = handle(stream, &hosts).await;
            drop(permit);
        });
    }
}

/// Serves the proxy's unix listener, which only the bridge of a Linux network
/// namespace connects to, with the same clients and the same limits as the TCP
/// one.
#[cfg(target_os = "linux")]
async fn serve_unix(listener: UnixListener, hosts: Arc<[Host]>, open: Arc<Semaphore>) {
    loop {
        let Ok(permit) = Arc::clone(&open).acquire_owned().await else {
            return;
        };
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(_) => {
                drop(permit);
                tokio::time::sleep(ACCEPT_RETRY).await;
                continue;
            }
        };
        let hosts = Arc::clone(&hosts);
        tokio::spawn(async move {
            let _ = handle(stream, &hosts).await;
            drop(permit);
        });
    }
}

/// Serves one client, over TCP or a unix socket: a first byte of `0x05` is
/// SOCKS5, anything else is HTTP.
async fn handle<S>(mut client: S, hosts: &[Host]) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut first = [0u8; 1];
    match timeout(HEAD_TIMEOUT, client.read(&mut first)).await {
        Ok(Ok(0)) | Err(_) => return Ok(()),
        Ok(Ok(_)) => {}
        Ok(Err(error)) => return Err(error),
    }
    if first[0] == SOCKS5 {
        socks5(client, hosts).await
    } else {
        http(client, first[0], hosts).await
    }
}

/// One HTTP request: `CONNECT host:port` opens a tunnel and an absolute URI is
/// forwarded in origin form and its reply copied back.
async fn http<S>(mut client: S, first: u8, hosts: &[Host]) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (buf, head_len) = match timeout(HEAD_TIMEOUT, read_head(&mut client, first)).await {
        Ok(Ok(read)) => read,
        Ok(Err(error)) if error.kind() == io::ErrorKind::InvalidData => {
            return reply(&mut client, TOO_LARGE).await;
        }
        Ok(Err(_)) | Err(_) => return reply(&mut client, BAD_REQUEST).await,
    };
    // A client may send bytes past the head in the same read: a body, or the
    // first bytes of a tunnel. They go on to the server after the head.
    let (head, rest) = buf.split_at(head_len);
    let end = request_line_end(head);
    let Ok(line) = std::str::from_utf8(&head[..end]) else {
        return reply(&mut client, BAD_REQUEST).await;
    };
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return reply(&mut client, BAD_REQUEST).await;
    };

    if method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = authority(target) else {
            return reply(&mut client, BAD_REQUEST).await;
        };
        if !allowed(hosts, host, port) {
            return refuse(&mut client, host, port).await;
        }
        let mut upstream = match TcpStream::connect((host, port)).await {
            Ok(upstream) => upstream,
            Err(_) => return reply(&mut client, BAD_GATEWAY).await,
        };
        let _ = upstream.set_nodelay(true);
        client
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await?;
        if !rest.is_empty() {
            upstream.write_all(rest).await?;
        }
        copy_bidirectional(&mut client, &mut upstream).await?;
        return Ok(());
    }

    let Some((host, port, path)) = absolute(target) else {
        return reply(&mut client, BAD_REQUEST).await;
    };
    if !allowed(hosts, host, port) {
        return refuse(&mut client, host, port).await;
    }
    let mut upstream = match TcpStream::connect((host, port)).await {
        Ok(upstream) => upstream,
        Err(_) => return reply(&mut client, BAD_GATEWAY).await,
    };
    let mut origin = origin_form(method, path, version, &head[end + 2..head.len() - 2]);
    origin.extend_from_slice(rest);
    upstream.write_all(&origin).await?;
    copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// One SOCKS5 client: no auth, the CONNECT command, a domain or an address.
/// The version byte is already read, as the one that named the protocol.
async fn socks5<S>(mut client: S, hosts: &[Host]) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut count = [0u8; 1];
    read_exact(&mut client, &mut count).await?;
    let count = count[0] as usize;
    let mut methods = [0u8; 255];
    read_exact(&mut client, &mut methods[..count]).await?;
    if !methods[..count].contains(&0) {
        return socks_reply(&mut client, 0xff).await;
    }
    client.write_all(&[SOCKS5, 0x00]).await?;

    let mut request = [0u8; 4];
    read_exact(&mut client, &mut request).await?;
    if request[0] != SOCKS5 {
        return Ok(());
    }
    if request[1] != 0x01 {
        return socks_reply(&mut client, 0x07).await;
    }
    // An address entry is named by its text, without the brackets of IPv6, so
    // a listed IP literal is compared as an address and never as a name.
    let mut octets = [0u8; 16];
    let mut name = [0u8; 255];
    let host = match request[3] {
        0x01 => {
            read_exact(&mut client, &mut octets[..4]).await?;
            IpAddr::from([octets[0], octets[1], octets[2], octets[3]]).to_string()
        }
        0x04 => {
            read_exact(&mut client, &mut octets).await?;
            IpAddr::from(octets).to_string()
        }
        0x03 => {
            let mut length = [0u8; 1];
            read_exact(&mut client, &mut length).await?;
            let length = length[0] as usize;
            read_exact(&mut client, &mut name[..length]).await?;
            match std::str::from_utf8(&name[..length]) {
                Ok(name) => name.to_string(),
                Err(_) => return socks_reply(&mut client, 0x08).await,
            }
        }
        _ => return socks_reply(&mut client, 0x08).await,
    };
    let mut port = [0u8; 2];
    read_exact(&mut client, &mut port).await?;
    let port = u16::from_be_bytes(port);
    if !allowed(hosts, &host, port) {
        return socks_reply(&mut client, 0x02).await;
    }
    let mut upstream = match TcpStream::connect((host.as_str(), port)).await {
        Ok(upstream) => upstream,
        Err(_) => return socks_reply(&mut client, 0x05).await,
    };
    // The bound address is reported as `0.0.0.0:0`: a client needs the code
    // only.
    client
        .write_all(&[SOCKS5, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;
    copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// Answers a SOCKS5 client with `code` and no address.
async fn socks_reply<S>(client: &mut S, code: u8) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    client
        .write_all(&[SOCKS5, code, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await
}

/// Writes one HTTP refusal and leaves the body empty, which carries the code
/// alone.
async fn reply<S>(client: &mut S, text: &[u8]) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    client.write_all(text).await
}

/// Answers a target that is not in the list, and says which one in the body,
/// so a person reading a command's output sees the entry the config needs.
async fn refuse<S>(client: &mut S, host: &str, port: u16) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let target = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let body = format!("reagent sandbox: {target} is not in sandbox.network");
    let head = format!(
        concat!(
            "HTTP/1.1 403 Forbidden\r\n",
            "Connection: close\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "Content-Length: {}\r\n\r\n",
        ),
        body.len()
    );
    client.write_all(head.as_bytes()).await?;
    client.write_all(body.as_bytes()).await
}

/// Reads exactly `buf` within [`HEAD_TIMEOUT`].
async fn read_exact<S>(client: &mut S, buf: &mut [u8]) -> io::Result<()>
where
    S: AsyncRead + Unpin,
{
    match timeout(HEAD_TIMEOUT, client.read_exact(buf)).await {
        Ok(result) => result.map(|_| ()),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the request head took over 10 s",
        )),
    }
}

/// Reads the request head, up to the empty line, [`HEAD_LIMIT`] bytes at
/// most, and gives the bytes read with the length of the head in them: the
/// bytes after it came in the same read. `first` is the byte that named the
/// protocol.
async fn read_head<S>(client: &mut S, first: u8) -> io::Result<(Vec<u8>, usize)>
where
    S: AsyncRead + Unpin,
{
    let mut head = Vec::with_capacity(512);
    head.push(first);
    let mut chunk = [0u8; 1024];
    let mut scanned = 0;
    loop {
        // Only the new bytes, and the three before them, can finish the head.
        if let Some(at) = head[scanned..]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            let end = scanned + at + 4;
            if end > HEAD_LIMIT {
                break;
            }
            return Ok((head, end));
        }
        scanned = head.len().saturating_sub(3);
        if head.len() > HEAD_LIMIT {
            break;
        }
        let read = client.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the request head ended early",
            ));
        }
        head.extend_from_slice(&chunk[..read]);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "the request head is over 8 KiB",
    ))
}

/// The end of the request line, up to its carriage return.
fn request_line_end(head: &[u8]) -> usize {
    head.windows(2)
        .position(|window| window == b"\r\n")
        .unwrap_or(head.len())
}

/// `host:port`, with an IPv6 host in brackets.
fn authority(text: &str) -> Option<(&str, u16)> {
    let (host, port) = match text.strip_prefix('[') {
        Some(rest) => {
            let end = rest.find(']')?;
            (&rest[..end], rest[end + 1..].strip_prefix(':')?)
        }
        None => text.rsplit_once(':')?,
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port.parse().ok()?))
}

/// `http://host:port/path` as its host, its port (80 when the URI has none)
/// and the path it asks for.
fn absolute(uri: &str) -> Option<(&str, u16, &str)> {
    if uri.len() < 7 || !uri.as_bytes()[..7].eq_ignore_ascii_case(b"http://") {
        return None;
    }
    let rest = &uri[7..];
    let (target, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    // A URI without a port means port 80.
    let has_port = match target.strip_prefix('[') {
        Some(rest) => rest.find(']').is_some_and(|end| rest.len() > end + 1),
        None => target.contains(':'),
    };
    if !has_port {
        let host = target
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(target);
        return (!host.is_empty()).then_some((host, 80, path));
    }
    let (host, port) = authority(target)?;
    Some((host, port, path))
}

/// The request in origin form, with `Connection: close` and no other
/// connection header.
fn origin_form(method: &str, path: &str, version: &str, headers: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(headers.len() + path.len() + 48);
    out.extend_from_slice(method.as_bytes());
    out.push(b' ');
    out.extend_from_slice(path.as_bytes());
    out.push(b' ');
    out.extend_from_slice(version.as_bytes());
    out.extend_from_slice(b"\r\n");
    for line in headers.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || (line.len() >= 11 && line[..11].eq_ignore_ascii_case(b"connection:"))
        {
            continue;
        }
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out
}

/// Whether `host:port` is one of the entries. The host is compared without
/// case, and an IP literal is compared as an address.
fn allowed(hosts: &[Host], host: &str, port: u16) -> bool {
    let wanted = host.parse::<IpAddr>().ok();
    hosts.iter().any(|entry| {
        entry.port() == port
            && match (wanted, entry.host().parse::<IpAddr>().ok()) {
                (Some(wanted), Some(listed)) => wanted == listed,
                _ => entry.host().eq_ignore_ascii_case(host),
            }
    })
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    /// A local server that answers each connection with the bytes it read
    /// first, then closes it.
    async fn echo() -> SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("binds");
        let addr = listener.local_addr().expect("an address");
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let Ok(read) = stream.read(&mut buf).await else {
                        return;
                    };
                    let _ = stream.write_all(&buf[..read]).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        addr
    }

    /// A proxy for the one entry `host:port`.
    async fn proxy_for(host: &str, port: u16) -> Proxy {
        let entry = Host::parse(&format!("{host}:{port}")).expect("an entry");
        Proxy::start(&[entry]).await.expect("starts")
    }

    /// Reads the whole answer of a proxy to its end.
    async fn answer(client: &mut TcpStream) -> String {
        let mut out = Vec::new();
        let _ = client.read_to_end(&mut out).await;
        String::from_utf8_lossy(&out).into_owned()
    }

    #[tokio::test]
    async fn host_list_proxy_connect_tunnels_bytes_both_ways() {
        let target = echo().await;
        let proxy = proxy_for("127.0.0.1", target.port()).await;
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port()))
            .await
            .expect("connects");
        let request = format!(
            "CONNECT 127.0.0.1:{} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            target.port()
        );
        client.write_all(request.as_bytes()).await.expect("writes");
        let mut established = [0u8; 39];
        client
            .read_exact(&mut established)
            .await
            .expect("the established line");
        assert_eq!(
            &established[..],
            b"HTTP/1.1 200 Connection established\r\n\r\n"
        );
        client.write_all(b"ping").await.expect("writes");
        let mut back = [0u8; 4];
        client.read_exact(&mut back).await.expect("reads the echo");
        assert_eq!(&back, b"ping");
    }

    #[tokio::test]
    async fn host_list_proxy_refuses_a_port_that_is_not_listed() {
        let target = echo().await;
        let proxy = proxy_for("127.0.0.1", target.port()).await;
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port()))
            .await
            .expect("connects");
        client
            .write_all(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("writes");
        let text = answer(&mut client).await;
        assert!(text.starts_with("HTTP/1.1 403 Forbidden"), "{text}");
        assert!(
            text.contains("reagent sandbox: 127.0.0.1:1 is not in sandbox.network"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn host_list_proxy_forwards_an_absolute_uri_in_origin_form() {
        let target = echo().await;
        let proxy = proxy_for("127.0.0.1", target.port()).await;
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port()))
            .await
            .expect("connects");
        let request = format!(
            concat!(
                "GET http://127.0.0.1:{}/path?q=1 HTTP/1.1\r\n",
                "Host: 127.0.0.1:{}\r\n",
                "Connection: keep-alive\r\n\r\n",
            ),
            target.port(),
            target.port()
        );
        client.write_all(request.as_bytes()).await.expect("writes");
        let text = answer(&mut client).await;
        assert!(text.starts_with("GET /path?q=1 HTTP/1.1\r\n"), "{text}");
        assert!(
            text.contains(&format!("Host: 127.0.0.1:{}", target.port())),
            "{text}"
        );
        assert!(text.contains("Connection: close\r\n"), "{text}");
        assert!(!text.contains("keep-alive"), "{text}");
    }

    #[tokio::test]
    async fn host_list_proxy_keeps_a_body_sent_with_the_head() {
        let target = echo().await;
        let proxy = proxy_for("127.0.0.1", target.port()).await;
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port()))
            .await
            .expect("connects");
        let request = format!(
            concat!(
                "POST http://127.0.0.1:{}/up HTTP/1.1\r\n",
                "Host: x\r\n",
                "Content-Length: 5\r\n\r\n",
                "hello",
            ),
            target.port()
        );
        client.write_all(request.as_bytes()).await.expect("writes");
        let text = answer(&mut client).await;
        assert!(text.starts_with("POST /up HTTP/1.1\r\n"), "{text}");
        assert!(
            text.ends_with("Connection: close\r\n\r\nhello"),
            "the body follows the head once: {text:?}"
        );
    }

    #[tokio::test]
    async fn host_list_proxy_keeps_bytes_sent_right_after_connect() {
        let target = echo().await;
        let proxy = proxy_for("127.0.0.1", target.port()).await;
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port()))
            .await
            .expect("connects");
        let request = format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\nping", target.port());
        client.write_all(request.as_bytes()).await.expect("writes");
        let text = timeout(Duration::from_secs(5), answer(&mut client))
            .await
            .unwrap_or_default();
        assert_eq!(text, "HTTP/1.1 200 Connection established\r\n\r\nping");
    }

    #[test]
    fn host_list_proxy_an_absolute_uri_without_a_port_is_port_80() {
        assert_eq!(
            absolute("http://github.com/x"),
            Some(("github.com", 80, "/x"))
        );
        assert_eq!(absolute("http://github.com"), Some(("github.com", 80, "/")));
        assert_eq!(absolute("http://[::1]/"), Some(("::1", 80, "/")));
        assert_eq!(absolute("http://[::1]:8080/a"), Some(("::1", 8080, "/a")));
    }

    #[tokio::test]
    async fn host_list_proxy_refuses_a_head_just_over_the_limit() {
        let target = echo().await;
        let proxy = proxy_for("127.0.0.1", target.port()).await;
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port()))
            .await
            .expect("connects");
        let mut request = format!("GET http://127.0.0.1:{}/ HTTP/1.1\r\nX: ", target.port());
        // One byte over the limit, with the end of the head in the last read.
        while request.len() < HEAD_LIMIT + 1 - 4 {
            request.push('a');
        }
        request.push_str("\r\n\r\n");
        assert_eq!(request.len(), HEAD_LIMIT + 1);
        client.write_all(request.as_bytes()).await.expect("writes");
        let text = answer(&mut client).await;
        assert!(text.starts_with("HTTP/1.1 431"), "{text}");
    }

    #[tokio::test]
    async fn host_list_proxy_refuses_an_http_target_that_is_not_listed() {
        let target = echo().await;
        let proxy = proxy_for("127.0.0.1", target.port()).await;
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port()))
            .await
            .expect("connects");
        client
            .write_all(b"GET http://127.0.0.1:1/ HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .expect("writes");
        let text = answer(&mut client).await;
        assert!(text.starts_with("HTTP/1.1 403 Forbidden"), "{text}");
        assert!(
            text.contains("reagent sandbox: 127.0.0.1:1 is not in sandbox.network"),
            "{text}"
        );
    }

    /// Greets a SOCKS5 proxy and asks for `host:port`, and answers the reply
    /// code.
    async fn socks_connect(proxy: &Proxy, host: &[u8], port: u16) -> u8 {
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port()))
            .await
            .expect("connects");
        client
            .write_all(&[SOCKS5, 0x01, 0x00])
            .await
            .expect("the greeting");
        let mut greeting = [0u8; 2];
        client.read_exact(&mut greeting).await.expect("the method");
        assert_eq!(greeting, [SOCKS5, 0x00]);
        let mut request = vec![SOCKS5, 0x01, 0x00];
        match host {
            [a, b, c, d] => {
                request.push(0x01);
                request.extend_from_slice(&[*a, *b, *c, *d]);
            }
            name => {
                request.push(0x03);
                request.push(name.len() as u8);
                request.extend_from_slice(name);
            }
        }
        request.extend_from_slice(&port.to_be_bytes());
        client.write_all(&request).await.expect("the request");
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.expect("the reply");
        reply[1]
    }

    #[tokio::test]
    async fn host_list_proxy_socks5_takes_a_domain_and_an_address() {
        let target = echo().await;
        let entry = Host::parse(&format!("localhost:{}", target.port())).expect("an entry");
        let ip = Host::parse(&format!("127.0.0.1:{}", target.port())).expect("an entry");
        let proxy = Proxy::start(&[entry, ip]).await.expect("starts");
        assert_eq!(
            socks_connect(&proxy, b"localhost", target.port()).await,
            0x00
        );
        assert_eq!(
            socks_connect(&proxy, &[127, 0, 0, 1], target.port()).await,
            0x00
        );
        // A name that is not listed is refused without a DNS lookup.
        assert_eq!(
            socks_connect(&proxy, b"example.invalid", target.port()).await,
            0x02
        );
        assert_eq!(
            socks_connect(&proxy, &[127, 0, 0, 1], 1).await,
            0x02,
            "a port that is not listed"
        );
    }

    #[tokio::test]
    async fn host_list_proxy_refuses_a_head_over_eight_kibibytes() {
        let target = echo().await;
        let proxy = proxy_for("127.0.0.1", target.port()).await;
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port()))
            .await
            .expect("connects");
        client
            .write_all(b"GET http://127.0.0.1:1/ HTTP/1.1\r\n")
            .await
            .expect("writes");
        // No empty line in the first 9 KiB, so the head runs over the limit.
        let filler = vec![b'a'; 9 * 1024];
        // The proxy may close the connection while this goes out.
        let _ = client.write_all(&filler).await;
        let text = answer(&mut client).await;
        assert!(text.starts_with("HTTP/1.1 431"), "{text}");
    }

    #[test]
    fn host_list_proxy_matches_a_host_without_case_and_an_address_as_one() {
        let hosts = [
            Host::parse("GitHub.com:443").expect("an entry"),
            Host::parse("[::1]:8080").expect("an entry"),
        ];
        assert!(allowed(&hosts, "github.com", 443));
        assert!(allowed(&hosts, "GITHUB.COM", 443));
        assert!(!allowed(&hosts, "github.com", 80));
        assert!(allowed(&hosts, "::1", 8080));
        assert!(!allowed(&hosts, "localhost", 8080));
        // An entry IP is compared as an address, so its text form is enough.
        let ip = [Host::parse("127.0.0.1:80").expect("an entry")];
        assert!(allowed(&ip, "127.0.0.1", 80));
        assert!(!allowed(&ip, "127.0.0.2", 80));
    }
}
