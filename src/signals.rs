//! Signal handling (SIGHUP reload, SIGINT/SIGTERM stop) and an
//! interruptible sleep.

use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the signal handler and polled by the main loop.
pub static STOP: AtomicBool = AtomicBool::new(false);
pub static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" {
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    fn sleep(seconds: u32) -> u32;
}

/// Signal handler: only touches atomics, which is safe in signal context.
extern "C" fn on_signal(signum: i32) {
    if signum == 1 {
        RELOAD.store(true, Ordering::SeqCst); // SIGHUP
    } else {
        STOP.store(true, Ordering::SeqCst); // SIGINT / SIGTERM
    }
}

pub fn install_signal_handlers() {
    // SAFETY: `on_signal` only uses atomic operations, which are signal-safe.
    unsafe {
        signal(1, on_signal); // SIGHUP
        signal(2, on_signal); // SIGINT
        signal(15, on_signal); // SIGTERM
    }
}

/// Sleeps for `secs` seconds, but wakes up immediately when a signal arrives
/// (`std::thread::sleep` would go back to sleep), so stop and reload take
/// effect without delay.
pub fn nap(secs: u64) {
    // SAFETY: plain libc call, no pointers involved.
    unsafe { sleep(secs.min(u32::MAX as u64) as u32) };
}
