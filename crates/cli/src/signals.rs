//! SIGINT and SIGTERM: how the binary ends when asked to stop.
//!
//! A run cannot be cancelled part way, so a signal ends the process. What
//! the signal changes is how it ends:
//!
//! - **The first SIGINT or SIGTERM** finishes the hotpath report, if a
//!   profiled build holds one (`hold_profile`), then exits `128 + signo` -
//!   `130` for SIGINT, `143` for SIGTERM. The exit is never 0: the run did not
//!   finish, so an output archive it leaves is incomplete and must not read as
//!   a success. Nothing else unwinds; scratch under `--tmp-dir` stays where it
//!   is, as it would after a kill, and the next full run clears it.
//! - **A second signal** while the report is being written exits at once
//!   with `128 + signo` of that signal and no report.
//!
//! The report is why this exists. brokkr's script benches stop a run at a
//! phase marker with one SIGTERM (`stop_at`), and hotpath writes its report
//! only when its guard drops; under the default disposition the SIGTERM
//! killed the process before that, so a profiled run stopped at a marker
//! captured nothing. In an unprofiled build the guard is hotpath's zero-sized
//! stand-in and the only difference from the default disposition is the
//! conventional exit status in place of death by signal.
//!
//! Mechanism, as in pbfhogg's CLI: both signals are blocked in the main
//! thread before any other thread exists, so every thread (rayon's pool,
//! the pipeline's workers) inherits the mask and no signal handler ever runs;
//! one dedicated thread takes them with `sigwait`, so the report is written
//! from ordinary thread context, not from a handler. elivagar starts no child
//! processes, so the blocked mask (which `exec` would hand down) reaches no
//! other program.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, PoisonError};

/// The run's hotpath guard, while it is live. The signal waiter takes it
/// from here to write the report; a run that finishes drops it here itself.
/// Its drop runs under the lock either way, so a signal arriving while a
/// finished run writes its report waits for that report rather than racing
/// it.
static PROFILE: Mutex<Option<hotpath::HotpathGuard>> = Mutex::new(None);

/// The first signal taken, or 0.
static FIRST_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// Block SIGINT and SIGTERM in the calling thread and start the waiter. Call
/// first thing in `main`, before any other thread is spawned: a thread
/// started earlier keeps the default disposition and the signal would kill
/// the process from it instead.
///
/// A failure leaves the default disposition in place, which costs only the
/// report on a signal, so it is reported and the run goes on.
pub(crate) fn install() {
    let set = signal_set();
    // SAFETY: `set` is an initialised signal set; a null old-set pointer is
    // allowed.
    let rc =
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) };
    if rc != 0 {
        eprintln!(
            "warning: could not block SIGINT and SIGTERM ({}); a signal will end the run \
             without a hotpath report",
            std::io::Error::from_raw_os_error(rc)
        );
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("elivagar-signals".to_owned())
        .spawn(move || wait_loop(&set));
    if let Err(e) = spawned {
        // Nobody would take the blocked signals: restore them so Ctrl-C
        // still ends the process.
        // SAFETY: as above.
        unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, &raw const set, std::ptr::null_mut()) };
        eprintln!(
            "warning: could not start the signal thread ({e}); a signal will end the run \
             without a hotpath report"
        );
    }
}

/// Hand the run's hotpath guard to the signal policy for the rest of the run.
pub(crate) fn hold_profile(guard: hotpath::HotpathGuard) {
    *PROFILE.lock().unwrap_or_else(PoisonError::into_inner) = Some(guard);
}

/// The run finished: write the report now, on the calling thread.
pub(crate) fn finish_profile() {
    let mut slot = PROFILE.lock().unwrap_or_else(PoisonError::into_inner);
    // Dropped in place while the lock is held; see `PROFILE`.
    *slot = None;
}

fn signal_set() -> libc::sigset_t {
    // SAFETY: `sigemptyset` initialises the zeroed set; adding two valid
    // signal numbers cannot fail.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        libc::sigaddset(&raw mut set, libc::SIGINT);
        libc::sigaddset(&raw mut set, libc::SIGTERM);
        set
    }
}

fn wait_loop(set: &libc::sigset_t) {
    loop {
        let mut signo: libc::c_int = 0;
        // SAFETY: `set` is initialised and `signo` is a valid out pointer.
        if unsafe { libc::sigwait(set, &raw mut signo) } != 0 {
            continue;
        }
        if FIRST_SIGNAL
            .compare_exchange(0, signo, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // The report is written on a thread of its own, so this one is
            // free to take a second signal while that write is under way.
            let spawned = std::thread::Builder::new()
                .name("elivagar-signal-exit".to_owned())
                .spawn(move || stop(signo));
            if spawned.is_err() {
                // No thread to write the report from: exit without it.
                note(signo, "exiting without a hotpath report");
                exit_now(signo);
            }
        } else {
            exit_now(signo);
        }
    }
}

/// The first signal's exit: finish the report, then end the process.
fn stop(signo: libc::c_int) -> ! {
    note(
        signo,
        "stopping; the run did not finish and its output is incomplete",
    );
    finish_profile();
    exit_now(signo);
}

/// One line on stderr, written with a single `write` so it takes no lock a
/// running thread may hold. A failed or short write costs only the line.
fn note(signo: libc::c_int, what: &str) {
    let line = format!("elivagar: {} received; {what}\n", name(signo));
    // SAFETY: the buffer is valid for its length for the whole call.
    let _written = unsafe { libc::write(libc::STDERR_FILENO, line.as_ptr().cast(), line.len()) };
}

/// End the process with `128 + signo`. `_exit` runs no destructor and no
/// atexit handler, so it is safe while the pipeline's threads still run, and
/// the report, if any, is already written by the time it is called.
fn exit_now(signo: libc::c_int) -> ! {
    // SAFETY: `_exit` is async-signal-safe and callable from any thread.
    unsafe { libc::_exit(128 + signo) }
}

fn name(signo: libc::c_int) -> &'static str {
    match signo {
        libc::SIGINT => "SIGINT",
        libc::SIGTERM => "SIGTERM",
        _ => "a signal",
    }
}
