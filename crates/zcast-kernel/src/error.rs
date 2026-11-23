use thiserror::Error;
use zcast_interfaces::{LinkError, StorageError};

#[derive(Debug, Error)]
pub enum KernelError {
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("link parser error: {0}")]
    Links(#[from] LinkError),
    #[error("config error: {0}")]
    Config(String),
    #[error("unsafe bind address '{0}' without an active auth module (bind to 127.0.0.1 or pass --allow-unauthenticated-public)")]
    UnsafeUnauthenticatedBind(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("forbidden")]
    Forbidden,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
