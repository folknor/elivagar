//! Relation replay: read back exactly the relation frames the main
//! locations pass saw, instead of re-reading the whole input.
//!
//! Relation blocks are buffered until the read ends (the tail needs the
//! finished way index, and locations-mode files can carry way blocks after
//! relation blocks). Past the buffer cap the decoded blocks are dropped. The
//! tail used to recover them by re-reading the input through
//! `BlobFilter::only_relations`, which on an input without usable index
//! metadata reads every node and way blob as well: at planet, about 85 GB
//! for 1.9 GB of decompressed relations. That stream also ran inside the
//! tail, while the tail made random reads into the freshly written way index,
//! and evicted the index from the page cache: 1.5M single-page major faults
//! and about 190 s of a 470 s phase12 on a 28 GB host, measured 2026-11-16.
//!
//! The main pass therefore records the byte range of every relation frame
//! (`input::FrameRange`), and the tail decodes those frames from
//! [`FrameRuns`]: a `Read` over the input that serves only the recorded runs,
//! back to back, as one headerless blob stream for `pbfhogg::BlobReader::new`.
//! A sorted file keeps its relations at the end, so this is normally one
//! contiguous run.

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;

use super::input::FrameRange;
use crate::pipeline::PipelineError;

/// Read size for the replay stream's buffer.
pub(super) const READ_BUFFER_BYTES: usize = 4 << 20;

/// Sort recorded relation frames into file order and merge exactly adjacent
/// ones into runs. Frames are never empty and never overlap: anything else
/// is a bookkeeping defect, refused rather than replayed.
pub(super) fn merge_runs(mut frames: Vec<FrameRange>) -> Result<Vec<FrameRange>, PipelineError> {
    frames.sort_unstable_by_key(|frame| frame.start);
    let mut runs: Vec<FrameRange> = Vec::with_capacity(frames.len().min(1024));
    for frame in frames {
        match runs.last_mut() {
            Some(last) if frame.start < last.end => {
                return Err(PipelineError::internal(
                    "relation replay: recorded frames overlap",
                ));
            }
            Some(last) if frame.start == last.end => last.end = frame.end,
            _ => runs.push(frame),
        }
    }
    Ok(runs)
}

/// The recorded runs of the input, read back to back with positional reads.
/// A run the file can no longer supply in full is an `UnexpectedEof` error,
/// never a short clean end - `BlobReader` would read that as the end of the
/// stream and the tail would silently lose relations.
pub(super) struct FrameRuns {
    file: File,
    runs: Vec<FrameRange>,
    /// Index of the run being read.
    run: usize,
    /// Next file offset to read inside `runs[run]`.
    pos: u64,
}

impl FrameRuns {
    pub(super) fn new(file: File, runs: Vec<FrameRange>) -> Self {
        let pos = runs.first().map_or(0, |run| run.start);
        Self {
            file,
            runs,
            run: 0,
            pos,
        }
    }

    /// Total bytes the runs cover.
    pub(super) fn total_bytes(&self) -> u64 {
        self.runs.iter().map(|run| run.len()).sum()
    }

    pub(super) fn run_count(&self) -> usize {
        self.runs.len()
    }
}

impl Read for FrameRuns {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(run) = self.runs.get(self.run).copied() else {
            return Ok(0);
        };
        if buf.is_empty() {
            return Ok(0);
        }
        let remaining = run.end - self.pos;
        let want = usize::try_from(remaining).map_or(buf.len(), |r| r.min(buf.len()));
        let got = loop {
            match self.file.read_at(&mut buf[..want], self.pos) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "relation replay: input ended at byte {} inside a recorded \
                             frame run ending at {}",
                            self.pos, run.end
                        ),
                    ));
                }
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        };
        self.pos += got as u64;
        if self.pos == run.end {
            self.run += 1;
            if let Some(next) = self.runs.get(self.run) {
                self.pos = next.start;
            }
        }
        Ok(got)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write;

    fn frame(start: u64, end: u64) -> FrameRange {
        FrameRange::new(start, end).unwrap()
    }

    #[test]
    fn merge_sorts_and_joins_adjacent_frames_only() {
        let runs = merge_runs(vec![
            frame(30, 40),
            frame(0, 10),
            frame(10, 20),
            frame(45, 50),
        ])
        .unwrap();
        assert_eq!(runs, vec![frame(0, 20), frame(30, 40), frame(45, 50)]);
    }

    #[test]
    fn merge_refuses_overlap() {
        assert!(merge_runs(vec![frame(0, 10), frame(5, 15)]).is_err());
    }

    #[test]
    fn empty_frames_are_unrepresentable() {
        assert!(FrameRange::new(5, 5).is_none());
        assert!(FrameRange::new(6, 5).is_none());
    }

    fn scratch_file(dir: &tempfile::TempDir, bytes: &[u8]) -> File {
        let path = dir.path().join("input.bin");
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        File::open(&path).unwrap()
    }

    #[test]
    fn runs_read_back_to_back() {
        let dir = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0..=99).collect();
        let file = scratch_file(&dir, &bytes);
        let mut runs = FrameRuns::new(file, vec![frame(10, 13), frame(50, 52), frame(98, 100)]);
        assert_eq!(runs.total_bytes(), 7);
        let mut out = Vec::new();
        runs.read_to_end(&mut out).unwrap();
        assert_eq!(out, vec![10, 11, 12, 50, 51, 98, 99]);
    }

    #[test]
    fn a_run_past_the_file_end_is_unexpected_eof() {
        let dir = tempfile::tempdir().unwrap();
        let bytes: Vec<u8> = (0..20).collect();
        let file = scratch_file(&dir, &bytes);
        let mut runs = FrameRuns::new(file, vec![frame(15, 30)]);
        let mut out = Vec::new();
        let err = runs.read_to_end(&mut out).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
