//! Typed failures of persisted sort chunks. Standard I/O carriers retain this
//! error as their source; pipeline classification depends on resume context.
use std::{fmt, io};

/// Evidence established by the chunk store, independent of run/resume context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChunkStoreErrorKind {
    /// A multi-chunk header names another compression algorithm.
    CompressionMismatch,
    /// Duplicate chunk ids or an unsupported filename/split zoom.
    Stale,
    /// A required chunk is absent.
    Missing,
    /// Extra chunks disagree with the expected completion count.
    CountMismatch,
    /// Chunk bytes, section bounds or records were rejected.
    Corrupt,
    /// An underlying I/O failure, with no inferred domain refusal.
    Io,
}

/// A chunk-store rejection or I/O failure, retaining the underlying cause.
#[derive(Debug)]
pub struct ChunkStoreError(Box<Inner>);

#[derive(Debug)]
struct Inner {
    kind: ChunkStoreErrorKind,
    message: String,
    cause: Option<io::Error>,
}

const _: () = assert!(std::mem::size_of::<ChunkStoreError>() == std::mem::size_of::<io::Error>());

impl ChunkStoreError {
    pub(super) fn new(kind: ChunkStoreErrorKind, message: impl Into<String>) -> Self {
        Self(Box::new(Inner {
            kind,
            message: message.into(),
            cause: None,
        }))
    }

    pub(super) fn open(error: io::Error) -> Self {
        let mut result = Self::from(error);
        if result
            .0
            .cause
            .as_ref()
            .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
        {
            result.0.kind = ChunkStoreErrorKind::Missing;
        }
        result
    }

    #[must_use]
    pub fn kind(&self) -> ChunkStoreErrorKind {
        self.0.kind
    }

    // Only chunk byte reads call this. EOF and frame decoder rejections here
    // establish corruption; other errno-free errors remain unclassified I/O.
    pub(super) fn read(error: io::Error) -> Self {
        let kind = if error.raw_os_error().is_none()
            && matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
            ) {
            ChunkStoreErrorKind::Corrupt
        } else {
            ChunkStoreErrorKind::Io
        };
        Self(Box::new(Inner {
            kind,
            message: "reading sort chunk bytes".into(),
            cause: Some(error),
        }))
    }
}

impl From<io::Error> for ChunkStoreError {
    fn from(error: io::Error) -> Self {
        Self(Box::new(Inner {
            kind: ChunkStoreErrorKind::Io,
            message: "sort chunk I/O".into(),
            cause: Some(error),
        }))
    }
}

impl From<ChunkStoreError> for io::Error {
    fn from(error: ChunkStoreError) -> Self {
        Self::other(error)
    }
}

impl fmt::Display for ChunkStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.message)?;
        if let Some(cause) = &self.0.cause {
            write!(f, ": {cause}")?;
        }
        Ok(())
    }
}
impl std::error::Error for ChunkStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.cause.as_ref().map(|e| e as &dyn std::error::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrorKind, PipelineError, ResumeRefusal};
    use std::error::Error;

    #[test]
    fn domain_rejections_need_resume_context_and_survive_io_carriers() {
        for kind in [
            ChunkStoreErrorKind::CompressionMismatch,
            ChunkStoreErrorKind::Stale,
            ChunkStoreErrorKind::Missing,
            ChunkStoreErrorKind::CountMismatch,
            ChunkStoreErrorKind::Corrupt,
        ] {
            for adapted in [false, true] {
                let chunk = ChunkStoreError::new(kind, "rejected chunk");
                let error = if adapted {
                    PipelineError::from(io::Error::other(io::Error::from(chunk)))
                } else {
                    PipelineError::from(chunk)
                };
                assert_eq!(error.kind(), ErrorKind::Failed);
                assert_eq!(error.kind().exit_code(), 2);
                let error = error.chunk_context(true);
                assert_eq!(error.kind().exit_code(), 3);
                assert_eq!(
                    error.resume_refusal(),
                    Some(&ResumeRefusal::ChunkStore { kind })
                );
                assert!(error.source().is_some());
            }
        }
    }

    #[test]
    fn operational_and_unclassified_errors_are_never_resume_refusals() {
        for adapted in [false, true] {
            for os in [false, true] {
                let original = if os {
                    io::Error::from_raw_os_error(13)
                } else {
                    io::Error::other("opaque failure")
                };
                let chunk = ChunkStoreError::read(original);
                let error = if adapted {
                    PipelineError::from(io::Error::from(chunk))
                } else {
                    PipelineError::from(chunk)
                };
                let error = error.chunk_context(true);
                assert_eq!(
                    error.kind(),
                    if os { ErrorKind::Io } else { ErrorKind::Failed }
                );
                assert!(error.resume_refusal().is_none());
                let mut source = error.source();
                let mut found_errno = false;
                while let Some(cause) = source {
                    found_errno |= cause
                        .downcast_ref::<io::Error>()
                        .is_some_and(|e| e.raw_os_error() == Some(13));
                    source = cause.source();
                }
                assert_eq!(found_errno, os);
            }
        }
    }
}
