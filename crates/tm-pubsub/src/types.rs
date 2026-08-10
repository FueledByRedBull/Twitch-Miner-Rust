use tm_domain::MinerEvent;

#[derive(Debug, Clone, PartialEq)]
pub enum IncomingTransportMessage {
    Pong,
    Reconnect,
    ResponseOk {
        nonce: Option<String>,
    },
    ResponseError {
        nonce: Option<String>,
        is_bad_auth: bool,
    },
    Event(Box<MinerEvent>),
    Ignore,
}
