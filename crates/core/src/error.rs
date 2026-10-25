use std::fmt;

/// Machine-readable failure categories shared by language adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Invalid,
    Unavailable,
    Released,
    Gone,
    Expired,
    Cancelled,
    Forbidden,
    Conflict,
    Unsupported,
    Closed,
    Exhausted,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Unavailable => "unavailable",
            Self::Released => "released",
            Self::Gone => "gone",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
            Self::Forbidden => "forbidden",
            Self::Conflict => "conflict",
            Self::Unsupported => "unsupported",
            Self::Closed => "closed",
            Self::Exhausted => "exhausted",
        }
    }
}

/// An operation failure. External effects performed by an adapter are not rolled back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    pub code: Code,
    pub reason: String,
    pub message: String,
}

impl Error {
    pub fn new(code: Code, reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
            message: message.into(),
        }
    }
    pub fn invalid(reason: &str, message: impl Into<String>) -> Self {
        Self::new(Code::Invalid, reason, message)
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}: {}",
            self.code.as_str(),
            self.reason,
            self.message
        )
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
