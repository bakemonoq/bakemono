use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("missing required tag `{0}`")]
    MissingTag(&'static str),
    #[error("unexpected event kind {got}, expected {expected}")]
    WrongKind { expected: u16, got: u16 },
    #[error("malformed `{tag}` tag value: {value}")]
    MalformedTag { tag: &'static str, value: String },
    #[error("event signature verification failed")]
    BadSignature,
    #[error("event build failed: {0}")]
    Build(String),
}

pub type Result<T> = std::result::Result<T, Error>;
