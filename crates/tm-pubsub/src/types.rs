use tm_events::MinerEvent;

#[derive(Debug, Clone, PartialEq)]
pub enum IncomingTransportMessage {
    Pong,
    Reconnect,
    ResponseError {
        error: String,
        nonce: Option<String>,
        is_bad_auth: bool,
    },
    Event(Box<MinerEvent>),
    Ignore,
}

/// Compatibility name retained for legacy `PubSub` callers and fixtures.
pub type PubSubEvent = MinerEvent;
