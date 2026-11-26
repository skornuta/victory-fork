//! Live bytes of the Rust heap, kept by the binary's global allocator for the resident line
//! (`victory-gpu`'s `ramline`). Here because `apps/victory` and `victory-gpu` both depend on this crate.
//!
//! (Neptune) Striped: each thread adds to a counter on a cache line of its own (64 of them,
//! a thread's chosen once), and [`sample`] sums them. The first form kept two atomics on one
//! shared line, and that slowed loading with many threads allocating; with a line per thread
//! an allocation costs one uncontended atomic add. A block freed on another thread than the
//! one that made it moves bytes between stripes; the sum is still exact.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering::Relaxed};

/// Live bytes, and the peak of live bytes, as the last [`sample`] found them.
pub static RUST_HEAP: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether the counting is on (set once by `main`: on unless `VICTORY_HEAP_COUNT=0`).
static ON: AtomicBool = AtomicBool::new(false);

const STRIPES: usize = 64;

/// One counter on a cache line (and its neighbour, which the prefetcher pairs) of its own.
#[repr(align(128))]
struct Stripe(AtomicI64);

static LIVE: [Stripe; STRIPES] = [const { Stripe(AtomicI64::new(0)) }; STRIPES];

/// The next stripe handed to a thread.
static NEXT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// This thread's stripe (`usize::MAX`: none chosen yet). Constant-initialised and without
    /// a destructor: reading it never allocates, also while a thread ends.
    static SLOT: Cell<usize> = const { Cell::new(usize::MAX) };
}

fn stripe() -> &'static AtomicI64 {
    let at = SLOT
        .try_with(|slot| {
            let mut at = slot.get();
            if at == usize::MAX {
                at = NEXT.fetch_add(1, Relaxed) % STRIPES;
                slot.set(at);
            }
            at
        })
        .unwrap_or(0);
    &LIVE[at].0
}

/// Turns the counting on or off; called by `main` before the runtime starts (the allocator
/// itself must not read the environment: that allocates).
pub fn enable(on: bool) {
    ON.store(on, Relaxed);
}

/// Whether the counting is on.
pub fn enabled() -> bool {
    ON.load(Relaxed)
}

/// `bytes` more are live.
pub fn grow(bytes: usize) {
    if !ON.load(Relaxed) {
        return;
    }
    stripe().fetch_add(bytes as i64, Relaxed);
}

/// `bytes` are released.
pub fn shrink(bytes: usize) {
    if !ON.load(Relaxed) {
        return;
    }
    stripe().fetch_sub(bytes as i64, Relaxed);
}

/// The live bytes now (the stripes summed; blocks made before the counting was turned on and
/// freed since count below zero, so the sum is floored at zero), noted in [`RUST_HEAP`] with
/// the peak of the samples so far.
pub fn sample() -> u64 {
    let live = LIVE.iter().map(|s| s.0.load(Relaxed)).sum::<i64>().max(0) as u64;
    RUST_HEAP[0].store(live, Relaxed);
    RUST_HEAP[1].fetch_max(live, Relaxed);
    live
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes made on one thread and freed on others sum to what is live; nothing is lost.
    /// (The counters are the process's: other tests' allocations are not counted, as
    /// counting is on only through `grow` and `shrink` called here.)
    #[test]
    fn stripes_sum_to_the_live_bytes() {
        enable(true);
        let before = LIVE.iter().map(|s| s.0.load(Relaxed)).sum::<i64>();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        grow(100 + i);
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        let freed: Vec<_> = (0..4)
            .map(|i| {
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        shrink(100 + 2 * i);
                        shrink(101 + 2 * i);
                    }
                })
            })
            .collect();
        freed.into_iter().for_each(|t| t.join().unwrap());
        let after = LIVE.iter().map(|s| s.0.load(Relaxed)).sum::<i64>();
        assert_eq!(after - before, 0, "made on eight threads, freed on four others");
        grow(4096);
        assert!(sample() >= 4096 || before < 0);
        shrink(4096);
    }
}
