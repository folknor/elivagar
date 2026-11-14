//! Machine-readable verification findings; Display is only presentation.
use crate::DecodeError;
use std::{fmt, sync::Arc};

#[derive(Debug)]
pub enum PaintOrderError {
    MissingKind,
    DuplicateAttribute(String),
    WrongAttributeType {
        attribute: String,
        expected: &'static str,
    },
    UnknownLandKind(String),
    UnknownStreetKind(String),
    DecreasingRank {
        rank: u8,
        previous: u8,
    },
}
impl fmt::Display for PaintOrderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingKind => write!(f, "missing kind attribute"),
            Self::DuplicateAttribute(a) => write!(f, "duplicate {a} attribute"),
            Self::WrongAttributeType {
                attribute,
                expected,
            } => write!(f, "{attribute} attribute is not a {expected}"),
            Self::UnknownLandKind(k) => write!(f, "unknown land kind {k:?}"),
            Self::UnknownStreetKind(k) => write!(f, "unknown street kind {k:?}"),
            Self::DecreasingRank { rank, previous } => {
                write!(f, "paint rank {rank} follows {previous}")
            }
        }
    }
}
impl std::error::Error for PaintOrderError {}

#[derive(Debug)]
pub enum Finding {
    Payload(DecodeError),
    Mvt(DecodeError),
    NoLayers,
    EmptyLayerName,
    ZeroExtent {
        layer: String,
    },
    Geometry {
        layer: String,
        feature: usize,
        cause: DecodeError,
    },
    PaintOrder {
        layer: String,
        feature: usize,
        cause: PaintOrderError,
    },
    OceanDecode {
        feature: usize,
        cause: DecodeError,
    },
    OceanSelfIntersection {
        feature: usize,
        ring: usize,
        vertices: usize,
    },
}
impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Payload(e) => write!(f, "payload decode failed: {e}"),
            Self::Mvt(e) => write!(f, "MVT decode failed: {e}"),
            Self::NoLayers => write!(f, "no MVT layers"),
            Self::EmptyLayerName => write!(f, "empty layer name"),
            Self::ZeroExtent { layer } => write!(f, "[{layer}] layer extent is zero"),
            Self::Geometry {
                layer,
                feature,
                cause,
            } => write!(f, "[{layer}] feat {feature}: {cause}"),
            Self::PaintOrder {
                layer,
                feature,
                cause,
            } => write!(f, "[{layer}] feat {feature}: {cause}"),
            Self::OceanDecode { feature, cause } => {
                write!(f, "ocean feat {feature} polygon decode failed: {cause}")
            }
            Self::OceanSelfIntersection {
                feature,
                ring,
                vertices,
            } => write!(
                f,
                "ocean feat {feature} ring {ring} is self-intersecting ({vertices} verts)"
            ),
        }
    }
}
impl std::error::Error for Finding {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Payload(e) | Self::Mvt(e) => Some(e),
            Self::Geometry { cause, .. } | Self::OceanDecode { cause, .. } => Some(cause),
            Self::PaintOrder { cause, .. } => Some(cause),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct TileFinding {
    pub z: u8,
    pub x: u32,
    pub y: u32,
    /// Shared by addressed tiles using the same payload and validation class.
    pub finding: Arc<Finding>,
}
impl fmt::Display for TileFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "z{}/{}/{}: {}", self.z, self.x, self.y, self.finding)
    }
}
