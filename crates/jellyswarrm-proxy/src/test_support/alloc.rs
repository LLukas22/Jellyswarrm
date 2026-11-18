//! Process-wide allocation counter for memory measurements in tests.
//!
//! Counts are global, so a measurement is only meaningful when its test runs
//! alone, for example `cargo test -- --ignored --test-threads=1 <name>`.
use cap::Cap;
use std::alloc::System;

#[global_allocator]
static ALLOCATOR: Cap<System> = Cap::new(System, usize::MAX);

/// Bytes currently allocated by the process.
pub(crate) fn live_bytes() -> isize {
    ALLOCATOR.allocated() as isize
}

/// Highest live size the process has reached.
pub(crate) fn peak_bytes() -> isize {
    ALLOCATOR.max_allocated() as isize
}
