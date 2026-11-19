
pub mod issue;
pub mod report;
pub mod sanitize;
pub mod time;

#[cfg(test)]
pub(crate) mod test_support;

use std::fmt;

/// The crate error: a message plus, when there is one, the underlying cause's
/// text.
///
/// Go wrote these as `fmt.Errorf("repair: ...: %w", err)`. [`Error::cause_text`]
/// is the `%w` chain's reach, so a caller (and the tests) can assert the cause
/// the way `errors.Is` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
    cause: Option<String>,
}

impl Error {
    /// A failure with no underlying cause.
    pub fn new(message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
            cause: None,
        }
    }

    /// A failure wrapping another one, keeping the cause's text.
    pub fn caused_by(message: impl Into<String>, cause: impl fmt::Display) -> Error {
        Error {
            message: message.into(),
            cause: Some(cause.to_string()),
        }
    }

    /// The message without the cause.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The wrapped failure's text, if there was one.
    pub fn cause_text(&self) -> Option<&str> {
        self.cause.as_deref()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cause {
            Some(cause) => write!(f, "{}: {cause}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::caused_by("repair: io", e)
    }
}

/// `Result` with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
