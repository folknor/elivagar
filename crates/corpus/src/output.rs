//! Console output, matching brokkr's tagged line format: every line of a
//! message carries the tag, padded to a 10-column field, and everything goes
//! to stdout, as brokkr's renderer sends it. `plain` adds no tag, so a
//! machine-readable line (`regress --json`) is the same bytes either way.

use std::io::Write;

fn emit(prefix: &str, msg: &str) {
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
    let mut stdout = std::io::stdout().lock();
    // A closed stdout (a pipe into `head`) is not worth a panic.
    drop(stdout.write_all(out.as_bytes()));
    drop(stdout.flush());
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
