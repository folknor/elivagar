//! Typed failures while reading archive and vector tile bytes.
use std::{error::Error, fmt, io};

/// The evidence established by a decoding operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DecodeErrorKind {
    /// Bytes violate the format or its bounds.
    Malformed,
    /// The declared format, codec or layout is not supported.
    Unsupported,
    /// An underlying operation carries OS failure evidence.
    Io,
    /// A failure without evidence for a more specific class.
    Failed,
}

/// A decode failure, its operation locus, and its original cause.
#[derive(Debug)]
pub struct DecodeError(Box<Detail>);
#[derive(Debug)]
struct Detail {
    kind: DecodeErrorKind,
    locus: String,
    message: String,
    cause: Option<Box<dyn Error + Send + Sync>>,
}
const _: () = assert!(std::mem::size_of::<DecodeError>() == std::mem::size_of::<io::Error>());

impl DecodeError {
    pub(crate) fn new(
        kind: DecodeErrorKind,
        locus: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self(Box::new(Detail {
            kind,
            locus: locus.into(),
            message: message.into(),
            cause: None,
        }))
    }
    pub(crate) fn malformed(locus: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(DecodeErrorKind::Malformed, locus, message)
    }
    pub(crate) fn caused(
        locus: impl Into<String>,
        cause: impl Error + Send + Sync + 'static,
    ) -> Self {
        let mut error = Self::malformed(locus, "");
        error.0.cause = Some(Box::new(cause));
        error
    }
    pub(crate) fn set_kind(&mut self, kind: DecodeErrorKind) {
        self.0.kind = kind;
    }

    pub(crate) fn find<'a>(error: &'a (dyn Error + 'static)) -> Option<&'a DecodeError> {
        if let Some(decoded) = error.downcast_ref::<DecodeError>() {
            return Some(decoded);
        }
        if let Some(io) = error.downcast_ref::<io::Error>()
            && let Some(inner) = io.get_ref()
            && let Some(decoded) = Self::find(inner)
        {
            return Some(decoded);
        }
        error.source().and_then(Self::find)
    }

    /// The failure class, without parsing the diagnostic.
    #[must_use]
    pub fn kind(&self) -> DecodeErrorKind {
        self.0.kind
    }
    /// The section or message operation that failed.
    #[must_use]
    pub fn locus(&self) -> &str {
        &self.0.locus
    }
}
impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.0.locus, self.0.message)?;
        if let Some(cause) = &self.0.cause {
            write!(f, "{cause}")?;
        }
        Ok(())
    }
}
impl Error for DecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.cause.as_deref().map(|e| e as &(dyn Error + 'static))
    }
}
impl From<io::Error> for DecodeError {
    fn from(error: io::Error) -> Self {
        let kind = if let Some(decoded) = Self::find(&error) {
            decoded.kind()
        } else if error.raw_os_error().is_some() {
            DecodeErrorKind::Io
        } else {
            DecodeErrorKind::Failed
        };
        let mut result = Self::caused("archive I/O", error);
        result.0.kind = kind;
        result
    }
}
// Standard I/O adapters retain the typed error as their source.
impl From<DecodeError> for io::Error {
    fn from(error: DecodeError) -> Self {
        io::Error::other(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrorKind, PipelineError};

    #[test]
    fn typed_evidence_survives_classification_and_io_carriers() {
        let error = DecodeError::malformed("directory", "invalid routing");
        assert_eq!(error.kind(), DecodeErrorKind::Malformed);
        assert_eq!(error.locus(), "directory");
        let carrier = io::Error::from(error);
        assert!(carrier.get_ref().expect("typed cause").is::<DecodeError>());
        assert_eq!(PipelineError::from(carrier).kind(), ErrorKind::InputRefused);
        let carrier = io::Error::other(io::Error::from(DecodeError::from(
            io::Error::from_raw_os_error(5),
        )));
        assert_eq!(PipelineError::from(carrier).kind(), ErrorKind::Io);

        let error = DecodeError::from(io::Error::from_raw_os_error(5));
        assert_eq!(error.kind(), DecodeErrorKind::Io);
        assert_eq!(
            error
                .source()
                .expect("original errno")
                .downcast_ref::<io::Error>()
                .expect("I/O source")
                .raw_os_error(),
            Some(5)
        );
        assert_eq!(PipelineError::from(error).kind(), ErrorKind::Io);

        let error = DecodeError::from(io::Error::other("opaque failure"));
        assert_eq!(PipelineError::from(error).kind(), ErrorKind::Failed);
        for kind in [DecodeErrorKind::Malformed, DecodeErrorKind::Unsupported] {
            let error = DecodeError::new(kind, "header", "rejected bytes");
            assert_eq!(PipelineError::from(error).kind(), ErrorKind::InputRefused);
        }
    }
}
