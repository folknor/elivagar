#![allow(clippy::unwrap_used)]

//! SIGTERM and SIGINT end a run with `128 + signo`, from the signal thread
//! that also writes a profiled build's hotpath report (crates/cli/src/
//! signals.rs). brokkr's `stop_at` sends exactly this SIGTERM.
//!
//! The run is held mid-flight deterministically: its input is a FIFO that
//! nothing ever writes, so opening it blocks forever and the run can only end
//! through the signal. The signal is sent once the signal thread exists,
//! which `main` starts before anything else, so a pass cannot be a race won
//! by the default disposition (that would end the child by the signal itself,
//! with no exit code).

use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Far beyond what the child needs to reach the signal thread or to exit.
const BOUND: Duration = Duration::from_secs(30);

fn make_fifo(path: &Path) {
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("fifo path");
    // SAFETY: mkfifo reads the nul-terminated path and does not retain it.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    assert_eq!(
        rc,
        0,
        "mkfifo {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

/// Whether the child has started its signal thread. The kernel truncates a
/// thread name to 15 bytes, so `elivagar-signals` reads `elivagar-signal`.
fn signal_thread_started(pid: u32) -> bool {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return false;
    };
    tasks.flatten().any(|task| {
        std::fs::read_to_string(task.path().join("comm"))
            .is_ok_and(|comm| comm.trim_end() == "elivagar-signal")
    })
}

fn kill_and_fail(child: &mut Child, why: &str) -> ! {
    drop(child.kill());
    drop(child.wait());
    panic!("{why}");
}

fn signalled_run_exits_with(signo: libc::c_int) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let input = dir.path().join("never-written.osm.pbf");
    make_fifo(&input);
    let output = dir.path().join("out.pmtiles");

    let mut child = Command::new(env!("CARGO_BIN_EXE_elivagar"))
        .arg("run")
        .arg(&input)
        .arg("--output")
        .arg(&output)
        .arg("--tmp-dir")
        .arg(dir.path().join("tmp"))
        .args(["--threads", "1"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start elivagar");

    let start = Instant::now();
    while !signal_thread_started(child.id()) {
        if let Some(status) = child.try_wait().expect("poll elivagar") {
            panic!("elivagar ended before the signal was sent: {status}");
        }
        if start.elapsed() > BOUND {
            kill_and_fail(&mut child, "the signal thread never started");
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let pid = libc::pid_t::try_from(child.id()).expect("pid fits pid_t");
    // SAFETY: kill takes a pid and a signal number and retains nothing.
    assert_eq!(unsafe { libc::kill(pid, signo) }, 0, "kill");

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll elivagar") {
            break status;
        }
        if start.elapsed() > BOUND {
            kill_and_fail(&mut child, "elivagar did not exit after the signal");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().expect("stderr"), &mut stderr)
        .expect("read stderr");

    assert_eq!(
        status.signal(),
        None,
        "the signal killed elivagar instead of being taken: {stderr}"
    );
    assert_eq!(status.code(), Some(128 + signo), "{stderr}");
    assert!(
        stderr.contains("received; stopping"),
        "no stopping notice: {stderr}"
    );
}

#[test]
fn sigterm_mid_run_exits_143() {
    signalled_run_exits_with(libc::SIGTERM);
}

#[test]
fn sigint_mid_run_exits_130() {
    signalled_run_exits_with(libc::SIGINT);
}
