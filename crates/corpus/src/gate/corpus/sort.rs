//! Bounded-memory external sorting for corpus multiset hashes.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use xxhash_rust::xxh3::Xxh3;

const SORT_CHUNK_VALUES: usize = 1_000_000;
const MERGE_FAN_IN: usize = 64;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) struct ExternalU128Sorter {
    values: Vec<u128>,
    runs: Vec<PathBuf>,
    scratch_root: PathBuf,
    temp_dir: Option<TempDir>,
    next_run: usize,
}

impl ExternalU128Sorter {
    /// `scratch_root` is the project-state directory spill runs live under
    /// (`ProjectState::corpus_sort_dir`); nothing is created there until the
    /// first chunk spills.
    pub(crate) fn new(scratch_root: &Path) -> Self {
        Self {
            values: Vec::new(),
            runs: Vec::new(),
            scratch_root: scratch_root.to_path_buf(),
            temp_dir: None,
            next_run: 0,
        }
    }

    pub(crate) fn push(&mut self, value: u128) -> io::Result<()> {
        self.values.push(value);
        if self.values.len() == SORT_CHUNK_VALUES {
            self.flush_chunk()?;
        }
        Ok(())
    }

    /// Hash the sorted values using the same byte stream as an in-memory sort.
    /// Resident memory is bounded by one chunk plus the bounded merge fan-in.
    pub(crate) fn finish_hash(&mut self, domain: &[u8], count: u64) -> io::Result<u128> {
        let mut hash = Xxh3::new();
        hash.update(domain);
        hash.update(&count.to_le_bytes());
        if self.runs.is_empty() {
            self.values.sort_unstable();
            for value in self.values.drain(..) {
                hash.update(&value.to_le_bytes());
            }
            return Ok(hash.digest128());
        }

        self.flush_chunk()?;
        self.compact_runs()?;
        merge_runs(&self.runs, |value| {
            hash.update(&value.to_le_bytes());
            Ok(())
        })?;
        Ok(hash.digest128())
    }

    /// Count distinct values with the same bounded-memory sort used for the
    /// corpus folds. The input count is the number of leaf runs, not addressed
    /// tiles, so a zero-length leaf still participates as before.
    pub(crate) fn finish_unique_count(&mut self) -> io::Result<u64> {
        let mut previous = None;
        let mut unique = 0u64;
        let mut count = |value| {
            if previous != Some(value) {
                unique = unique
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("unique hash count overflows"))?;
                previous = Some(value);
            }
            Ok(())
        };

        if self.runs.is_empty() {
            self.values.sort_unstable();
            for value in self.values.drain(..) {
                count(value)?;
            }
        } else {
            self.flush_chunk()?;
            self.compact_runs()?;
            merge_runs(&self.runs, &mut count)?;
        }
        Ok(unique)
    }

    fn flush_chunk(&mut self) -> io::Result<()> {
        if self.values.is_empty() {
            return Ok(());
        }
        self.values.sort_unstable();
        let path = self.next_run_path()?;
        let file = File::create(&path)?;
        let mut writer = BufWriter::new(file);
        for value in self.values.drain(..) {
            writer.write_all(&value.to_le_bytes())?;
        }
        writer.flush()?;
        self.runs.push(path);
        // Keep run metadata bounded too; waiting until finish_hash() would
        // leave one PathBuf per chunk resident for the whole input walk.
        self.compact_runs()
    }

    fn next_run_path(&mut self) -> io::Result<PathBuf> {
        if self.temp_dir.is_none() {
            self.temp_dir = Some(TempDir::create(&self.scratch_root)?);
        }
        let temp_dir = self
            .temp_dir
            .as_ref()
            .ok_or_else(|| io::Error::other("external sort temporary directory is absent"))?;
        let path = temp_dir.path.join(format!("run-{}", self.next_run));
        self.next_run += 1;
        Ok(path)
    }

    fn compact_runs(&mut self) -> io::Result<()> {
        while self.runs.len() > MERGE_FAN_IN {
            let old_runs = std::mem::take(&mut self.runs);
            let mut compacted = Vec::with_capacity(old_runs.len().div_ceil(MERGE_FAN_IN));
            for batch in old_runs.chunks(MERGE_FAN_IN) {
                if batch.len() == 1 {
                    compacted.push(batch[0].clone());
                    continue;
                }
                let destination = self.next_run_path()?;
                merge_to_file(batch, &destination)?;
                for path in batch {
                    fs::remove_file(path)?;
                }
                compacted.push(destination);
            }
            self.runs = compacted;
        }
        Ok(())
    }
}

struct TempDir {
    path: PathBuf,
}

// The spill directory keeps brokkr's `brokkr-corpus-sort-PID-ID` name and is
// made under `.brokkr/corpus-sort`, so the leftovers of a killed sort are the
// ones `brokkr clean` already recognises and removes.
impl TempDir {
    fn create(base: &Path) -> io::Result<Self> {
        fs::create_dir_all(base)?;
        let mut last_error = None;
        for _ in 0..100 {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("brokkr-corpus-sort-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            io::Error::other("could not allocate a unique corpus sort directory")
        }))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.path));
    }
}

fn merge_to_file(paths: &[PathBuf], destination: &Path) -> io::Result<()> {
    let file = File::create(destination)?;
    let mut writer = BufWriter::new(file);
    merge_runs(paths, |value| writer.write_all(&value.to_le_bytes()))?;
    writer.flush()
}

fn merge_runs(paths: &[PathBuf], mut emit: impl FnMut(u128) -> io::Result<()>) -> io::Result<()> {
    let mut readers = paths
        .iter()
        .map(File::open)
        .map(|result| result.map(BufReader::new))
        .collect::<io::Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::with_capacity(readers.len());
    for (index, reader) in readers.iter_mut().enumerate() {
        if let Some(value) = read_next(reader)? {
            heap.push(Reverse((value, index)));
        }
    }
    while let Some(Reverse((value, index))) = heap.pop() {
        emit(value)?;
        if let Some(value) = read_next(&mut readers[index])? {
            heap.push(Reverse((value, index)));
        }
    }
    Ok(())
}

fn read_next(reader: &mut BufReader<File>) -> io::Result<Option<u128>> {
    let mut bytes = [0; 16];
    if reader.read(&mut bytes[..1])? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut bytes[1..])?;
    Ok(Some(u128::from_le_bytes(bytes)))
}
