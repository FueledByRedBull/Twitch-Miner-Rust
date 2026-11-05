use std::fmt;
use std::net::Ipv6Addr;

use thiserror::Error;

/// The proxy protocols the miner accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyScheme {
    /// SOCKS5 where the proxy resolves host names (`socks5h://`).
    Socks5h,
    /// HTTP proxy that tunnels with `CONNECT` (`http://`).
    Http,
}

impl ProxyScheme {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Socks5h => "socks5h",
            Self::Http => "http",
        }
    }
}

impl fmt::Display for ProxyScheme {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why a proxy URL was rejected. No variant carries any part of the URL, so a
/// rejected value cannot reach a log through the error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProxyParseError {
    #[error("not a valid proxy URL; expected socks5h://[user:password@]host:port or http://[user:password@]host:port")]
    Malformed,
    #[error("socks5:// would resolve host names locally and leak them; use socks5h:// so the proxy resolves them")]
    LocalDns,
    #[error("https:// proxies are not supported; use socks5h:// or http://")]
    Https,
    #[error("only socks5h:// and http:// proxies are supported")]
    UnsupportedScheme,
    #[error("the proxy URL needs a host")]
    MissingHost,
    #[error("the proxy URL needs an explicit port")]
    MissingPort,
    #[error("the proxy URL must not have a path, query, or fragment")]
    ExtraComponents,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Credentials {
    pub(crate) username: String,
    pub(crate) password: String,
}

/// A validated proxy endpoint. `Debug` shows the scheme only: neither the
/// address nor the credentials appear in any formatted output.
#[derive(Clone)]
pub struct ProxyConfig {
    scheme: ProxyScheme,
    host: String,
    port: u16,
    credentials: Option<Credentials>,
    reqwest: reqwest::Proxy,
}

impl ProxyConfig {
    /// Parses `socks5h://` or `http://` with an optional `user:password@` and a
    /// required host and port. User names and passwords use percent-encoding.
    pub fn parse(input: &str) -> Result<Self, ProxyParseError> {
        let input = input.trim();
        let (scheme_text, rest) = input.split_once("://").ok_or(ProxyParseError::Malformed)?;
        let scheme = match scheme_text.to_ascii_lowercase().as_str() {
            "socks5h" => ProxyScheme::Socks5h,
            "http" => ProxyScheme::Http,
            "socks5" => return Err(ProxyParseError::LocalDns),
            "https" => return Err(ProxyParseError::Https),
            _ => return Err(ProxyParseError::UnsupportedScheme),
        };

        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(authority_end);
        if !matches!(tail, "" | "/") {
            return Err(ProxyParseError::ExtraComponents);
        }
        let (userinfo, host_port) = match authority.rsplit_once('@') {
            Some((userinfo, host_port)) => (Some(userinfo), host_port),
            None => (None, authority),
        };
        let (host, port) = split_host_port(host_port)?;
        let credentials = userinfo.map(parse_credentials).transpose()?;

        let reqwest = reqwest::Proxy::all(input).map_err(|_| ProxyParseError::Malformed)?;
        Ok(Self {
            scheme,
            host,
            port,
            credentials,
            reqwest,
        })
    }

    #[must_use]
    pub fn scheme(&self) -> ProxyScheme {
        self.scheme
    }

    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    pub(crate) fn credentials(&self) -> Option<&Credentials> {
        self.credentials.as_ref()
    }

    pub(crate) fn reqwest_proxy(&self) -> reqwest::Proxy {
        self.reqwest.clone()
    }
}

impl PartialEq for ProxyConfig {
    fn eq(&self, other: &Self) -> bool {
        self.scheme == other.scheme
            && self.host == other.host
            && self.port == other.port
            && self.credentials == other.credentials
    }
}

impl Eq for ProxyConfig {}

impl fmt::Debug for ProxyConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyConfig")
            .field("scheme", &self.scheme)
            .finish_non_exhaustive()
    }
}

fn split_host_port(host_port: &str) -> Result<(String, u16), ProxyParseError> {
    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (address, after) = bracketed
            .split_once(']')
            .ok_or(ProxyParseError::Malformed)?;
        address
            .parse::<Ipv6Addr>()
            .map_err(|_| ProxyParseError::Malformed)?;
        let port = after
            .strip_prefix(':')
            .ok_or(ProxyParseError::MissingPort)?;
        (address, port)
    } else {
        let (host, port) = host_port
            .rsplit_once(':')
            .ok_or(ProxyParseError::MissingPort)?;
        if host.is_empty() {
            return Err(ProxyParseError::MissingHost);
        }
        if !host.split('.').all(is_host_label) {
            return Err(ProxyParseError::Malformed);
        }
        (host, port)
    };
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ProxyParseError::Malformed);
    }
    match port.parse::<u16>() {
        Ok(port) if port != 0 => Ok((host.to_string(), port)),
        _ => Err(ProxyParseError::Malformed),
    }
}

fn is_host_label(label: &str) -> bool {
    !label.is_empty()
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn parse_credentials(userinfo: &str) -> Result<Credentials, ProxyParseError> {
    let (username, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    if username.is_empty() {
        return Err(ProxyParseError::Malformed);
    }
    Ok(Credentials {
        username: percent_decode(username),
        password: percent_decode(password),
    })
}

/// Decodes `%XX` escapes the way reqwest does for proxy credentials, so the
/// miner's own handshakes and reqwest's agree on the same user and password.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .and_then(|pair| {
                let high = char::from(pair[0]).to_digit(16)?;
                let low = char::from(pair[1]).to_digit(16)?;
                u8::try_from(high * 16 + low).ok()
            });
        if let Some(byte) = escaped {
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}
