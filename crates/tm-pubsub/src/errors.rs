use thiserror::Error;
use tokio_tungstenite::tungstenite;

#[derive(Debug, Error)]
pub enum PubSubError {
    #[error("no user id for pubsub")]
    MissingUserId,
    #[error("pubsub topic capacity exceeded: configured {configured}, maximum {maximum}")]
    CapacityExceeded { configured: usize, maximum: usize },
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
