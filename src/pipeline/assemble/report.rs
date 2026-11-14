//! What the ordered writer accepted, tallied once per item.

use crate::archive::PmtilesWriter;
use crate::tiling::{TileId, ZOOM_COUNT};

use super::super::PipelineError;
use super::super::stats::{
    TILE_OVERSIZE_SEVERE_BYTES, TILE_OVERSIZE_WARN_BYTES, TileSizeDiagnostics,
    record_tile_size_diagnostics,
};
use super::EncodedTile;

/// Two accounting rules, kept apart: addressed tiles grow by a run's length,
/// while unique payload counts and bytes grow only when the writer's dedup
/// reports a new payload.
#[derive(Default)]
pub(super) struct WriteTally {
    pub(super) tiles_written: u64,
    tiles_per_zoom: [u64; ZOOM_COUNT],
    unique_per_zoom: [u64; ZOOM_COUNT],
    bytes_per_zoom: [u64; ZOOM_COUNT],
    pub(super) size_diag: TileSizeDiagnostics,
}

impl WriteTally {
    pub(super) fn write_encoded(
        &mut self,
        pmtiles: &mut PmtilesWriter,
        tile: &EncodedTile,
    ) -> Result<(), PipelineError> {
        let (z, x, y) = tile.tile_id.coord();
        let bytes = tile.compressed.len() as u64;
        record_tile_size_diagnostics(&mut self.size_diag, tile.tile_id, bytes);
        let unique = pmtiles.add_tile(z, x, y, &tile.compressed)?;
        self.addressed(z, 1, unique, bytes);
        Ok(())
    }

    /// Copied runs never cross a zoom boundary (the artifact planner splits
    /// them there), so one zoom's counters take the whole run.
    pub(super) fn write_run_copy(
        &mut self,
        pmtiles: &mut PmtilesWriter,
        tile_id: TileId,
        run_length: u32,
        data: &[u8],
    ) -> Result<(), PipelineError> {
        let z = tile_id.zoom();
        let bytes = u64::try_from(data.len())
            .map_err(|_| PipelineError::internal("tile payload length exceeds u64"))?;
        let unique = pmtiles.add_run(tile_id, run_length, data)?;
        self.addressed(z, u64::from(run_length), unique, bytes);
        record_tile_size_diagnostics(&mut self.size_diag, tile_id, bytes);
        if run_length > 1 {
            let rest = u64::from(run_length - 1);
            self.size_diag.total_tile_bytes += bytes * rest;
            if bytes > TILE_OVERSIZE_WARN_BYTES {
                self.size_diag.oversize_warn_count += rest;
            }
            if bytes > TILE_OVERSIZE_SEVERE_BYTES {
                self.size_diag.oversize_severe_count += rest;
            }
        }
        Ok(())
    }

    fn addressed(&mut self, z: u8, tiles: u64, unique: bool, bytes: u64) {
        self.tiles_written += tiles;
        self.tiles_per_zoom[z as usize] += tiles;
        if unique {
            self.unique_per_zoom[z as usize] += 1;
            self.bytes_per_zoom[z as usize] += bytes;
        }
    }

    pub(super) fn print_per_zoom(&self, min_zoom: u8, max_zoom: u8) {
        eprintln!("  Per-zoom tiles (total / unique / unique MB):");
        for z in min_zoom..=max_zoom {
            let total = self.tiles_per_zoom[z as usize];
            let unique = self.unique_per_zoom[z as usize];
            let mb = self.bytes_per_zoom[z as usize] as f64 / (1024.0 * 1024.0);
            if total > 0 {
                eprintln!("    z{z:2}: {total:>8} / {unique:>8} / {mb:>7.1} MB");
            }
        }
    }
}
