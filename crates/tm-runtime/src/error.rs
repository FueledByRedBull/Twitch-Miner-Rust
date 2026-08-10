use thiserror::Error;

pub type Result<T> = std::result::Result<T, RuntimeError>;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("runtime closed before handling {command}")]
    RuntimeClosed { command: &'static str },
}
