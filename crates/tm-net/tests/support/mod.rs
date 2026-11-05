//! Loopback fakes: a SOCKS5 proxy, an HTTP proxy, and destination servers.
//! Nothing here leaves the machine. Other crates include this file to test
//! their transports through a proxy.
#![allow(dead_code)]

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

/// A destination as the SOCKS5 proxy received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocksTarget {
    Domain(String, u16),
    Ip(IpAddr, u16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocksRequest {
    pub offered_methods: Vec<u8>,
    pub credentials: Option<(String, String)>,
    pub target: SocksTarget,
}

#[derive(Clone)]
pub struct SocksOptions {
    /// User and password the proxy demands, if any.
    pub required_credentials: Option<(String, String)>,
    /// SOCKS5 reply code sent instead of connecting (0 connects).
    pub reply: u8,
    /// Where every accepted request is actually sent, whatever name it named.
    pub forward_to: SocketAddr,
    /// Destination ports that go somewhere other than `forward_to`.
    pub routes: Vec<(u16, SocketAddr)>,
}

impl SocksOptions {
    pub fn forwarding_to(forward_to: SocketAddr) -> Self {
        Self {
            required_credentials: None,
            reply: 0,
            forward_to,
            routes: Vec::new(),
        }
    }
}

pub struct FakeSocks {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<SocksRequest>>>,
    pub connections: Arc<AtomicUsize>,
}

impl FakeSocks {
    pub fn requests(&self) -> Vec<SocksRequest> {
        self.requests.lock().unwrap().clone()
    }
}

pub async fn spawn_socks(options: SocksOptions) -> FakeSocks {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::new(AtomicUsize::new(0));
    tokio::spawn({
        let requests = Arc::clone(&requests);
        let connections = Arc::clone(&connections);
        async move {
            while let Ok((client, _)) = listener.accept().await {
                connections.fetch_add(1, Ordering::SeqCst);
                let options = options.clone();
                let requests = Arc::clone(&requests);
                tokio::spawn(async move {
                    let _ = serve_socks(client, &options, &requests).await;
                });
            }
        }
    });
    FakeSocks {
        addr,
        requests,
        connections,
    }
}

async fn serve_socks(
    mut client: TcpStream,
    options: &SocksOptions,
    requests: &Mutex<Vec<SocksRequest>>,
) -> io::Result<()> {
    let mut greeting = [0_u8; 2];
    client.read_exact(&mut greeting).await?;
    let mut offered_methods = vec![0_u8; usize::from(greeting[1])];
    client.read_exact(&mut offered_methods).await?;

    let mut credentials = None;
    if let Some(expected) = &options.required_credentials {
        if !offered_methods.contains(&0x02) {
            return client.write_all(&[5, 0xFF]).await;
        }
        client.write_all(&[5, 0x02]).await?;
        let mut version_and_user_len = [0_u8; 2];
        client.read_exact(&mut version_and_user_len).await?;
        let mut user = vec![0_u8; usize::from(version_and_user_len[1])];
        client.read_exact(&mut user).await?;
        let password_len = client.read_u8().await?;
        let mut password = vec![0_u8; usize::from(password_len)];
        client.read_exact(&mut password).await?;
        let given = (
            String::from_utf8_lossy(&user).into_owned(),
            String::from_utf8_lossy(&password).into_owned(),
        );
        let accepted = &given == expected;
        credentials = Some(given);
        client.write_all(&[1, u8::from(!accepted)]).await?;
        if !accepted {
            return Ok(());
        }
    } else {
        client.write_all(&[5, 0x00]).await?;
    }

    let mut request_head = [0_u8; 4];
    client.read_exact(&mut request_head).await?;
    let target = match request_head[3] {
        1 => {
            let mut octets = [0_u8; 4];
            client.read_exact(&mut octets).await?;
            let port = client.read_u16().await?;
            SocksTarget::Ip(IpAddr::from(octets), port)
        }
        3 => {
            let len = client.read_u8().await?;
            let mut name = vec![0_u8; usize::from(len)];
            client.read_exact(&mut name).await?;
            let port = client.read_u16().await?;
            SocksTarget::Domain(String::from_utf8_lossy(&name).into_owned(), port)
        }
        4 => {
            let mut octets = [0_u8; 16];
            client.read_exact(&mut octets).await?;
            let port = client.read_u16().await?;
            SocksTarget::Ip(IpAddr::from(octets), port)
        }
        _ => return Ok(()),
    };
    let port = match &target {
        SocksTarget::Domain(_, port) | SocksTarget::Ip(_, port) => *port,
    };
    requests.lock().unwrap().push(SocksRequest {
        offered_methods,
        credentials,
        target,
    });

    if options.reply != 0 {
        return client
            .write_all(&[5, options.reply, 0, 1, 0, 0, 0, 0, 0, 0])
            .await;
    }
    let mut upstream = TcpStream::connect(route(options.forward_to, &options.routes, port)).await?;
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

#[derive(Clone)]
pub struct HttpProxyOptions {
    /// Exact `Proxy-Authorization` value the proxy demands, if any.
    pub required_authorization: Option<String>,
    /// Status of the response to any request; 200 forwards it.
    pub status: u16,
    /// Bytes sent verbatim instead of any response, to model a broken proxy.
    pub raw_response: Option<Vec<u8>>,
    /// Never answer, to model a proxy that hangs.
    pub hang: bool,
    pub forward_to: SocketAddr,
    /// Destination ports that go somewhere other than `forward_to`.
    pub routes: Vec<(u16, SocketAddr)>,
}

impl HttpProxyOptions {
    pub fn forwarding_to(forward_to: SocketAddr) -> Self {
        Self {
            required_authorization: None,
            status: 200,
            raw_response: None,
            hang: false,
            forward_to,
            routes: Vec::new(),
        }
    }
}

pub struct FakeHttpProxy {
    pub addr: SocketAddr,
    /// Every request head the proxy received, as text.
    pub heads: Arc<Mutex<Vec<String>>>,
    pub connections: Arc<AtomicUsize>,
}

impl FakeHttpProxy {
    pub fn heads(&self) -> Vec<String> {
        self.heads.lock().unwrap().clone()
    }
}

pub async fn spawn_http_proxy(options: HttpProxyOptions) -> FakeHttpProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::new(AtomicUsize::new(0));
    tokio::spawn({
        let heads = Arc::clone(&heads);
        let connections = Arc::clone(&connections);
        async move {
            while let Ok((client, _)) = listener.accept().await {
                connections.fetch_add(1, Ordering::SeqCst);
                let options = options.clone();
                let heads = Arc::clone(&heads);
                tokio::spawn(async move {
                    let _ = serve_http_proxy(client, &options, &heads).await;
                });
            }
        }
    });
    FakeHttpProxy {
        addr,
        heads,
        connections,
    }
}

async fn serve_http_proxy(
    mut client: TcpStream,
    options: &HttpProxyOptions,
    heads: &Mutex<Vec<String>>,
) -> io::Result<()> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(client.read_u8().await?);
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    heads.lock().unwrap().push(text.clone());
    if options.hang {
        std::future::pending::<()>().await;
    }
    if let Some(raw) = &options.raw_response {
        return client.write_all(raw).await;
    }
    if let Some(expected) = &options.required_authorization {
        let given = text.lines().find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("proxy-authorization"))
                .map(|(_, value)| value.trim().to_string())
        });
        if given.as_deref() != Some(expected.as_str()) {
            return client
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
                )
                .await;
        }
    }
    let is_connect = text.starts_with("CONNECT ");
    if options.status != 200 {
        let response = format!(
            "HTTP/1.1 {} Refused\r\nContent-Length: 0\r\n\r\n",
            options.status
        );
        return client.write_all(response.as_bytes()).await;
    }
    let port = request_port(&text);
    let mut upstream = TcpStream::connect(route(options.forward_to, &options.routes, port)).await?;
    if is_connect {
        client
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await?;
    } else {
        upstream.write_all(&head).await?;
    }
    copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// A destination that must never be reached directly: it counts every
/// connection it accepts.
pub struct Target {
    pub addr: SocketAddr,
    pub connections: Arc<AtomicUsize>,
}

impl Target {
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// Answers any HTTP request with `200` and `body`, then closes.
pub async fn spawn_http_target(body: &'static str) -> Target {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    tokio::spawn({
        let connections = Arc::clone(&connections);
        async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                connections.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") {
                        match stream.read_u8().await {
                            Ok(byte) => head.push(byte),
                            Err(_) => return,
                        }
                    }
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        }
    });
    Target { addr, connections }
}

/// Accepts WebSocket upgrades, sends `greeting` as a text frame, and counts
/// connections.
pub async fn spawn_ws_target(greeting: &'static str) -> Target {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    tokio::spawn({
        let connections = Arc::clone(&connections);
        async move {
            while let Ok((stream, _)) = listener.accept().await {
                connections.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    let _ = socket.send(Message::Text(greeting.into())).await;
                    while socket.next().await.is_some() {}
                });
            }
        }
    });
    Target { addr, connections }
}

/// An address nothing listens on, so connecting to it is refused at once.
pub async fn refused_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

fn route(default: SocketAddr, routes: &[(u16, SocketAddr)], port: u16) -> SocketAddr {
    routes
        .iter()
        .find(|(routed, _)| *routed == port)
        .map_or(default, |(_, address)| *address)
}

/// The destination port named by a CONNECT or absolute-form request line.
fn request_port(head: &str) -> u16 {
    let target = head.split_whitespace().nth(1).unwrap_or_default();
    let (default, authority) = match target.strip_prefix("http://") {
        Some(rest) => (80, rest.split('/').next().unwrap_or_default()),
        None => (443, target),
    };
    authority
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(default)
}
