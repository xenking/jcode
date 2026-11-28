//! Per-thread CPU clock, shared by startup timing in the agent, the tool
//! registry and the server.

/// CPU time consumed by the calling thread, in milliseconds. Comparing it with
/// wall time separates real work from waiting (scheduler, disk, locks).
pub(crate) fn thread_cpu_ms() -> f64 {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: valid pointer to a timespec; CLOCK_THREAD_CPUTIME_ID is
        // supported on Linux and macOS.
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0 {
            return ts.tv_sec as f64 * 1000.0 + ts.tv_nsec as f64 / 1_000_000.0;
        }
    }
    0.0
}
