use thiserror::Error;

pub type Result<T> = std::result::Result<T, RuntimeError>;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("runtime command send failed for {command}")]
    SendFailed { command: &'static str },
    #[error("runtime actor closed before replying to {command}")]
    ActorClosed { command: &'static str },
    #[error("runtime caller dropped reply for {command}")]
    CallerDropped { command: &'static str },
}
