use std::io;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_socks::tcp::Socks5Stream;

use crate::proxy::{ProxyConfig, ProxyScheme};

/// Largest response head accepted from an HTTP proxy.
const MAX_CONNECT_RESPONSE_BYTES: usize = 8 * 1024;

/// How a proxy handshake failed. It names the failure class only; the proxy's
/// address and credentials never appear in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProxyFailure {
    #[error("could not connect to the configured proxy")]
    Unreachable,
    #[error("the proxy handshake timed out")]
    Timeout,
    #[error("the proxy rejected the credentials")]
    Authentication,
    #[error("the proxy refused to connect to the destination")]
    Rejected,
    #[error("the proxy sent an invalid or oversized response")]
    Protocol,
}

impl From<ProxyFailure> for io::Error {
    fn from(failure: ProxyFailure) -> Self {
        let kind = match failure {
            ProxyFailure::Unreachable | ProxyFailure::Rejected => io::ErrorKind::ConnectionRefused,
            ProxyFailure::Timeout => io::ErrorKind::TimedOut,
            ProxyFailure::Authentication => io::ErrorKind::PermissionDenied,
            ProxyFailure::Protocol => io::ErrorKind::InvalidData,
        };
        io::Error::new(kind, failure)
    }
}

/// Opens a tunnel to `host:port` through `proxy`. A host name is sent to the
/// proxy as a name, never resolved here.
pub(crate) async fn connect(
    proxy: &ProxyConfig,
    host: &str,
    port: u16,
) -> Result<TcpStream, ProxyFailure> {
    let stream = TcpStream::connect((proxy.host(), proxy.port()))
        .await
        .map_err(|_| ProxyFailure::Unreachable)?;
    match proxy.scheme() {
        ProxyScheme::Socks5h => socks5(proxy, stream, host, port).await,
        ProxyScheme::Http => http_connect(proxy, stream, host, port).await,
    }
}

async fn socks5(
    proxy: &ProxyConfig,
    stream: TcpStream,
    host: &str,
    port: u16,
) -> Result<TcpStream, ProxyFailure> {
    let target = (host, port);
    let tunnel = match proxy.credentials() {
        Some(credentials) => {
            Socks5Stream::connect_with_password_and_socket(
                stream,
                target,
                &credentials.username,
                &credentials.password,
            )
            .await
        }
        None => Socks5Stream::connect_with_socket(stream, target).await,
    }
    .map_err(|error| socks_failure(&error))?;
    Ok(tunnel.into_inner())
}

fn socks_failure(error: &tokio_socks::Error) -> ProxyFailure {
    use tokio_socks::Error;
    match error {
        Error::PasswordAuthFailure(_)
        | Error::AuthorizationRequired
        | Error::NoAcceptableAuthMethods
        | Error::InvalidAuthValues(_) => ProxyFailure::Authentication,
        Error::GeneralSocksServerFailure
        | Error::ConnectionNotAllowedByRuleset
        | Error::NetworkUnreachable
        | Error::HostUnreachable
        | Error::ConnectionRefused
        | Error::TtlExpired => ProxyFailure::Rejected,
        _ => ProxyFailure::Protocol,
    }
}

async fn http_connect(
    proxy: &ProxyConfig,
    mut stream: TcpStream,
    host: &str,
    port: u16,
) -> Result<TcpStream, ProxyFailure> {
    if host.is_empty()
        || host
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err(ProxyFailure::Protocol);
    }
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(credentials) = proxy.credentials() {
        let token = STANDARD.encode(format!("{}:{}", credentials.username, credentials.password));
        request = format!("{request}Proxy-Authorization: Basic {token}\r\n");
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|_| ProxyFailure::Protocol)?;

    let head = read_response_head(&mut stream).await?;
    match connect_status(&head)? {
        200..=299 => Ok(stream),
        407 => Err(ProxyFailure::Authentication),
        _ => Err(ProxyFailure::Rejected),
    }
}

/// Reads one byte at a time so nothing past the blank line is consumed: the
/// tunnel's own traffic starts right after it.
async fn read_response_head(stream: &mut TcpStream) -> Result<Vec<u8>, ProxyFailure> {
    let mut head = Vec::with_capacity(256);
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_CONNECT_RESPONSE_BYTES {
            return Err(ProxyFailure::Protocol);
        }
        let byte = stream.read_u8().await.map_err(|_| ProxyFailure::Protocol)?;
        head.push(byte);
    }
    Ok(head)
}

fn connect_status(head: &[u8]) -> Result<u16, ProxyFailure> {
    let line = head
        .split(|byte| *byte == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())
        .ok_or(ProxyFailure::Protocol)?;
    let mut parts = line.trim_end().splitn(3, ' ');
    let version = parts.next().ok_or(ProxyFailure::Protocol)?;
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(ProxyFailure::Protocol);
    }
    parts
        .next()
        .filter(|code| code.len() == 3)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(ProxyFailure::Protocol)
}
