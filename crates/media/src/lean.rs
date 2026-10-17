//! Lean caches while an export runs where memory is tight (the web's wasm heap).
//!
//! The decode and frame caches are sized for playback and scrubbing: hundreds of MB to keep a
//! clip's GOP and the frames around the playhead. A one-pass export reads every frame once, in
//! order, so those caches only grow the heap; on the web a 4K export ran out of its 2 GiB with
//! them. Whoever runs such an export holds a [`LeanGuard`] for as long as it runs, and the caches
//! check [`active`] to keep only what the export needs. The guard is released when it is dropped,
//! so the caches go back to their playback sizes however the export ends (done, failed,
//! cancelled, its session dropped).

use std::sync::atomic::{AtomicUsize, Ordering};

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Caches stay lean while at least one guard is alive.
#[derive(Debug)]
#[must_use = "the caches are lean only while the guard is alive"]
pub struct LeanGuard(());

/// Make the caches lean until the guard is dropped (guards nest).
pub fn enter() -> LeanGuard {
    ACTIVE.fetch_add(1, Ordering::Relaxed);
    LeanGuard(())
}

impl Drop for LeanGuard {
    fn drop(&mut self) {
        // never below zero, whatever the order guards go in
        let _ = ACTIVE.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }
}

/// Whether caches should be lean now.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed) > 0
}

/// `normal`, or at most `lean` while caches are lean.
pub fn cap(normal: usize, lean: usize) -> usize {
    if active() { normal.min(lean) } else { normal }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_nest_and_release_on_drop() {
        // (other tests in this binary do not take guards)
        assert!(!active());
        assert_eq!(cap(100, 10), 100);
        let a = enter();
        let b = enter();
        assert!(active());
        assert_eq!(cap(100, 10), 10);
        assert_eq!(cap(5, 10), 5);
        drop(a);
        assert!(active());
        drop(b);
        assert!(!active());
        assert_eq!(cap(100, 10), 100);
    }
}
