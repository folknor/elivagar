//! The run's error type and its exit-code classes.
//!
//! A failure carries a class only when the code that raised it has evidence
//! for that class. A known operational failure is [`ErrorKind::Io`], a known
//! rejection of the request, the input, the resume state or the ocean
//! artifact is the matching refusal, and everything else is
//! [`ErrorKind::Failed`]. The absence of an errno is never evidence: an
//! `io::Error` without `raw_os_error()` may be a synthesized EOF, a rewrapped
//! OS error or one of our own validation messages, so the generic conversion
//! sends it to `Failed` rather than guessing a domain class. A subsystem that
//! still reports through untyped `io::Error` therefore exits 2, which is
//! coarse but true, until it types its refusals at their source.
//!
//! The kind is derived from the private class, so a kind and a typed reason
//! can never disagree, and the cause is kept as a source rather than being
//! formatted away.

use std::fmt;

type Cause = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Broad failure class of a [`PipelineError`]. Each maps to one process exit
/// code (see [`ErrorKind::exit_code`]); the classes say what kind of thing went
/// wrong, not which recovery to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The request itself is invalid: a setting out of range, a combination
    /// the build or the contract cannot serve, a malformed environment knob.
    /// Raised before any work.
    Config,
    /// Input bytes were read and rejected (malformed injected data, unsorted
    /// input without the opt-in).
    InputRefused,
    /// The resume state under the scratch directory is absent or does not
    /// match this run. [`PipelineError::resume_refusal`] names the reason.
    ResumeRefused,
    /// The named ocean artifact cannot serve this run.
    ArtifactRefused,
    /// An operational I/O failure the OS reported.
    Io,
    /// An impossible in-memory program state. Never used for persisted
    /// state that differs from what was expected: corruption of scratch
    /// files is a possible cause there, so those are `Failed`.
    Internal,
    /// Not classified. The failure may follow substantial work.
    Failed,
}

impl ErrorKind {
    /// The process exit code for this class, as `reference/cli.md` documents.
    #[must_use]
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Failed => 2,
            Self::ResumeRefused => 3,
            Self::ArtifactRefused => 4,
            Self::Config => 64,
            Self::InputRefused => 65,
            Self::Internal => 70,
            Self::Io => 74,
        }
    }

    /// The class an `io::Error` carries with no further context: `Io` when
    /// the OS reported it, `Failed` otherwise.
    #[must_use]
    pub fn of_io(error: &std::io::Error) -> Self {
        if error.raw_os_error().is_some() {
            Self::Io
        } else {
            Self::Failed
        }
    }
}

/// Why a `--skip-to` resume was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResumeRefusal {
    /// No PBF checkpoint record exists in the scratch directory.
    MissingCheckpoint,
    /// The checkpoint record has another version or does not parse.
    MalformedCheckpoint,
    /// No chunk completion record exists, so the ocean phase may not have
    /// finished.
    MissingCompletion,
    /// The completion record has another version or does not parse.
    MalformedCompletion,
    /// The chunks were written under another sort-chunk compression.
    SortCompressionChanged,
    /// The chunks were built from a different input PBF.
    InputChanged,
    /// The chunks were produced under a different producer config; the
    /// differing fields are named.
    ProducerConfigChanged { fields: Vec<String> },
    /// The ocean chunks were produced from a different ocean input.
    OceanChanged,
}

#[derive(Debug)]
enum Class {
    Config,
    Input,
    Resume(ResumeRefusal),
    Artifact,
    Io,
    Internal,
    Failed,
}

/// Error returned by [`crate::run`] and [`crate::ocean_build`].
///
/// Read the class with [`PipelineError::kind`]; `Display` gives the message
/// followed by the cause.
#[derive(Debug)]
pub struct PipelineError(Box<Inner>);

#[derive(Debug)]
struct Inner {
    class: Class,
    message: String,
    cause: Option<Cause>,
}

impl PipelineError {
    fn new(class: Class, message: impl Into<String>) -> Self {
        Self(Box::new(Inner {
            class,
            message: message.into(),
            cause: None,
        }))
    }

    pub(crate) fn config(message: impl Into<String>) -> Self {
        Self::new(Class::Config, message)
    }

    pub(crate) fn input(message: impl Into<String>) -> Self {
        Self::new(Class::Input, message)
    }

    pub(crate) fn resume(reason: ResumeRefusal, message: impl Into<String>) -> Self {
        Self::new(Class::Resume(reason), message)
    }

    pub(crate) fn artifact(message: impl Into<String>) -> Self {
        Self::new(Class::Artifact, message)
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(Class::Internal, message)
    }

    pub(crate) fn failed(message: impl Into<String>) -> Self {
        Self::new(Class::Failed, message)
    }

    /// An `io::Error` under a context message, classed by
    /// [`ErrorKind::of_io`].
    pub(crate) fn io(message: impl Into<String>, error: std::io::Error) -> Self {
        let class = match ErrorKind::of_io(&error) {
            ErrorKind::Io => Class::Io,
            _ => Class::Failed,
        };
        Self::new(class, message).with_cause(error)
    }

    /// Attach the underlying error. `Display` appends it after the message.
    #[must_use]
    pub(crate) fn with_cause(
        mut self,
        cause: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        self.0.cause = Some(cause.into());
        self
    }

    /// The failure class, which fixes the exit code.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self.0.class {
            Class::Config => ErrorKind::Config,
            Class::Input => ErrorKind::InputRefused,
            Class::Resume(_) => ErrorKind::ResumeRefused,
            Class::Artifact => ErrorKind::ArtifactRefused,
            Class::Io => ErrorKind::Io,
            Class::Internal => ErrorKind::Internal,
            Class::Failed => ErrorKind::Failed,
        }
    }

    /// The reason, when this is a refused resume.
    #[must_use]
    pub fn resume_refusal(&self) -> Option<&ResumeRefusal> {
        match &self.0.class {
            Class::Resume(reason) => Some(reason),
            _ => None,
        }
    }

    /// The message without the cause.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0.message
    }
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.message)?;
        if let Some(cause) = &self.0.cause {
            if self.0.message.is_empty() {
                write!(f, "{cause}")?;
            } else {
                write!(f, ": {cause}")?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for PipelineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0
            .cause
            .as_deref()
            .map(|cause| cause as &(dyn std::error::Error + 'static))
    }
}

/// A bare `io::Error` keeps its own message and is classed by
/// [`ErrorKind::of_io`]: never a domain refusal, since nothing here knows
/// which operation produced it.
impl From<std::io::Error> for PipelineError {
    fn from(error: std::io::Error) -> Self {
        Self::io(String::new(), error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_conversion_needs_os_evidence() {
        let os = std::io::Error::from_raw_os_error(28);
        assert_eq!(PipelineError::from(os).kind(), ErrorKind::Io);
        let synthesized = std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk");
        assert_eq!(PipelineError::from(synthesized).kind(), ErrorKind::Failed);
        let eof = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
        assert_eq!(PipelineError::from(eof).kind(), ErrorKind::Failed);
    }

    #[test]
    fn display_appends_the_cause() {
        let error = PipelineError::failed("PBF read failed")
            .with_cause(std::io::Error::other("short read"));
        assert_eq!(error.to_string(), "PBF read failed: short read");
        let bare = PipelineError::from(std::io::Error::other("plain"));
        assert_eq!(bare.to_string(), "plain");
    }

    #[test]
    fn exit_codes_are_distinct() {
        let kinds = [
            ErrorKind::Config,
            ErrorKind::InputRefused,
            ErrorKind::ResumeRefused,
            ErrorKind::ArtifactRefused,
            ErrorKind::Io,
            ErrorKind::Internal,
            ErrorKind::Failed,
        ];
        let mut codes: Vec<i32> = kinds.iter().map(|k| k.exit_code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), kinds.len());
        assert!(!codes.contains(&0) && !codes.contains(&1));
    }

    #[test]
    fn resume_reason_rides_with_the_kind() {
        let error = PipelineError::resume(ResumeRefusal::InputChanged, "different PBF");
        assert_eq!(error.kind(), ErrorKind::ResumeRefused);
        assert_eq!(error.resume_refusal(), Some(&ResumeRefusal::InputChanged));
        assert_eq!(PipelineError::config("x").resume_refusal(), None);
    }
}
