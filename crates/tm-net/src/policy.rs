use std::io;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::UrlError;
use tokio_tungstenite::tungstenite::handshake::client::Response;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::{
    client_async_tls_with_config, connect_async_with_config, MaybeTlsStream, WebSocketStream,
};

use crate::proxy::{ProxyConfig, ProxyParseError, ProxyScheme};
use crate::tunnel::{self, ProxyFailure};

/// Bounds connecting to the proxy and finishing its handshake.
const PROXY_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const ENV_PROXY_VARIABLES: [&str; 6] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

/// How the miner reaches the network: directly, or through one proxy that
/// carries every connection.
///
/// It is built once at startup and passed explicitly to everything that opens a
/// connection. The proxy's own host name is resolved locally (use an IP address
/// to avoid that); every destination name is resolved by the proxy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkPolicy {
    proxy: Option<ProxyConfig>,
}

impl NetworkPolicy {
    /// No proxy: connections go out directly.
    #[must_use]
    pub fn direct() -> Self {
        Self::default()
    }

    /// Every connection goes through `proxy` or fails.
    #[must_use]
    pub fn through(proxy: ProxyConfig) -> Self {
        Self { proxy: Some(proxy) }
    }

    /// An empty setting means no proxy; anything else must be a valid proxy URL.
    pub fn from_setting(setting: &str) -> Result<Self, ProxyParseError> {
        let setting = setting.trim();
        if setting.is_empty() {
            return Ok(Self::direct());
        }
        ProxyConfig::parse(setting).map(Self::through)
    }

    /// The protocol of the configured proxy, if any. Nothing else about the
    /// proxy is exposed.
    #[must_use]
    pub fn proxy_scheme(&self) -> Option<ProxyScheme> {
        self.proxy.as_ref().map(ProxyConfig::scheme)
    }

    /// The environment proxy variables that `is_set` reports as set while no
    /// proxy is configured. They are ignored, so an operator who relied on one
    /// is connecting directly and needs to be told. Values are never read here.
    #[must_use]
    pub fn ignored_env_proxies(&self, is_set: impl Fn(&str) -> bool) -> Vec<&'static str> {
        if self.proxy.is_some() {
            return Vec::new();
        }
        ENV_PROXY_VARIABLES
            .into_iter()
            .filter(|name| is_set(name))
            .collect()
    }

    /// A client builder that ignores `HTTP_PROXY`, `HTTPS_PROXY` and
    /// `ALL_PROXY`, and sends everything through the configured proxy when
    /// there is one.
    pub fn http_client_builder(&self) -> reqwest::ClientBuilder {
        let builder = reqwest::Client::builder().no_proxy();
        match &self.proxy {
            Some(proxy) => builder.proxy(proxy.reqwest_proxy()),
            None => builder,
        }
    }

    /// Opens a TCP stream to `host:port`. Through a proxy the destination name
    /// is resolved by the proxy and the whole handshake is time-limited; a
    /// proxy failure is an error, never a direct connection.
    pub async fn connect_tcp(&self, host: &str, port: u16) -> io::Result<TcpStream> {
        self.connect_tcp_within(host, port, PROXY_HANDSHAKE_TIMEOUT)
            .await
    }

    pub(crate) async fn connect_tcp_within(
        &self,
        host: &str,
        port: u16,
        handshake_timeout: Duration,
    ) -> io::Result<TcpStream> {
        let Some(proxy) = &self.proxy else {
            return TcpStream::connect((host, port)).await;
        };
        match tokio::time::timeout(handshake_timeout, tunnel::connect(proxy, host, port)).await {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(failure)) => Err(failure.into()),
            Err(_) => Err(ProxyFailure::Timeout.into()),
        }
    }

    /// Opens a WebSocket. Without a proxy this is `connect_async_with_config`
    /// unchanged; with one, the TCP stream comes from [`Self::connect_tcp`] and
    /// TLS and the upgrade run over it.
    pub async fn connect_websocket(
        &self,
        url: &str,
        config: WebSocketConfig,
    ) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response), WebSocketError> {
        if self.proxy.is_none() {
            return connect_async_with_config(url, Some(config), false).await;
        }
        let request = url.into_client_request()?;
        let uri = request.uri();
        let host = uri
            .host()
            .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
            .ok_or(WebSocketError::Url(UrlError::NoHostName))?;
        let port = uri
            .port_u16()
            .or_else(|| match uri.scheme_str() {
                Some("wss") => Some(443),
                Some("ws") => Some(80),
                _ => None,
            })
            .ok_or(WebSocketError::Url(UrlError::UnsupportedUrlScheme))?;
        let stream = self
            .connect_tcp(host, port)
            .await
            .map_err(WebSocketError::Io)?;
        client_async_tls_with_config(request, stream, Some(config), None).await
    }
}
