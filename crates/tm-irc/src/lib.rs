//! Twitch IRC framing and chat-presence transport.
//!
//! IRC reports transport-neutral presence/chat observations and does not own
//! mining state, reward accounting, or Twitch mutation policy.

use std::fmt;
use std::io;
use std::sync::{Arc, OnceLock};

use tm_net::NetworkPolicy;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

pub const IRC_HOST: &str = "irc.chat.twitch.tv";
pub const IRC_PORT: u16 = 6697;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedLine {
    Ping {
        payload: String,
    },
    AuthenticationFailed,
    PrivMsg {
        nick: String,
        channel: String,
        message: String,
    },
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEvent {
    Mention {
        nick: String,
        channel: String,
        message: String,
    },
    AuthenticationFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatEventKind {
    Mention,
    AuthenticationFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatTransportAction {
    Write(String),
    Event(ChatEvent),
}

pub trait ChatLogger {
    fn activity(&mut self) {}
    fn printf(&mut self, message: &str);
    fn errorf(&mut self, message: &str);
    fn emoji_eventf(&mut self, emoji: &str, event: ChatEventKind, message: &str);
}

pub struct ChatClient<L> {
    username: String,
    channel: String,
    token: String,
    disable_at_in_nickname: bool,
    logger: L,
    closed: bool,
}

impl<L> fmt::Debug for ChatClient<L> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChatClient")
            .field("username", &self.username)
            .field("channel", &self.channel)
            .field("token", &"<redacted>")
            .field("disable_at_in_nickname", &self.disable_at_in_nickname)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl<L> ChatClient<L>
where
    L: ChatLogger,
{
    pub fn new(
        username: impl AsRef<str>,
        token: impl AsRef<str>,
        channel: impl AsRef<str>,
        logger: L,
        disable_at_in_nickname: bool,
    ) -> Self {
        Self {
            username: normalize_name(username.as_ref()),
            channel: normalize_name(channel.as_ref()),
            token: token.as_ref().trim().to_string(),
            disable_at_in_nickname,
            logger,
            closed: false,
        }
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn handle_line(&mut self, line: &str) -> Option<ChatEvent> {
        self.handle_parsed_line(parse_line(line))
    }

    pub fn registration_commands(&self) -> Vec<String> {
        vec![
            format!("PASS oauth:{}\r\n", self.token),
            format!("NICK {}\r\n", self.username),
            format!("JOIN #{}\r\n", self.channel),
        ]
    }

    pub fn protocol_actions(&mut self, line: &str) -> Vec<ChatTransportAction> {
        let parsed = parse_line(line);
        // A rejected login is not healthy traffic; counting it would reset the
        // failure count that the reconnect after it records.
        if parsed != ParsedLine::AuthenticationFailed {
            self.logger.activity();
        }
        match parsed {
            ParsedLine::Ping { payload } => {
                vec![ChatTransportAction::Write(format!("PONG{payload}\r\n"))]
            }
            parsed => self
                .handle_parsed_line(parsed)
                .into_iter()
                .map(ChatTransportAction::Event)
                .collect(),
        }
    }

    /// Connects over `network` and runs the session. A configured proxy that is
    /// unavailable is an error, never a direct connection.
    pub async fn connect_and_run(&mut self, network: &NetworkPolicy) -> io::Result<()> {
        let stream = network.connect_tcp(IRC_HOST, IRC_PORT).await?;
        let server_name = ServerName::try_from(IRC_HOST.to_owned())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let stream = TlsConnector::from(irc_tls_config())
            .connect(server_name, stream)
            .await?;
        self.run_stream(stream).await
    }

    pub async fn run_stream<S>(&mut self, stream: S) -> io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let (read_half, mut write_half) = tokio::io::split(stream);
        for command in self.registration_commands() {
            write_half.write_all(command.as_bytes()).await?;
        }
        write_half.flush().await?;

        let mut reader = BufReader::new(read_half);
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).await?;
            if read == 0 {
                return Ok(());
            }
            for action in self.protocol_actions(line.trim()) {
                if let ChatTransportAction::Write(payload) = action {
                    write_half.write_all(payload.as_bytes()).await?;
                    write_half.flush().await?;
                }
            }
            if self.closed {
                return Ok(());
            }
        }
    }

    fn handle_parsed_line(&mut self, parsed: ParsedLine) -> Option<ChatEvent> {
        match parsed {
            ParsedLine::AuthenticationFailed => {
                self.closed = true;
                self.logger.errorf(&format_chat_auth_failure(&self.channel));
                Some(ChatEvent::AuthenticationFailed)
            }
            ParsedLine::PrivMsg {
                nick,
                channel,
                message,
            } => {
                if !mentions_username(&message, &self.username, self.disable_at_in_nickname) {
                    return None;
                }

                self.logger.emoji_eventf(
                    ":speech_balloon:",
                    ChatEventKind::Mention,
                    &format_chat_mention(&nick, &channel, &message),
                );

                Some(ChatEvent::Mention {
                    nick,
                    channel,
                    message,
                })
            }
            ParsedLine::Ping { .. } | ParsedLine::Other => None,
        }
    }
}

#[must_use]
pub fn parse_line(line: &str) -> ParsedLine {
    let line = line.trim();
    if line.is_empty() {
        return ParsedLine::Other;
    }

    if let Some(payload) = line.strip_prefix("PING") {
        return ParsedLine::Ping {
            payload: payload.to_string(),
        };
    }

    if !line.contains("PRIVMSG") {
        // Only a server line can reject the login; chat text saying the same
        // words is an ordinary message.
        if contains_ignore_case(line, "authentication failed") {
            return ParsedLine::AuthenticationFailed;
        }
        return ParsedLine::Other;
    }

    let Some((prefix, message)) = line.split_once(" :") else {
        return ParsedLine::Other;
    };

    let prefix = prefix.strip_prefix(':').unwrap_or(prefix);
    let mut parts = prefix.split_whitespace();
    let nick_part = parts.next().unwrap_or_default();
    let _command = parts.next();
    let channel = parts
        .next()
        .unwrap_or_default()
        .trim_start_matches('#')
        .to_string();
    let nick = nick_part
        .split_once('!')
        .map_or_else(|| nick_part.to_string(), |(name, _)| name.to_string());

    ParsedLine::PrivMsg {
        nick,
        channel,
        message: message.to_string(),
    }
}

#[must_use]
pub fn mentions_username(message: &str, username: &str, disable_at_in_nickname: bool) -> bool {
    let message = message.to_lowercase();
    let username = username.trim().to_lowercase();
    let target = format!("@{username}");
    let mentioned = message.contains(&target);
    if disable_at_in_nickname && !mentioned {
        return message.contains(&username);
    }
    mentioned
}

fn normalize_name(value: &str) -> String {
    value.trim().to_lowercase()
}

#[must_use]
pub fn irc_addr() -> String {
    format!("{IRC_HOST}:{IRC_PORT}")
}

fn irc_tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    Arc::clone(CONFIG.get_or_init(|| {
        let roots = webpki_roots::TLS_SERVER_ROOTS
            .iter()
            .cloned()
            .collect::<RootCertStore>();
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }))
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn format_chat_auth_failure(channel: &str) -> String {
    if channel.is_empty() {
        String::from("chat authentication failed")
    } else {
        format!("chat #{channel} authentication failed")
    }
}

fn format_chat_mention(nick: &str, channel: &str, message: &str) -> String {
    let display_nick = if nick.is_empty() { "unknown" } else { nick };
    format!("{display_nick} at #{channel} wrote: {message}")
}

impl fmt::Display for ParsedLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParsedLine::Ping { payload } => write!(f, "PING{payload}"),
            ParsedLine::AuthenticationFailed => f.write_str("AuthenticationFailed"),
            ParsedLine::PrivMsg {
                nick,
                channel,
                message,
            } => write!(f, "PrivMsg({nick} #{channel}: {message})"),
            ParsedLine::Other => f.write_str("Other"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Default)]
    struct StubLogger {
        calls: Vec<String>,
    }

    impl ChatLogger for StubLogger {
        fn activity(&mut self) {
            self.calls.push(String::from("activity"));
        }

        fn printf(&mut self, message: &str) {
            self.calls.push(format!("printf:{message}"));
        }

        fn errorf(&mut self, message: &str) {
            self.calls.push(format!("errorf:{message}"));
        }

        fn emoji_eventf(&mut self, emoji: &str, event: ChatEventKind, message: &str) {
            self.calls
                .push(format!("emoji:{emoji}:{event:?}:{message}"));
        }
    }

    fn fixture_value(name: &str) -> Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name);
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn new_client_normalizes_input() {
        let client = ChatClient::new(
            "UserName",
            " token ",
            "Channel ",
            StubLogger::default(),
            false,
        );
        assert_eq!(client.username(), "username");
        assert_eq!(client.channel(), "channel");
        assert_eq!(client.token(), "token");
    }

    #[test]
    fn debug_redacts_chat_token() {
        let client = ChatClient::new(
            "UserName",
            "chat-secret-token",
            "Channel",
            StubLogger::default(),
            false,
        );
        let output = format!("{client:?}");
        assert!(!output.contains("chat-secret-token"));
        assert!(output.contains("<redacted>"));
    }

    #[test]
    fn rejected_login_is_not_recorded_as_activity() {
        let mut client = ChatClient::new("user", "token", "chan", StubLogger::default(), false);
        client.protocol_actions("PING :tmi.twitch.tv");
        client.protocol_actions("NOTICE * :Login authentication failed");
        let activity = client
            .logger
            .calls
            .iter()
            .filter(|call| *call == "activity")
            .count();
        assert_eq!(activity, 1);
    }

    #[test]
    fn parse_line_extracts_privmsg() {
        let parsed = parse_line(":nick!user PRIVMSG #chan :hello @target there");
        assert_eq!(
            parsed,
            ParsedLine::PrivMsg {
                nick: String::from("nick"),
                channel: String::from("chan"),
                message: String::from("hello @target there"),
            }
        );
    }

    #[test]
    fn parse_line_detects_auth_failure_and_ping() {
        assert_eq!(
            parse_line("NOTICE * :Authentication failed."),
            ParsedLine::AuthenticationFailed
        );
        assert_eq!(
            parse_line("PING :tmi.twitch.tv"),
            ParsedLine::Ping {
                payload: String::from(" :tmi.twitch.tv"),
            }
        );
    }

    #[test]
    fn parse_line_keeps_chat_text_about_auth_failure_as_a_message() {
        assert!(matches!(
            parse_line(
                ":nick!nick@nick.tmi.twitch.tv PRIVMSG #chan :login authentication failed lol"
            ),
            ParsedLine::PrivMsg { .. }
        ));
    }

    #[test]
    fn registration_commands_use_verified_tls_endpoint() {
        let client = ChatClient::new(
            "UserName",
            " token ",
            "Channel ",
            StubLogger::default(),
            false,
        );
        assert_eq!(
            client.registration_commands(),
            vec![
                "PASS oauth:token\r\n".to_string(),
                "NICK username\r\n".to_string(),
                "JOIN #channel\r\n".to_string(),
            ]
        );
        assert_eq!(irc_addr(), "irc.chat.twitch.tv:6697");
        assert!(Arc::ptr_eq(&irc_tls_config(), &irc_tls_config()));
    }

    #[test]
    fn protocol_actions_answer_ping() {
        let mut client = ChatClient::new("target", "token", "chan", StubLogger::default(), false);
        assert_eq!(
            client.protocol_actions("PING :tmi.twitch.tv"),
            vec![ChatTransportAction::Write(
                "PONG :tmi.twitch.tv\r\n".to_string()
            )]
        );
    }

    #[test]
    fn handle_line_logs_mentions() {
        let fixture = fixture_value("irc.valid_mention.json");
        let logger = StubLogger::default();
        let mut client = ChatClient::new(
            "target",
            "token",
            "chan",
            logger,
            fixture["disable_at_in_nickname"].as_bool().unwrap_or(false),
        );

        let event = client.handle_line(fixture["line"].as_str().unwrap());
        assert_eq!(
            event,
            Some(ChatEvent::Mention {
                nick: String::from("nick"),
                channel: String::from("chan"),
                message: String::from("hello @target there"),
            })
        );
        assert_eq!(client.logger.calls.len(), 1);
        assert!(client.logger.calls[0].starts_with("emoji::speech_balloon:"));
    }

    #[test]
    fn handle_line_supports_disable_at_in_nickname() {
        let fixture = fixture_value("irc.plain_username_mention.json");
        let logger = StubLogger::default();
        let mut client = ChatClient::new(
            "target",
            "token",
            "chan",
            logger,
            fixture["disable_at_in_nickname"].as_bool().unwrap_or(false),
        );

        let event = client.handle_line(fixture["line"].as_str().unwrap());
        assert_eq!(
            event,
            Some(ChatEvent::Mention {
                nick: String::from("nick"),
                channel: String::from("chan"),
                message: String::from("target is cool"),
            })
        );
        assert_eq!(client.logger.calls.len(), 1);
    }

    #[test]
    fn handle_line_ignores_non_mentions() {
        let fixture = fixture_value("irc.non_mention.json");
        let logger = StubLogger::default();
        let mut client = ChatClient::new("target", "token", "chan", logger, false);

        assert_eq!(client.handle_line(fixture["line"].as_str().unwrap()), None);
        assert!(client.logger.calls.is_empty());
    }

    #[test]
    fn handle_line_marks_auth_failure_closed() {
        let fixture = fixture_value("irc.auth_failure.json");
        let logger = StubLogger::default();
        let mut client = ChatClient::new("target", "token", "chan", logger, false);

        let event = client.handle_line(fixture["line"].as_str().unwrap());
        assert_eq!(event, Some(ChatEvent::AuthenticationFailed));
        assert!(client.is_closed());
        assert_eq!(client.logger.calls.len(), 1);
        assert!(client.logger.calls[0].starts_with("errorf:"));
    }

    #[tokio::test]
    async fn run_stream_writes_registration_and_pong() {
        let logger = StubLogger::default();
        let client = ChatClient::new("target", "token", "chan", logger, false);
        let (client_side, mut server_side) = tokio::io::duplex(1024);

        let task = tokio::spawn(async move {
            let mut client = client;
            client.run_stream(client_side).await.unwrap();
            client
        });

        let mut initial = vec![0_u8; 128];
        let read = server_side.read(&mut initial).await.unwrap();
        let output = String::from_utf8(initial[..read].to_vec()).unwrap();
        assert!(output.contains("PASS oauth:token\r\n"));
        assert!(output.contains("NICK target\r\n"));
        assert!(output.contains("JOIN #chan\r\n"));

        server_side
            .write_all(b"PING :tmi.twitch.tv\r\n")
            .await
            .unwrap();

        let mut pong = vec![0_u8; 64];
        let read = server_side.read(&mut pong).await.unwrap();
        assert_eq!(
            String::from_utf8(pong[..read].to_vec()).unwrap(),
            "PONG :tmi.twitch.tv\r\n"
        );

        drop(server_side);
        let client = task.await.unwrap();
        assert!(!client.is_closed());
    }

    /// Accepts one SOCKS5 connection, records the destination it asked for,
    /// then hangs up.
    async fn spawn_recording_socks(
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<(String, u16)>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0_u8; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            stream.write_all(&[5, 0]).await.unwrap();
            let mut head = [0_u8; 4];
            stream.read_exact(&mut head).await.unwrap();
            assert_eq!(head[3], 3, "the destination must be sent as a name");
            let len = usize::from(stream.read_u8().await.unwrap());
            let mut name = vec![0_u8; len];
            stream.read_exact(&mut name).await.unwrap();
            let port = stream.read_u16().await.unwrap();
            (String::from_utf8(name).unwrap(), port)
        });
        (address, task)
    }

    #[tokio::test]
    async fn the_session_connects_through_the_proxy_by_name() {
        let (address, destination) = spawn_recording_socks().await;
        let network = NetworkPolicy::through(
            tm_net::ProxyConfig::parse(&format!("socks5h://{address}")).unwrap(),
        );
        let mut client = ChatClient::new("user", "token", "chan", StubLogger::default(), false);

        assert!(client.connect_and_run(&network).await.is_err());

        assert_eq!(
            destination.await.unwrap(),
            (String::from(IRC_HOST), IRC_PORT)
        );
    }

    #[tokio::test]
    async fn an_unavailable_proxy_fails_the_session() {
        let dead = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap()
        };
        let network =
            NetworkPolicy::through(tm_net::ProxyConfig::parse(&format!("http://{dead}")).unwrap());
        let mut client = ChatClient::new("user", "token", "chan", StubLogger::default(), false);

        let error = client.connect_and_run(&network).await.unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
    }
}
