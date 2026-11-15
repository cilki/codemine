//! The one way the crate takes a mutex.

use std::sync::{Mutex, MutexGuard};

/// Lock `mutex`, shrugging off poisoning. Everything behind a mutex here is
/// a plain value — a map, a queue, a cached answer — that a holder panicking
/// partway can't leave in a state the next holder would mind, so the panic
/// stays on the thread it happened on instead of wedging every other thread
/// that reads the same state: a web UI handler's panic must not stop the
/// main loop, nor the other way round.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
