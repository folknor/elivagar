//! The comparison-native view of a decoded tile.
//!
//! elivagar's shed `regress.rs` had its own MVT decoder whose output types
//! carried derived fields inline - each ring/component/feature arrived with its
//! bbox, its digest and its structure signature already computed, because the
//! matcher consults all three per candidate edge and recomputing them inside the
//! O(n*k) residual search was never affordable.
//!
//! The public `tile_detail` decoder brokkr links deliberately does none of that:
//! it stops at structure and hands back layers, features, components and rings
//! in **wire order**, leaving "what counts as the same tile" to the caller (see
//! that module's header). So the derivation lives here instead: `prepare`
//! rebuilds the augmented form once per tile, applying the canonicalization the
//! old decoder did at decode time (layers sorted by name, attrs sorted,
//! components sorted through `compare_detail_components`) and folding in the
//! bboxes, digests and structure signatures the engine needs.
//!
//! The digests here are engine-internal bucketing keys, not the canonical
//! semantic hash: they only ever compare two tiles inside one run, and nothing
//! committed depends on their values. This module shares the canonical byte
//! sink, but does not call the frozen `corpus::canonical::semantic_hash` entry.

use std::cmp::Ordering;
use std::sync::Arc;

use super::super::corpus::canon_sink::{CanonSink, HashSink, detail_attrs_hash, write_len};
use super::super::corpus::canonical::{
    CanonicalComponentOrder, CanonicalRingOrder, compare_component_slices,
    compare_detail_components,
};
use super::super::eliv::{self, CanonRingRole, DetailAttr};
use super::geometry::Bbox;

// ---------------------------------------------------------------------------
// Structure signatures: the matcher's cheap "could these two ever pair" key.
// ---------------------------------------------------------------------------

/// Ring count, total vertices and a positional fold of the ring roles. Two
/// components with different signatures can never compare equal, so the residual
/// matcher uses it to partition candidates before it evaluates any Hausdorff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ComponentStructure {
    rings: u32,
    vertices: u32,
    roles: u64,
}

/// The same idea one level up, folding the per-component signatures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct FeatureStructure {
    components: u32,
    rings: u32,
    vertices: u32,
    roles: u64,
}

// ---------------------------------------------------------------------------
// The augmented types.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PreparedRing {
    pub(crate) role: CanonRingRole,
    pub(crate) points: Vec<(i32, i32)>,
    pub(crate) bbox: Bbox,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PreparedComponent {
    pub(crate) rings: Vec<PreparedRing>,
    pub(crate) bbox: Bbox,
    pub(crate) digest: u128,
    pub(crate) structure: ComponentStructure,
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedFeature {
    pub(crate) id: Option<u64>,
    pub(crate) geom_type: u8,
    pub(crate) attrs: Vec<(Arc<str>, DetailAttr)>,
    pub(crate) components: Vec<PreparedComponent>,
    pub(crate) attrs_digest: u128,
    pub(crate) geometry_digest: u128,
    pub(crate) bbox: Bbox,
    pub(crate) structure: FeatureStructure,
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedLayer {
    pub(crate) name: Arc<str>,
    pub(crate) extent: u32,
    pub(crate) version: u32,
    pub(crate) features: Vec<PreparedFeature>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PreparedTile {
    pub(crate) layers: Vec<PreparedLayer>,
}

impl CanonicalRingOrder for PreparedRing {
    fn role(&self) -> eliv::CanonRingRole {
        self.role
    }

    fn points(&self) -> &[(i32, i32)] {
        &self.points
    }
}

impl CanonicalComponentOrder for PreparedComponent {
    type Ring = PreparedRing;

    fn rings(&self) -> &[Self::Ring] {
        &self.rings
    }
}

// ---------------------------------------------------------------------------
// Construction from the wire-order decode.
// ---------------------------------------------------------------------------

/// Decode one decompressed MVT tile into the comparison-native form.
///
/// `Strictness` is the caller's: the corpus gate and `regress` against two
/// elivagar archives decode strict, a cross-producer comparison tolerant.
pub(crate) fn decode_prepared_tile(
    data: &[u8],
    strictness: eliv::Strictness,
) -> Result<PreparedTile, String> {
    Ok(prepare(&eliv::decode_detail_tile(data, strictness)?))
}

/// Rebuild the augmented form over a wire-order `DetailTile`.
///
/// Layers sort by name because `compare_detail_tiles` walks the two layer lists
/// as a merge join; the wire order is the producer's paint order and is not a
/// comparison order.
pub(crate) fn prepare(tile: &eliv::DetailTile) -> PreparedTile {
    let mut layers: Vec<PreparedLayer> = tile.layers.iter().map(prepare_layer).collect();
    layers.sort_by(|a, b| a.name.cmp(&b.name));
    PreparedTile { layers }
}

fn prepare_layer(layer: &eliv::DetailLayer) -> PreparedLayer {
    PreparedLayer {
        name: Arc::clone(&layer.name),
        extent: layer.extent,
        version: layer.version,
        features: layer.features.iter().map(prepare_feature).collect(),
    }
}

fn prepare_feature(feature: &eliv::DetailFeature) -> PreparedFeature {
    // Attrs and components are unordered sets on the wire; sorting them is what
    // makes the digests and the equality comparisons order-insensitive, and it
    // is exactly what the old decoder did before handing a feature back.
    let mut attrs = feature.attrs.clone();
    attrs.sort();
    let mut ordered = feature.components.clone();
    ordered.sort_by(compare_detail_components);
    let components: Vec<PreparedComponent> = ordered.iter().map(prepare_component).collect();

    let attrs_digest = detail_attrs_hash(&attrs);
    let geometry_digest = detail_geometry_hash(feature.geom_type, &components);
    let bbox = components
        .iter()
        .fold(Bbox::empty(), |mut bbox, component| {
            bbox.include_bbox(component.bbox);
            bbox
        });
    let structure = feature_structure(&components);
    PreparedFeature {
        id: feature.id,
        geom_type: feature.geom_type,
        attrs,
        components,
        attrs_digest,
        geometry_digest,
        bbox,
        structure,
    }
}

fn prepare_component(component: &eliv::DetailComponent) -> PreparedComponent {
    let rings: Vec<PreparedRing> = component
        .rings
        .iter()
        .map(|ring| PreparedRing {
            role: ring.role,
            points: ring.points.clone(),
            bbox: Bbox::from_points(&ring.points),
        })
        .collect();
    let bbox = rings.iter().fold(Bbox::empty(), |mut bbox, ring| {
        bbox.include_bbox(ring.bbox);
        bbox
    });
    let structure = component_structure(&rings);
    let mut sink = HashSink::new();
    write_detail_component(&mut sink, &rings);
    PreparedComponent {
        rings,
        bbox,
        digest: sink.finish(),
        structure,
    }
}

fn component_structure(rings: &[PreparedRing]) -> ComponentStructure {
    let mut vertices = 0u32;
    let mut roles = 0u64;
    for ring in rings {
        vertices = vertices.saturating_add(u32::try_from(ring.points.len()).unwrap_or(u32::MAX));
        roles = roles
            .wrapping_mul(5)
            .wrapping_add(u64::from(ring.role as u8) + 1);
    }
    ComponentStructure {
        rings: u32::try_from(rings.len()).unwrap_or(u32::MAX),
        vertices,
        roles,
    }
}

fn feature_structure(components: &[PreparedComponent]) -> FeatureStructure {
    let mut rings = 0u32;
    let mut vertices = 0u32;
    let mut roles = 0u64;
    for component in components {
        rings = rings.saturating_add(component.structure.rings);
        vertices = vertices.saturating_add(component.structure.vertices);
        roles = roles
            .wrapping_mul(31)
            .wrapping_add(component.structure.roles);
    }
    FeatureStructure {
        components: u32::try_from(components.len()).unwrap_or(u32::MAX),
        rings,
        vertices,
        roles,
    }
}

// ---------------------------------------------------------------------------
// Ordering over the prepared form.
// ---------------------------------------------------------------------------

/// Canonical order of two prepared components: ring count, then per-ring role
/// and points. Mirrors `corpus::canonical::compare_detail_components` over the
/// augmented types (the derived fields are functions of these, so comparing them
/// too would only be redundant work).
pub(crate) fn compare_prepared_components(
    a: &PreparedComponent,
    b: &PreparedComponent,
) -> Ordering {
    compare_prepared_component_slices(std::slice::from_ref(a), std::slice::from_ref(b))
}

pub(crate) fn compare_prepared_component_slices(
    a: &[PreparedComponent],
    b: &[PreparedComponent],
) -> Ordering {
    compare_component_slices(a, b)
}

fn write_detail_component(sink: &mut impl CanonSink, rings: &[PreparedRing]) {
    write_len(sink, rings.len());
    for ring in rings {
        sink.bytes(&[ring.role as u8]);
        write_len(sink, ring.points.len());
        for &(x, y) in &ring.points {
            sink.bytes(&x.to_le_bytes());
            sink.bytes(&y.to_le_bytes());
        }
    }
}

fn detail_geometry_hash(geom_type: u8, components: &[PreparedComponent]) -> u128 {
    let mut sink = HashSink::new();
    sink.bytes(&[geom_type]);
    write_len(&mut sink, components.len());
    for component in components {
        write_detail_component(&mut sink, &component.rings);
    }
    sink.finish()
}
