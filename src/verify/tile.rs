//! The one strict parse every tile check reads.

use crate::mvt::message::{self, ValueMessage};
use crate::tile_detail::Strictness;

/// One layer as verify checks it: the envelope plus every feature parsed,
/// every value decoded and every tag index resolved against the tables.
pub(super) struct CheckedLayer<'a> {
    pub(super) message: message::LayerMessage<'a>,
    pub(super) values: Vec<ValueMessage<'a>>,
    pub(super) features: Vec<CheckedFeature<'a>>,
}

pub(super) struct CheckedFeature<'a> {
    pub(super) message: message::FeatureMessage<'a>,
    pub(super) tags: Vec<(usize, usize)>,
}

/// Parse a tile under the message reader's `Strict` policy and go further
/// than its envelope: every feature and every value-table entry, referenced
/// or not, must decode, and every tag index must name a key and a value.
/// Verify reads elivagar's own output, which writes no field MVT does not
/// name and no field twice.
pub(super) fn parse_checked_tile(data: &[u8]) -> Result<Vec<CheckedLayer<'_>>, String> {
    let strict = Strictness::Strict;
    let mut checked = Vec::new();
    for message in message::parse_tile(data, strict)? {
        let name = message.name;
        let values = message
            .values
            .iter()
            .enumerate()
            .map(|(idx, bytes)| {
                message::decode_value(bytes, strict)
                    .map_err(|e| format!("[{name}] value {idx}: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut features = Vec::with_capacity(message.features.len());
        for (idx, bytes) in message.features.iter().enumerate() {
            let feature = message::parse_feature(bytes, strict)
                .map_err(|e| format!("[{name}] feat {idx}: {e}"))?;
            let tags = message::tag_pairs(&feature.tags)
                .map_err(|e| format!("[{name}] feat {idx}: {e}"))?;
            if let Some(&(key, value)) = tags
                .iter()
                .find(|&&(key, value)| key >= message.keys.len() || value >= values.len())
            {
                return Err(format!(
                    "[{name}] feat {idx}: tag ({key}, {value}) out of bounds"
                ));
            }
            features.push(CheckedFeature {
                message: feature,
                tags,
            });
        }
        checked.push(CheckedLayer {
            message,
            values,
            features,
        });
    }
    Ok(checked)
}
