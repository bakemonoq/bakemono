use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("missing required tag `{0}`")]
    MissingTag(&'static str),
    #[error("unexpected event kind {got}, expected {expected}")]
    WrongKind { expected: u16, got: u16 },
    #[error("malformed `{tag}` tag value: {value}")]
    MalformedTag { tag: &'static str, value: String },
    #[error("`{field}` exceeds its size limit")]
    TooLarge { field: &'static str },
    #[error("event signature verification failed")]
    BadSignature,
    #[error("event build failed: {0}")]
    Build(String),
    #[error("malformed head: {0}")]
    MalformedHead(String),
    #[error("head schema {0} is newer than this build understands")]
    UnknownSchema(u32),
    #[error("head signed by an untrusted key")]
    UntrustedKey,
    #[error("head version {got} does not advance past {last}")]
    StaleVersion { got: u64, last: u64 },
    #[error("malformed hex in `{0}`")]
    BadHex(&'static str),
    #[error("revoked entry names no target")]
    EmptyRevoked,
}

pub type Result<T> = std::result::Result<T, Error>;
