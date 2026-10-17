//! Signal handling (SIGHUP reload, SIGUSR1 status, SIGINT/SIGTERM stop), an
//! interruptible sleep and the SIGPIPE policy.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the signal handler and polled by the main loop.
pub static STOP: AtomicBool = AtomicBool::new(false);
pub static RELOAD: AtomicBool = AtomicBool::new(false);
pub static STATUS: AtomicBool = AtomicBool::new(false);

const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGPIPE: i32 = 13;
const SIGTERM: i32 = 15;
const SIGUSR1: i32 = 10;
/// Default disposition (`SIG_DFL`).
const SIG_DFL: usize = 0;

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
    fn sleep(seconds: u32) -> u32;
}

/// Signal handler: only touches atomics, which is safe in signal context.
extern "C" fn on_signal(signum: i32) {
    match signum {
        SIGHUP => RELOAD.store(true, Ordering::SeqCst),
        SIGUSR1 => STATUS.store(true, Ordering::SeqCst),
        _ => STOP.store(true, Ordering::SeqCst), // SIGINT / SIGTERM
    }
}

pub fn install_signal_handlers() {
    let handler = on_signal as extern "C" fn(i32) as usize;
    // SAFETY: `on_signal` only uses atomic operations, which are signal-safe.
    unsafe {
        signal(SIGHUP, handler);
        signal(SIGINT, handler);
        signal(SIGTERM, handler);
        signal(SIGUSR1, handler);
        // Rust ignores SIGPIPE by default, which makes `println!` panic when
        // the reader goes away (`iowatchd --once | head`). Restore the
        // conventional behavior of dying quietly.
        signal(SIGPIPE, SIG_DFL);
    }
}

/// Sleeps for `secs` seconds, but wakes up immediately when a signal arrives
/// (`std::thread::sleep` would go back to sleep), so stop and reload take
/// effect without delay.
pub fn nap(secs: u64) {
    // SAFETY: plain libc call, no pointers involved.
    unsafe { sleep(secs.min(u32::MAX as u64) as u32) };
}
