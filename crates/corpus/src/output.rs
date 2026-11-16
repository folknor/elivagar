//! Console output, matching brokkr's tagged line format: every line of a
//! message carries the tag, padded to a 10-column field, and everything goes
//! to stdout, as brokkr's renderer sends it - unless [`reserve_stdout`] was
//! called, which moves tagged lines to stderr. `plain` adds no tag and always
//! writes stdout, so a machine-readable line (`regress --json`) is the same
//! bytes either way.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set when stdout carries a machine-readable document (`regress --json`):
/// every tagged line then goes to stderr so stdout holds the document alone.
/// `plain` is unaffected. Off by default, so non-JSON output is unchanged.
static STDOUT_RESERVED: AtomicBool = AtomicBool::new(false);

/// Reserve stdout for `plain` output; tagged lines move to stderr.
pub(crate) fn reserve_stdout() {
    STDOUT_RESERVED.store(true, Ordering::Relaxed);
}

fn emit(prefix: &str, msg: &str) {
    let to_stderr = !prefix.is_empty() && STDOUT_RESERVED.load(Ordering::Relaxed);
    emit_to(prefix, msg, to_stderr);
}

fn emit_to(prefix: &str, msg: &str, to_stderr: bool) {
    let mut out = String::new();
    for line in msg.lines() {
        out.push_str(prefix);
        out.push_str(line);
        out.push('\n');
    }
    if msg.is_empty() {
        out.push_str(prefix.trim_end());
        out.push('\n');
    }
    // A closed stream (a pipe into `head`) is not worth a panic.
    if to_stderr {
        let mut stderr = std::io::stderr().lock();
        drop(stderr.write_all(out.as_bytes()));
        drop(stderr.flush());
    } else {
        let mut stdout = std::io::stdout().lock();
        drop(stdout.write_all(out.as_bytes()));
        drop(stdout.flush());
    }
}

pub(crate) fn plain(msg: &str) {
    emit("", msg);
}

pub(crate) fn run_msg(msg: &str) {
    emit("[run]     ", msg);
}

pub(crate) fn result_msg(msg: &str) {
    emit("[result]  ", msg);
}

pub(crate) fn bench_msg(msg: &str) {
    emit("[bench]   ", msg);
}

pub(crate) fn error(msg: &str) {
    if !msg.is_empty() {
        emit("[error]   ", msg);
    }
}

pub(crate) fn warn(msg: &str) {
    if !msg.is_empty() {
        emit("[warn]    ", msg);
    }
}

/// How a path is shown in a message.
pub(crate) fn path(path: &std::path::Path) -> std::path::Display<'_> {
    path.display()
}
