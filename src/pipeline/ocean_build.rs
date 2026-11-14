// The world-ocean artifact build. It orchestrates the ocean passes, the
// external sort and partitioned assembly, so it lives beside `run` in the
// pipeline rather than under `ocean/`, which the pipeline depends on and
// which must not depend back on assembly.

use super::{
    OceanMode, PipelineError, SORT_CHUNKS_DIR, TileCompression, TilePayloadFormat, TilegenConfig,
    assemble, validate_compression_level,
};
use crate::geometry::projection::MercBbox;
use crate::shortbread;
use crate::sort;
use crate::tiling::ZoomRange;
use std::path::{Path, PathBuf};

/// Build the durable world-ocean artifact without opening an OSM PBF. It uses
/// the normal ocean, external-sort, and partitioned assembly path so artifact
/// bytes obey the same encoding and compression rules as runtime ocean tiles.
#[allow(clippy::too_many_lines)]
pub fn ocean_build(
    full_shapefile: &Path,
    simplified_shapefile: Option<&Path>,
    output_path: &Path,
    tmp_dir: &Path,
    compression_level: u32,
    threads: std::num::NonZeroUsize,
) -> Result<(), PipelineError> {
    validate_compression_level(compression_level)?;
    crate::debug::reset_run_counters();
    crate::ocean::reset_ocean_counters();
    sort::reset_sort_counters();
    let plan = crate::ocean::OceanPlan::new(Some(full_shapefile), simplified_shapefile, None)
        .map_err(PipelineError)?
        .ok_or_else(|| {
            PipelineError("ocean-build needs a full-resolution shapefile".to_string())
        })?;
    // Fail on missing inputs before retaining the active artifact. The key
    // itself is assembled from the buffers consumed by the passes below.
    for pass in crate::ocean::OceanPass::ALL {
        let files = plan.source(pass);
        drop(std::fs::File::open(files.shp())?);
        drop(std::fs::File::open(files.shx())?);
    }
    crate::ocean::retention::retain_outgoing_ocean_artifact(output_path)?;
    drop(std::fs::remove_dir_all(tmp_dir));
    let chunks_dir = tmp_dir.join(SORT_CHUNKS_DIR);
    let mut writer = sort::SortWriter::new(
        &chunks_dir,
        crate::resources::DEFAULT_SORT_BUDGET,
        sort::ChunkCompression::None,
    )?;
    let world = MercBbox {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 1.0,
        max_y: 1.0,
    };
    // The same passes a run executes, over the artifact's whole zoom range.
    let artifact_zooms = ZoomRange::new(
        crate::ocean::artifact::ARTIFACT_MIN_ZOOM,
        crate::ocean::artifact::ARTIFACT_MAX_ZOOM,
    )
    .expect("the artifact zoom contract is a profile range");
    let mut pass_hashes = Vec::with_capacity(crate::ocean::OceanPass::ALL.len());
    for pass in crate::ocean::OceanPass::ALL {
        let window = pass.emission(artifact_zooms).ok_or_else(|| {
            PipelineError(format!("artifact zoom range misses the {pass:?} pass"))
        })?;
        let (_, hashes) = crate::ocean::process_ocean_shapefile_with_hashes(
            plan.source(pass).shp(),
            &world,
            window,
            &mut writer,
        )?;
        pass_hashes.push(hashes);
    }
    let [low_hashes, full_hashes] = pass_hashes[..] else {
        return Err(PipelineError(
            "ocean-build ran an unexpected number of passes".to_string(),
        ));
    };
    if !plan.has_simplified() && low_hashes != full_hashes {
        return Err(PipelineError(
            "full ocean shapefile changed between low and high zoom passes".to_string(),
        ));
    }
    let source = crate::ocean::OceanSourceKey::from_hashes(
        full_hashes,
        plan.has_simplified().then_some(low_hashes),
        crate::ocean::artifact::ARTIFACT_MIN_ZOOM,
        crate::ocean::artifact::ARTIFACT_MAX_ZOOM,
    );
    let key = crate::ocean::OceanArtifactKey {
        source,
        compression_level,
    };
    writer.flush()?;
    let mut reader = writer.finish()?;
    let config = TilegenConfig {
        pbf_path: PathBuf::new(),
        output_path: output_path.to_path_buf(),
        tmp_dir: tmp_dir.to_path_buf(),
        min_zoom: crate::ocean::artifact::ARTIFACT_MIN_ZOOM,
        max_zoom: crate::ocean::artifact::ARTIFACT_MAX_ZOOM,
        ocean_shapefile: None,
        ocean_simplified_shapefile: None,
        ocean_tiles: None,
        ocean_artifact_key: Some(key.clone()),
        ocean_only_metadata: true,
        skip_to: None,
        in_memory: false,
        compression_level,
        force_sorted: false,
        allow_unsafe_flat_index: false,
        threads,
        way_inflight_budget: None,
        assemble_batch_budget: None,
        dedup_cap: None,
        sort_chunk_size: None,
        locations_on_ways: false,
        tile_format: TilePayloadFormat::Mvt,
        tile_compression: TileCompression::Gzip,
        compress_sort_chunks: sort::ChunkCompression::None,
        fanout_caps: [None; shortbread::Layer::count()],
        polygon_simplify_factor: 1.0,
    };
    let resources = crate::resources::Resources::resolve(&config)?;
    resources.emit();
    // The durable artifact has no input PBF. Its top-level ocean_artifact key
    // identifies its shapefile sources and policy without changing the wire
    // metadata consumed by brokkr and OceanTiles::open. Assembly counts the
    // expected absence of run-shaped provenance without opening a PBF.
    let _ = assemble::phase_assemble_with_ocean(
        &mut reader,
        &config,
        &resources,
        &world,
        None,
        assemble::RunProvenance {
            input_identity: None,
            effective: None,
            resumed_from: None,
            ocean_mode: OceanMode::Band { key },
            low_zoom_source: Some(plan.low_zoom_source(crate::ocean::artifact::ARTIFACT_MIN_ZOOM)),
        },
    )?;
    Ok(())
}
