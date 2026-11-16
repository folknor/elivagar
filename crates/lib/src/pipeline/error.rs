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
    /// input without the opt-in, a malformed ocean shapefile).
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
    /// The retained sort chunk store is absent, incompatible or corrupt.
    ChunkStore {
        kind: crate::sort::ChunkStoreErrorKind,
    },
    /// The chunks were built from a different input PBF.
    InputChanged,
    /// The chunks were produced under a different producer config; the
    /// differing fields are named.
    ProducerConfigChanged { fields: Vec<String> },
    /// The ocean chunks were produced from a different ocean input.
    OceanChanged,
}

/// Why the named ocean artifact cannot serve the run.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArtifactRefusal {
    Missing,
    Format,
    Layout,
    Metadata,
    Coverage,
    Directory,
    Payload,
    KeyMismatch(KeyMismatch),
}

/// All differing fields of an artifact's declared identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyMismatch {
    pub fields: Vec<&'static str>,
}

#[derive(Debug)]
enum Class {
    Config,
    Input,
    Resume(ResumeRefusal),
    Artifact(ArtifactRefusal),
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
        Self::artifact_refused(ArtifactRefusal::Payload, message)
    }

    pub(crate) fn artifact_refused(reason: ArtifactRefusal, message: impl Into<String>) -> Self {
        Self::new(Class::Artifact(reason), message)
    }

    /// The reason, when the named ocean artifact was refused.
    #[must_use]
    pub fn artifact_refusal(&self) -> Option<&ArtifactRefusal> {
        match &self.0.class {
            Class::Artifact(reason) => Some(reason),
            _ => None,
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(Class::Internal, message)
    }

    pub(crate) fn failed(message: impl Into<String>) -> Self {
        Self::new(Class::Failed, message)
    }

    /// An `io::Error` under a context message, classed by
    /// [`ErrorKind::of_io`] unless a typed decode or chunk-store cause
    /// establishes more.
    pub(crate) fn io(message: impl Into<String>, error: std::io::Error) -> Self {
        let class = match ErrorKind::of_io(&error) {
            ErrorKind::Io => Class::Io,
            _ => Class::Failed,
        };
        Self::new(class, message)
            .with_cause(error)
            .decode_context()
            .chunk_context(false)
    }

    fn decode_context(mut self) -> Self {
        if let Some(error) = self
            .0
            .cause
            .as_deref()
            .and_then(|e| crate::DecodeError::find(e))
        {
            self.0.class = match error.kind() {
                crate::DecodeErrorKind::Malformed | crate::DecodeErrorKind::Unsupported => {
                    Class::Input
                }
                crate::DecodeErrorKind::Io => Class::Io,
                crate::DecodeErrorKind::Failed => Class::Failed,
            };
        }
        self
    }

    // Called at the run boundary as well as conversion: lazy merge errors
    // travel through assembly's standard I/O carriers before reaching here.
    pub(crate) fn chunk_context(mut self, resumed: bool) -> Self {
        fn find<'a>(
            error: &'a (dyn std::error::Error + 'static),
        ) -> Option<&'a crate::sort::ChunkStoreError> {
            if let Some(chunk) = error.downcast_ref::<crate::sort::ChunkStoreError>() {
                return Some(chunk);
            }
            if let Some(io) = error.downcast_ref::<std::io::Error>()
                && let Some(inner) = io.get_ref()
                && let Some(chunk) = find(inner)
            {
                return Some(chunk);
            }
            error.source().and_then(find)
        }
        if let Some(chunk) = self.0.cause.as_deref().and_then(|e| find(e)) {
            let kind = chunk.kind();
            if kind != crate::sort::ChunkStoreErrorKind::Io {
                self.0.class = if resumed {
                    Class::Resume(ResumeRefusal::ChunkStore { kind })
                } else {
                    Class::Failed
                };
            } else {
                // Retain OS evidence even across standard I/O adapters.
                let mut cause = std::error::Error::source(chunk);
                while let Some(error) = cause {
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.raw_os_error().is_some())
                    {
                        self.0.class = Class::Io;
                        break;
                    }
                    cause = error.source();
                }
            }
        }
        self
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
            Class::Artifact(_) => ErrorKind::ArtifactRefused,
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
/// which operation produced it. A typed decode or chunk-store cause is
/// separate producer evidence and retains its classification through the carrier.
impl From<std::io::Error> for PipelineError {
    fn from(error: std::io::Error) -> Self {
        Self::io(String::new(), error)
    }
}

impl From<crate::sort::ChunkStoreError> for PipelineError {
    fn from(error: crate::sort::ChunkStoreError) -> Self {
        Self::failed("sort chunk store")
            .with_cause(error)
            .chunk_context(false)
    }
}

/// Node-ID rejections are refused input; growth and remap failures keep the
/// ordinary I/O evidence rule.
impl From<crate::node_index::NodeStoreError> for PipelineError {
    fn from(error: crate::node_index::NodeStoreError) -> Self {
        use crate::node_index::NodeStoreError;
        let class = match &error {
            NodeStoreError::Grow(io) | NodeStoreError::Remap(io) => match ErrorKind::of_io(io) {
                ErrorKind::Io => Class::Io,
                _ => Class::Failed,
            },
            _ => Class::Input,
        };
        Self::new(class, "node store").with_cause(error)
    }
}

/// Shapefile bytes the reader refused are refused input; a failure to obtain
/// the bytes keeps the ordinary I/O evidence rule.
impl From<crate::ocean::ShapefileError> for PipelineError {
    fn from(error: crate::ocean::ShapefileError) -> Self {
        use crate::ocean::ShapefileError;
        match error {
            ShapefileError::Malformed(_) => Self::input(String::new()).with_cause(error),
            ShapefileError::Read(io) => Self::from(io),
        }
    }
}

/// The ring-cap partition's indivisible-rect state is proven unreachable at
/// its producer, so reaching it is an internal invariant violation.
impl From<crate::geometry::pyramid::RingCapError> for PipelineError {
    fn from(error: crate::geometry::pyramid::RingCapError) -> Self {
        Self::internal("polygon emission").with_cause(error)
    }
}

impl From<crate::DecodeError> for PipelineError {
    fn from(error: crate::DecodeError) -> Self {
        use crate::DecodeErrorKind;
        let result = match error.kind() {
            DecodeErrorKind::Malformed | DecodeErrorKind::Unsupported => {
                Self::input("input decode failed")
            }
            DecodeErrorKind::Io => Self::new(Class::Io, "archive read failed"),
            DecodeErrorKind::Failed => Self::failed("archive read failed"),
        };
        result.with_cause(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_cap_and_node_store_errors_keep_their_class_and_cause() {
        use crate::geometry::int_ocean::IntRect;
        use crate::geometry::pyramid::RingCapError;
        use crate::node_index::NodeStoreError;
        let ring_cap = RingCapError {
            rect: IntRect {
                min_x: 0,
                min_y: 0,
                max_x: 1,
                max_y: 1,
            },
            contours: 501,
        };
        let err = PipelineError::from(ring_cap);
        assert_eq!(err.kind(), ErrorKind::Internal);
        assert_eq!(err.kind().exit_code(), 70);
        assert!(
            std::error::Error::source(&err)
                .and_then(|e| e.downcast_ref::<RingCapError>())
                .is_some()
        );

        let cap = NodeStoreError::SafetyCap {
            node_id: 1,
            needed_bytes: 2,
            cap_bytes: 1,
        };
        assert_eq!(PipelineError::from(cap).kind(), ErrorKind::InputRefused);
        let order = NodeStoreError::NotIncreasing {
            node_id: 1,
            previous: 2,
        };
        assert_eq!(PipelineError::from(order).kind(), ErrorKind::InputRefused);
        let os = NodeStoreError::Grow(std::io::Error::from_raw_os_error(28));
        assert_eq!(PipelineError::from(os).kind(), ErrorKind::Io);
        let opaque = NodeStoreError::Remap(std::io::Error::other("opaque"));
        assert_eq!(PipelineError::from(opaque).kind(), ErrorKind::Failed);
    }

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
