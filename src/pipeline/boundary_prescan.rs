use rustc_hash::FxHashMap;

use crate::shortbread::Tags;

/// Relation-derived boundary properties resolved onto a member way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct BoundaryMeta {
    pub(super) min_admin_level: Option<u8>,
    pub(super) disputed: bool,
}

pub(super) type BoundaryWayMeta = FxHashMap<i64, BoundaryMeta>;

/// Fold one relation's already-filtered tags and way members into the prescan map.
pub(super) fn fold_relation<I>(tags: &Tags<'_>, way_ids: I, out: &mut BoundaryWayMeta)
where
    I: IntoIterator<Item = i64>,
{
    let admin_level = if tags.has_value("boundary", "administrative") {
        tags.get("admin_level")
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|level| matches!(level, 2 | 4))
    } else {
        None
    };
    let disputed = tags.has_value("boundary", "disputed");
    if admin_level.is_none() && !disputed {
        return;
    }
    for way_id in way_ids {
        let meta = out.entry(way_id).or_default();
        if let Some(level) = admin_level {
            meta.min_admin_level = Some(meta.min_admin_level.map_or(level, |old| old.min(level)));
        }
        meta.disputed |= disputed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_min_admin_level_and_independent_disputed_parentage() {
        let mut out = BoundaryWayMeta::default();
        let admin4 = [("boundary", "administrative"), ("admin_level", "4")];
        let admin2 = [("boundary", "administrative"), ("admin_level", "2")];
        let disputed = [("boundary", "disputed")];
        fold_relation(&Tags(&admin4), [42], &mut out);
        fold_relation(&Tags(&admin2), [42], &mut out);
        fold_relation(&Tags(&disputed), [42], &mut out);
        assert_eq!(
            out[&42],
            BoundaryMeta {
                min_admin_level: Some(2),
                disputed: true
            }
        );
    }

    #[test]
    fn ignores_unsupported_admin_levels() {
        let mut out = BoundaryWayMeta::default();
        let tags = [("boundary", "administrative"), ("admin_level", "6")];
        fold_relation(&Tags(&tags), [42], &mut out);
        assert!(out.is_empty());
    }
}
