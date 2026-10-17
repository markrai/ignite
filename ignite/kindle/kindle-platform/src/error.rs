//! Platform error types — prefer explicit Unsupported over invented behavior.

use std::fmt;

#[derive(Debug)]
pub enum PlatformError {
    Unsupported(&'static str),
    Io(String),
    NotFound(String),
    InvalidInput(String),
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            Self::Io(msg) => write!(f, "io: {msg}"),
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
            Self::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
        }
    }
}

impl std::error::Error for PlatformError {}

pub type PlatformResult<T> = Result<T, PlatformError>;
