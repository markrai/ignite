//! One error type for every protocol validation failure (fail closed, never repair).

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolError(String);

impl ProtocolError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProtocolError {}

pub type Result<T> = std::result::Result<T, ProtocolError>;

/// Early-return a [`ProtocolError`] built with `format!`.
macro_rules! refuse {
    ($($arg:tt)*) => {
        return Err($crate::error::ProtocolError::new(format!($($arg)*)))
    };
}
pub(crate) use refuse;
