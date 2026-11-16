//! The devkit's error type: the subset of brokkr's `DevError` these tools use.

use std::fmt;

#[derive(Debug)]
pub enum DevError {
    /// A configuration or resolution failure, with its message.
    Config(String),
    Io(std::io::Error),
    /// An archive that fails a structural expectation.
    Verify(String),
    /// A verdict: exit with this code, having already printed the report.
    ExitCode(i32),
}

impl fmt::Display for DevError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(f, "config: {message}"),
            Self::Io(error) => write!(f, "io: {error}"),
            Self::Verify(message) => write!(f, "verify: {message}"),
            Self::ExitCode(code) => write!(f, "exit code {code}"),
        }
    }
}

impl std::error::Error for DevError {}

impl From<std::io::Error> for DevError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
