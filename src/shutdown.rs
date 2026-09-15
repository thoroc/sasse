//! Asking a long-running worker to stop.
//!
//! A worker interrupted mid-candidate should finish the tick it is on and then
//! exit, rather than abandoning a gate halfway and leaving the lease to expire.
//! So a signal only sets a flag, which the loop checks between ticks.

use std::sync::atomic::{AtomicBool, Ordering};

use eyre::{Result, eyre};

static STOP: AtomicBool = AtomicBool::new(false);

/// Whether a stop has been asked for.
pub fn requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

#[cfg(unix)]
extern "C" fn note_request(_signal: libc::c_int) {
    // A store to an atomic is async-signal-safe. Nothing else belongs in here:
    // allocating, locking or printing from a signal handler is not.
    STOP.store(true, Ordering::SeqCst);
}

/// Arrange for an interrupt or a termination request to stop the loop cleanly.
#[cfg(unix)]
pub fn install() -> Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let handler = note_request as *const () as libc::sighandler_t;
        // SAFETY: the handler does nothing but store to an atomic.
        let previous = unsafe { libc::signal(signal, handler) };
        if previous == libc::SIG_ERR {
            return Err(eyre!(
                "could not install a handler for signal {signal}; \
                 refusing to run a loop that cannot be stopped cleanly"
            ));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn install() -> Result<()> {
    Ok(())
}
