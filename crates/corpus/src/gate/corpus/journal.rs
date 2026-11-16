//! Makes `bless`'s baseline write one atomic set (digest, contract.json and
//! leaves), without changing any of the three file formats.
//!
//! A bless used to replace the three files one after another, so an
//! interruption between two renames left a digest from one archive beside
//! leaves or a contract from another. The scheme is a roll-forward journal:
//!
//! 1. Every new file is written to a fsynced sibling temp (`atomic_write::
//!    stage`). Nothing committed has changed yet; an interruption here leaves
//!    only stray dot-temps, and the old baseline intact.
//! 2. The journal `<corpus>/.bless-journal` is written atomically, naming each
//!    temp -> target rename and each file to remove (`leaves` when blessing in
//!    buckets mode). Its appearance is the commit point: from here the new set
//!    is decided.
//! 3. The journal is replayed: each temp still present is renamed over its
//!    target, each removal done, the directories synced, and the journal
//!    deleted last.
//!
//! Replay is idempotent (a rename whose temp is gone already happened), so a
//! crash anywhere in step 3 is finished by replaying again. `bless` replays a
//! leftover journal before it does anything else; `check` and
//! `render-manifest` refuse a corpus carrying one as baseline trouble (exit 3)
//! rather than judge a half-written set or write into one.

use std::io;
use std::path::{Path, PathBuf};

use crate::atomic_write::{self, Staged};

pub(crate) const JOURNAL_FILE: &str = ".bless-journal";
const HEADER: &str = "elivagar-corpus bless journal v1";

pub(crate) fn path(corpus_dir: &Path) -> PathBuf {
    corpus_dir.join(JOURNAL_FILE)
}

/// The refusal text for a reader that meets a leftover journal, or `None`.
pub(crate) fn pending(corpus_dir: &Path) -> Option<String> {
    let p = path(corpus_dir);
    p.exists().then(|| {
        format!(
            "interrupted bless: {} is present, so the baseline set may be half \
             replaced; run bless again (any mode) to complete it",
            p.display()
        )
    })
}

fn field(p: &Path) -> io::Result<&str> {
    p.to_str()
        .filter(|s| !s.contains(['\t', '\n']))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cannot journal path {}", p.display()),
            )
        })
}

/// Commit `staged` and `remove` as one set through the journal.
pub(crate) fn commit(corpus_dir: &Path, staged: Vec<Staged>, remove: &[PathBuf]) -> io::Result<()> {
    let mut lines = vec![HEADER.to_owned()];
    for s in &staged {
        lines.push(format!("rename\t{}\t{}", field(&s.tmp)?, field(&s.target)?));
    }
    for r in remove {
        lines.push(format!("remove\t{}", field(r)?));
    }
    let mut text = lines.join("\n");
    text.push('\n');
    // Step 2, the commit point. On failure the Staged temps drop and vanish.
    atomic_write::replace(&path(corpus_dir), text.as_bytes())?;
    for s in staged {
        drop(s.keep());
    }
    replay(corpus_dir)
}

/// Finish a journaled set (step 3). A no-op when no journal exists.
pub(crate) fn replay(corpus_dir: &Path) -> io::Result<()> {
    let jpath = path(corpus_dir);
    let text = match std::fs::read_to_string(&jpath) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let bad = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unreadable bless journal {}", jpath.display()),
        )
    };
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Err(bad());
    }
    let mut ops = Vec::new();
    for line in lines {
        let parts: Vec<&str> = line.split('\t').collect();
        match parts.as_slice() {
            ["rename", tmp, target] => ops.push((Some(PathBuf::from(tmp)), PathBuf::from(target))),
            ["remove", target] => ops.push((None, PathBuf::from(target))),
            _ => return Err(bad()),
        }
    }
    for (tmp, target) in &ops {
        match tmp {
            Some(tmp) => match std::fs::rename(tmp, target) {
                Err(e) if e.kind() == io::ErrorKind::NotFound && !tmp.exists() => {}
                other => other?,
            },
            None => match std::fs::remove_file(target) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                other => other?,
            },
        }
        atomic_write::sync_parent(target);
    }
    std::fs::remove_file(&jpath)?;
    atomic_write::sync_parent(&jpath);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{commit, pending, replay};
    use crate::atomic_write::stage;
    use crate::gate::corpus::fixture::TestDir;

    #[test]
    fn commit_replaces_the_set_and_clears_the_journal() {
        let dir = TestDir::new("journal-commit");
        let a = dir.path.join("a");
        let b = dir.path.join("b");
        std::fs::write(&a, "old a").unwrap();
        std::fs::write(&b, "old b").unwrap();
        let staged = vec![stage(&a, b"new a").unwrap()];
        commit(&dir.path, staged, std::slice::from_ref(&b)).unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "new a");
        assert!(!b.exists());
        assert!(pending(&dir.path).is_none());
    }

    #[test]
    fn replay_finishes_an_interrupted_set() {
        let dir = TestDir::new("journal-replay");
        let a = dir.path.join("a");
        let b = dir.path.join("b");
        std::fs::write(&a, "old a").unwrap();
        std::fs::write(&b, "old b").unwrap();
        // Simulate a crash after a's rename but before b's.
        let (ta, _) = stage(&a, b"new a").unwrap().keep();
        let (tb, _) = stage(&b, b"new b").unwrap().keep();
        let a_target = std::fs::canonicalize(&a).unwrap();
        let b_target = std::fs::canonicalize(&b).unwrap();
        std::fs::write(
            super::path(&dir.path),
            format!(
                "{}\nrename\t{}\t{}\nrename\t{}\t{}\n",
                super::HEADER,
                ta.display(),
                a_target.display(),
                tb.display(),
                b_target.display()
            ),
        )
        .unwrap();
        std::fs::rename(&ta, &a_target).unwrap();
        assert!(pending(&dir.path).is_some());
        replay(&dir.path).unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "new a");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "new b");
        assert!(pending(&dir.path).is_none());
        // Idempotent once done.
        replay(&dir.path).unwrap();
    }
}
