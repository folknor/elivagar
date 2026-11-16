use rustc_hash::FxHashMap;

use crate::shortbread::Tags;

use crate::shortbread::boundaries::BoundaryMeta;

pub(super) type BoundaryWayMeta = FxHashMap<i64, BoundaryMeta>;

/// Fold one relation's already-filtered tags and way members into the prescan map.
pub(super) fn fold_relation<I>(tags: &Tags<'_>, way_ids: I, out: &mut BoundaryWayMeta)
where
    I: IntoIterator<Item = i64>,
{
    let Some(relation) = BoundaryMeta::from_relation(tags) else {
        return;
    };
    for way_id in way_ids {
        out.entry(way_id).or_default().fold_relation(relation);
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
                min_admin_level: Some(crate::shortbread::boundaries::AdminLevel::Country),
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
