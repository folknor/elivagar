//! Within-layer paint order. The rule - which layers are ordered and each
//! kind's rank - is the Shortbread profile's (`Layer::has_paint_order`,
//! `Layer::paint_rank_for`); this module only reads the rank attributes off
//! the wire, which is why the attribute extraction stays here rather than
//! coupling `shortbread/` to MVT value messages.

use super::finding::{Finding, PaintOrderError};
use super::tile::{CheckedFeature, CheckedLayer};
use crate::mvt::message::ValueMessage;
use crate::shortbread::{Layer, RankAttributes, RankError};

/// Ranks never decrease in wire order; the first violation, or the first
/// feature whose rank cannot be recovered. Unordered layers pass.
pub(super) fn check_layer(layer: &CheckedLayer<'_>) -> Result<(), Finding> {
    let Some(rank_layer) = Layer::from_name(layer.message.name).filter(|l| l.has_paint_order())
    else {
        return Ok(());
    };
    let mut previous = None;
    for (feature_idx, feature) in layer.features.iter().enumerate() {
        let finding = |cause| Finding::PaintOrder {
            layer: layer.message.name.to_string(),
            feature: feature_idx,
            cause,
        };
        let attrs = paint_attrs(layer, feature).map_err(finding)?;
        let kind = attrs
            .kind
            .ok_or_else(|| finding(PaintOrderError::MissingKind))?;
        let rank = match rank_layer.paint_rank_for(&RankAttributes {
            kind,
            link: attrs.link,
            tunnel: attrs.tunnel,
            bridge: attrs.bridge,
        }) {
            Ok(Some(rank)) => rank,
            Ok(None) => unreachable!("layer was filtered to ordered layers"),
            Err(RankError::UnknownLandKind) => {
                return Err(finding(PaintOrderError::UnknownLandKind(kind.to_string())));
            }
            Err(RankError::UnknownStreetKind) => {
                return Err(finding(PaintOrderError::UnknownStreetKind(
                    kind.to_string(),
                )));
            }
        };
        if let Some(previous) = previous
            && rank < previous
        {
            return Err(finding(PaintOrderError::DecreasingRank { rank, previous }));
        }
        previous = Some(rank);
    }
    Ok(())
}

#[derive(Default)]
struct PaintAttrs<'a> {
    kind: Option<&'a str>,
    link: bool,
    tunnel: bool,
    bridge: bool,
    seen_link: bool,
    seen_tunnel: bool,
    seen_bridge: bool,
}

/// The paint-rank attributes of one feature; `parse_checked_tile` has
/// already bounds-checked every tag index.
fn paint_attrs<'a>(
    layer: &CheckedLayer<'a>,
    feature: &CheckedFeature<'a>,
) -> Result<PaintAttrs<'a>, PaintOrderError> {
    let mut attrs = PaintAttrs::default();
    for &(key_idx, value_idx) in &feature.tags {
        let key = &layer.message.keys[key_idx];
        let value = layer.values[value_idx];
        match *key {
            "kind" => match value {
                ValueMessage::String(kind) if attrs.kind.replace(kind).is_none() => {}
                ValueMessage::String(_) => {
                    return Err(PaintOrderError::DuplicateAttribute("kind".to_string()));
                }
                _ => {
                    return Err(PaintOrderError::WrongAttributeType {
                        attribute: "kind".to_string(),
                        expected: "string",
                    });
                }
            },
            "link" | "tunnel" | "bridge" => {
                let flag = match value {
                    ValueMessage::Bool(flag) => flag,
                    _ => {
                        return Err(PaintOrderError::WrongAttributeType {
                            attribute: key.to_string(),
                            expected: "bool",
                        });
                    }
                };
                let (slot, seen) = match *key {
                    "link" => (&mut attrs.link, &mut attrs.seen_link),
                    "tunnel" => (&mut attrs.tunnel, &mut attrs.seen_tunnel),
                    "bridge" => (&mut attrs.bridge, &mut attrs.seen_bridge),
                    _ => unreachable!(),
                };
                if *seen {
                    return Err(PaintOrderError::DuplicateAttribute(key.to_string()));
                }
                *slot = flag;
                *seen = true;
            }
            _ => {}
        }
    }
    Ok(attrs)
}
