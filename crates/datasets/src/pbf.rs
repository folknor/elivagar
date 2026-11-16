//! What `download` does to a PBF, through pbfhogg's library: read the
//! replication fields of its header, and derive the `indexed` and
//! `locations` variants the hosts register beside `raw`.
//!
//! The derivations are the ones every host's variants were made with:
//! - `indexed` is `pbfhogg cat RAW -o INDEXED`: a passthrough that reframes
//!   each unindexed blob with blob-level indexdata, compression unchanged
//!   (pbfhogg's default, zlib 6, for any blob it re-encodes).
//! - `locations` is `pbfhogg add-locations-to-ways INDEXED -o LOCATIONS
//!   --index-type external --inject-prepass --compression zstd:1`: node
//!   coordinates on the ways, plus pbfhogg's WayMembers-v1 and
//!   SharedNodePins-v1 prepass metadata, which elivagar reads instead of
//!   running its own relation prepass. Untagged nodes are dropped and an
//!   unresolvable way reference fails the run, the CLI's defaults.
//!
//! Each output is a pbfhogg `Output` staged beside its destination and
//! published only on success, so a failed derivation leaves nothing at the
//! destination; the external join's scratch is a pbfhogg `RunScratch` under
//! the host's scratch dir, removed when the run ends.
//!
//! Geofabrik's replication sequence is read from the file's own header,
//! rather than from the stream's `state.txt` beside the download, which
//! cannot race Geofabrik's nightly publish.

use std::path::Path;
use std::sync::{Arc, Once};

use pbfhogg::altw::{AltwOptions, IndexType};
use pbfhogg::host::{
    DiagnosticSink, Diagnostics, Event, EventKind, Executor, Host, Invocation, MachineProbe,
    NoProcessActions, PageCachePolicy,
};
use pbfhogg::write::WriteOptions;
use pbfhogg::writer::Compression;
use pbfhogg::{ElementReader, HeaderOverrides, Output, Overwrite};

/// The replication fields of a PBF's header.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Replication {
    pub(crate) seq: Option<u64>,
    pub(crate) base_url: Option<String>,
}

fn report(path: &Path, what: &str, e: &pbfhogg::Error) -> String {
    format!("{}: {what}: {}", path.display(), e.report())
}

/// Open `path` as a PBF and read its header. A file whose first blob is not
/// an `OSMHeader` (an HTML error page, a truncated transfer) is refused.
pub(crate) fn replication(path: &Path) -> Result<Replication, String> {
    let reader =
        ElementReader::from_path(path).map_err(|e| report(path, "not a readable PBF", &e))?;
    let header = reader.header();
    Ok(Replication {
        // A negative sequence is no sequence: a wrong seq is worse than none.
        seq: header
            .osmosis_replication_sequence_number()
            .and_then(|s| u64::try_from(s).ok()),
        base_url: header.osmosis_replication_base_url().map(str::to_owned),
    })
}

/// pbfhogg's warnings and notes, on stderr in this tool's voice. Progress
/// and developer events are dropped.
struct Stderr;

impl DiagnosticSink for Stderr {
    fn event(&self, event: &Event<'_>) {
        if matches!(
            event.kind,
            EventKind::Warning | EventKind::Note | EventKind::Summary
        ) {
            crate::fetch::note(&format!("pbfhogg: {}", event.args));
        }
    }
}

/// A host for one pbfhogg invocation: this machine's cores and memory,
/// rayon's ambient pool, the page cache left alone, and a fresh token.
fn host() -> Host {
    // The external join holds many descriptors at once. pbfhogg never
    // raises the limit itself; its own CLI raises it to the hard cap, and
    // so does this embedding.
    static RAISE: Once = Once::new();
    RAISE.call_once(|| {
        let allowance = pbfhogg::process::raise_nofile_to_hard_cap();
        crate::fetch::note(&format!("pbfhogg {allowance}"));
    });
    Host {
        diagnostics: Diagnostics::new(Arc::new(Stderr)),
        process: Arc::new(NoProcessActions),
        resources: Arc::new(MachineProbe),
        page_cache: PageCachePolicy::Keep,
        executor: Executor::Ambient,
        invocation: Invocation::new(),
    }
}

fn output(host: &Host, dest: &Path) -> Result<Output, String> {
    Output::new(dest, Overwrite::Refuse, &host.diagnostics)
        .map_err(|e| report(dest, "cannot stage the output", &e))
}

/// `pbfhogg cat RAW -o DEST`.
pub(crate) fn index(raw: &Path, dest: &Path) -> Result<(), String> {
    let host = host();
    let out = output(&host, dest)?;
    pbfhogg::cat::cat(
        &[raw],
        out,
        None,
        &pbfhogg::cat::CleanAttrs::default(),
        &WriteOptions::default(),
        &HeaderOverrides::default(),
        &host,
    )
    .map(drop)
    .map_err(|e| report(raw, "pbfhogg cat", &e))
}

/// `pbfhogg add-locations-to-ways INDEXED -o DEST --index-type external
/// --inject-prepass --compression zstd:1`, its scratch under
/// `scratch_parent`. The published output must declare `LocationsOnWays`,
/// which is what tilegen detects it by.
pub(crate) fn add_locations(
    indexed: &Path,
    dest: &Path,
    scratch_parent: &Path,
) -> Result<(), String> {
    let host = host();
    let out = output(&host, dest)?;
    std::fs::create_dir_all(scratch_parent)
        .map_err(|e| format!("{}: {e}", scratch_parent.display()))?;
    let scratch = pbfhogg::scratch::RunScratch::create(scratch_parent, &host)
        .map_err(|e| report(scratch_parent, "cannot create pbfhogg scratch", &e))?;
    let options = AltwOptions {
        keep_untagged_nodes: false,
        write: WriteOptions {
            compression: Compression::Zstd(1),
            ..WriteOptions::default()
        },
        index_type: IndexType::External,
        inject_prepass: true,
        ignore_missing_nodes: false,
    };
    pbfhogg::altw::add_locations_to_ways(
        indexed,
        out,
        &options,
        &HeaderOverrides::default(),
        &host,
        scratch,
    )
    .map(drop)
    .map_err(|e| report(indexed, "pbfhogg add-locations-to-ways", &e))?;
    let reader = ElementReader::from_path(dest).map_err(|e| report(dest, "reading it back", &e))?;
    if !reader.header().has_locations_on_ways() {
        return Err(format!(
            "{}: add-locations-to-ways published a file without LocationsOnWays",
            dest.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod build {
    //! Tiny Geofabrik-shaped PBFs for tests, written with pbfhogg's own
    //! writer: sorted, no indexdata (as Geofabrik ships them), three nodes
    //! and one way inside a few hundred metres of Valletta.

    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
    use pbfhogg::writer::{Compression, PbfWriter};

    /// `seq` and `url` go to the header; `missing_ref` makes the way
    /// reference a node the file lacks, which add-locations-to-ways refuses.
    pub(crate) fn pbf(seq: Option<i64>, url: Option<&str>, missing_ref: bool) -> Vec<u8> {
        let mut header = HeaderBuilder::new()
            .sorted()
            .bbox(14.50, 35.89, 14.52, 35.91)
            // A replication timestamp distinct from the seq, so a reader
            // taking one field for the other fails.
            .replication_timestamp(1_794_777_748);
        if let Some(seq) = seq {
            header = header.replication_sequence_number(seq);
        }
        if let Some(url) = url {
            header = header.replication_base_url(url);
        }
        let header = header.build().expect("pbf header");

        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut writer = PbfWriter::new(&mut buf, Compression::default());
            writer.write_header(&header).expect("write header");
            let mut bb = BlockBuilder::new();
            bb.add_node(1, 358_990_000, 145_100_000, [("amenity", "cafe")], None);
            bb.add_node(
                2,
                358_995_000,
                145_105_000,
                std::iter::empty::<(&str, &str)>(),
                None,
            );
            bb.add_node(
                3,
                359_000_000,
                145_110_000,
                std::iter::empty::<(&str, &str)>(),
                None,
            );
            let nodes = bb.take().expect("node block").expect("nodes");
            writer
                .write_primitive_block_no_indexdata(nodes)
                .expect("write nodes");
            let refs: &[i64] = if missing_ref { &[1, 999] } else { &[1, 2, 3] };
            bb.add_way(10, [("highway", "residential")], refs, None);
            let ways = bb.take().expect("way block").expect("ways");
            writer
                .write_primitive_block_no_indexdata(ways)
                .expect("write ways");
            writer.flush().expect("flush");
        }
        buf.into_inner()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::test_scratch::scratch;

    #[test]
    fn reads_replication_fields_from_the_header() {
        let dir = scratch("pbf", "read");
        let url = "https://download.geofabrik.de/europe/malta-updates";
        let path = dir.join("r.osm.pbf");
        std::fs::write(&path, build::pbf(Some(4969), Some(url), false)).unwrap();
        assert_eq!(
            replication(&path).unwrap(),
            Replication {
                seq: Some(4969),
                base_url: Some(url.into())
            }
        );
        let bare = dir.join("bare.osm.pbf");
        std::fs::write(&bare, build::pbf(None, None, false)).unwrap();
        assert_eq!(replication(&bare).unwrap(), Replication::default());
    }

    #[test]
    fn refuses_what_is_not_a_pbf() {
        let dir = scratch("pbf", "refuse");
        for (name, bytes) in [
            ("html", b"<html>not found</html>".to_vec()),
            ("empty", Vec::new()),
            ("cut", build::pbf(Some(1), None, false)[..20].to_vec()),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            assert!(replication(&path).is_err(), "{name}");
        }
    }

    #[test]
    fn derives_indexed_and_locations_variants() {
        let dir = scratch("pbf", "derive");
        let raw = dir.join("raw.osm.pbf");
        std::fs::write(&raw, build::pbf(Some(7), None, false)).unwrap();
        let indexed = dir.join("indexed.osm.pbf");
        let locations = dir.join("locations.osm.pbf");
        let quiet = Diagnostics::discard();
        assert!(!pbfhogg::has_indexdata(&raw, false, &quiet).unwrap());
        index(&raw, &indexed).unwrap();
        assert!(pbfhogg::has_indexdata(&indexed, false, &quiet).unwrap());
        add_locations(&indexed, &locations, &dir.join("scratch")).unwrap();
        let header = ElementReader::from_path(&locations).unwrap();
        assert!(header.header().has_locations_on_ways());
        assert!(header.header().has_way_members_v1());
        // A second derivation onto an existing destination is refused.
        assert!(index(&raw, &indexed).is_err());
    }
}
