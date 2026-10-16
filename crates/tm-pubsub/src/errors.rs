use thiserror::Error;
use tokio_tungstenite::tungstenite;

#[derive(Debug, Error)]
pub enum PubSubError {
    #[error("no user id for pubsub")]
    MissingUserId,
    #[error("invalid pubsub payload: {0}")]
    InvalidPayload(#[from] serde_json::Error),
    #[error("invalid pubsub text payload: {0}")]
    InvalidText(#[from] std::string::FromUtf8Error),
    #[error("pubsub protocol error: {0}")]
    Protocol(&'static str),
    #[error("websocket error: {0}")]
    WebSocket(#[from] tungstenite::Error),
    #[error("event channel closed")]
    EventChannelClosed,
    #[error("pubsub reconnect requested")]
    ReconnectRequested,
    #[error("pubsub bad auth for {cookie_file}")]
    BadAuth { cookie_file: String },
    #[error("pubsub pong timeout")]
    PongTimeout,
}

impl PubSubError {
    /// Why the WebSocket transport failed, as a fixed label. The error text is
    /// not logged, so this keeps a dropped connection diagnosable without it.
    #[must_use]
    pub fn websocket_reason(&self) -> Option<&'static str> {
        use std::io::ErrorKind;
        use tungstenite::error::ProtocolError;
        let Self::WebSocket(error) = self else {
            return None;
        };
        Some(match error {
            tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => "closed",
            tungstenite::Error::Protocol(ProtocolError::ResetWithoutClosingHandshake) => {
                "reset-without-close"
            }
            tungstenite::Error::Protocol(_) => "protocol",
            tungstenite::Error::Io(error) => match error.kind() {
                ErrorKind::ConnectionReset => "io-connection-reset",
                ErrorKind::ConnectionAborted => "io-connection-aborted",
                ErrorKind::BrokenPipe => "io-broken-pipe",
                ErrorKind::TimedOut => "io-timed-out",
                ErrorKind::UnexpectedEof => "io-unexpected-eof",
                _ => "io-other",
            },
            tungstenite::Error::Tls(_) => "tls",
            _ => "other",
        })
    }
}
