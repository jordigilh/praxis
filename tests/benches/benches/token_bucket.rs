// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2024 Praxis Contributors

//! Criterion benchmarks for token-bucket state-management alternatives.

#![expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::expect_used,
    clippy::min_ident_chars,
    clippy::too_many_lines,
    clippy::unwrap_used,
    reason = "benchmarks"
)]

// Include the private implementation directly so the benchmark does not make
// token-bucket internals part of the filter crate's public API.
#[expect(
    dead_code,
    reason = "the included implementation also contains non-benchmarked introspection and test helpers"
)]
#[path = "../../../crates/filter/src/builtins/http/traffic_management/token_bucket.rs"]
mod production_token_bucket;

use std::{
    hint::black_box,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    sync::{Arc, Barrier},
    thread,
    time::Instant,
};

use criterion::{BenchmarkGroup, BenchmarkId, Criterion, criterion_group, criterion_main, measurement::WallTime};

// ----------------------------------------------------------------------------
// Benchmark Configuration
// ----------------------------------------------------------------------------

/// Fixed rate used by the comparable candidates.
const RATE: f64 = 10_000.0;

/// Fixed burst used to keep success benchmarks away from depletion.
const BURST: f64 = 1_000_000_000.0;

/// Exact interval for [`RATE`] in nanoseconds.
const INTERVAL_NANOS: u64 = 100_000;

/// Number of tokens in [`BURST`] as an integer.
const BURST_TOKENS: u64 = 1_000_000_000;

// ----------------------------------------------------------------------------
// Benchmark Candidates
// ----------------------------------------------------------------------------

/// Common operation surface for the benchmark candidates.
trait Candidate: Send + Sync + 'static {
    /// Construct a full bucket for successful-acquisition benchmarks.
    fn full() -> Self
    where
        Self: Sized;

    /// Construct a bucket with no available tokens for rejection benchmarks.
    fn empty() -> Self
    where
        Self: Sized;

    /// Construct a one-token bucket for contention benchmarks.
    fn single() -> Self
    where
        Self: Sized;

    /// Attempt one acquisition at the supplied monotonic timestamp.
    fn acquire(&self, now_nanos: u64) -> Option<f64>;
}

/// Call a candidate through an opaque boundary so benchmark-local state is
/// not constant-folded away by the optimizer.
#[inline(never)]
fn acquire_candidate(candidate: &dyn Candidate, now_nanos: u64) -> Option<f64> {
    candidate.acquire(now_nanos)
}

/// The mutex-protected compound state from the PR under test.
struct MutexCandidate {
    bucket: production_token_bucket::TokenBucket,
    rate: f64,
    burst: f64,
}

impl Candidate for MutexCandidate {
    fn full() -> Self {
        Self::with_burst(BURST)
    }

    fn empty() -> Self {
        let candidate = Self::single();
        let _ = candidate.acquire(0);
        candidate
    }

    fn single() -> Self {
        Self::with_burst(1.0)
    }

    fn acquire(&self, now_nanos: u64) -> Option<f64> {
        self.bucket.try_acquire(self.rate, self.burst, now_nanos)
    }
}

impl MutexCandidate {
    /// Construct a mutex candidate with a selected initial burst.
    fn with_burst(burst: f64) -> Self {
        Self {
            bucket: production_token_bucket::TokenBucket::new(burst),
            rate: RATE,
            burst,
        }
    }
}

/// The pre-PR split-atomic implementation, retained as a performance baseline.
///
/// This is not a correctness candidate: the separate token and timestamp
/// atomics permit the race fixed by the PR.
struct SplitAtomicsCandidate {
    tokens: AtomicU64,
    last_refill: AtomicU64,
    rate: f64,
    burst: f64,
}

impl Candidate for SplitAtomicsCandidate {
    fn full() -> Self {
        Self::with_burst(BURST)
    }

    fn empty() -> Self {
        let candidate = Self::single();
        let _ = candidate.acquire(0);
        candidate
    }

    fn single() -> Self {
        Self::with_burst(1.0)
    }

    fn acquire(&self, now_nanos: u64) -> Option<f64> {
        loop {
            let old_tokens_bits = self.tokens.load(Ordering::Acquire);
            let old_refill = self.last_refill.load(Ordering::Acquire);
            let mut tokens = f64::from_bits(old_tokens_bits);

            let elapsed_nanos = now_nanos.saturating_sub(old_refill);
            if elapsed_nanos > 0 {
                tokens = (tokens + nanos_to_secs(elapsed_nanos) * self.rate).min(self.burst);
            }

            if tokens < 1.0 {
                return None;
            }

            let new_tokens = tokens - 1.0;
            if self
                .tokens
                .compare_exchange_weak(
                    old_tokens_bits,
                    new_tokens.to_bits(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                self.last_refill.fetch_max(now_nanos, Ordering::Release);
                return Some(new_tokens);
            }
        }
    }
}

impl SplitAtomicsCandidate {
    /// Construct the split-atomic baseline with a selected initial burst.
    fn with_burst(burst: f64) -> Self {
        Self {
            tokens: AtomicU64::new(burst.to_bits()),
            last_refill: AtomicU64::new(0),
            rate: RATE,
            burst,
        }
    }
}

/// A correctness-preserving split-field implementation guarded by an atomic
/// spin lock. This measures the cost of making the two fields one logical
/// transition without requiring a 128-bit atomic.
struct LockedSplitAtomicsCandidate {
    tokens: AtomicU64,
    last_refill: AtomicU64,
    lock: AtomicBool,
    rate: f64,
    burst: f64,
}

impl Candidate for LockedSplitAtomicsCandidate {
    fn full() -> Self {
        Self::with_burst(BURST)
    }

    fn empty() -> Self {
        let candidate = Self::single();
        let _ = candidate.acquire(0);
        candidate
    }

    fn single() -> Self {
        Self::with_burst(1.0)
    }

    fn acquire(&self, now_nanos: u64) -> Option<f64> {
        self.lock();

        let tokens = f64::from_bits(self.tokens.load(Ordering::Relaxed));
        let last_refill = self.last_refill.load(Ordering::Relaxed);
        let elapsed_nanos = now_nanos.saturating_sub(last_refill);
        let tokens = if elapsed_nanos > 0 {
            (tokens + nanos_to_secs(elapsed_nanos) * self.rate).min(self.burst)
        } else {
            tokens
        };

        let result = if tokens < 1.0 {
            None
        } else {
            let remaining = tokens - 1.0;
            self.tokens.store(remaining.to_bits(), Ordering::Relaxed);
            self.last_refill.store(last_refill.max(now_nanos), Ordering::Relaxed);
            Some(remaining)
        };

        self.unlock();
        result
    }
}

impl LockedSplitAtomicsCandidate {
    /// Construct a split-field candidate with serialized compound updates.
    fn with_burst(burst: f64) -> Self {
        Self {
            tokens: AtomicU64::new(burst.to_bits()),
            last_refill: AtomicU64::new(0),
            lock: AtomicBool::new(false),
            rate: RATE,
            burst,
        }
    }

    fn lock(&self) {
        while self
            .lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }
    }

    fn unlock(&self) {
        self.lock.store(false, Ordering::Release);
    }
}

/// A single-atomic virtual-time/GCRA candidate.
///
/// This represents the family of alternatives used by `governor` and Envoy.
/// It intentionally does not expose the current implementation's exact
/// fractional token count or last-refill timestamp.
struct VirtualTimeCandidate {
    theoretical_arrival: AtomicU64,
    tolerance_nanos: u64,
    interval_nanos: u64,
    burst: f64,
}

impl Candidate for VirtualTimeCandidate {
    fn full() -> Self {
        Self::with_burst(BURST_TOKENS)
    }

    fn empty() -> Self {
        let candidate = Self::single();
        let _ = candidate.acquire(0);
        candidate
    }

    fn single() -> Self {
        Self::with_burst(1)
    }

    fn acquire(&self, now_nanos: u64) -> Option<f64> {
        loop {
            let old_arrival = self.theoretical_arrival.load(Ordering::Acquire);
            let threshold = old_arrival.saturating_sub(self.tolerance_nanos);
            if now_nanos < threshold {
                return None;
            }

            let next_arrival = old_arrival.max(now_nanos).saturating_add(self.interval_nanos);
            if self
                .theoretical_arrival
                .compare_exchange_weak(old_arrival, next_arrival, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let debt_nanos = next_arrival.saturating_sub(now_nanos);
                let remaining = self.burst - (debt_nanos as f64 / self.interval_nanos as f64);
                return Some(remaining.max(0.0));
            }
        }
    }
}

impl VirtualTimeCandidate {
    /// Construct a virtual-time candidate with a selected burst.
    fn with_burst(burst_tokens: u64) -> Self {
        Self {
            theoretical_arrival: AtomicU64::new(0),
            tolerance_nanos: burst_tokens.saturating_sub(1).saturating_mul(INTERVAL_NANOS),
            interval_nanos: INTERVAL_NANOS,
            burst: burst_tokens as f64,
        }
    }
}

/// Candidates that retain the existing read-only introspection operation.
trait InspectableCandidate: Candidate {
    /// Return the current token count at the supplied timestamp.
    fn current(&self, now_nanos: u64) -> f64;
}

/// Call the introspection operation through the same opaque boundary.
#[inline(never)]
fn current_candidate(candidate: &dyn InspectableCandidate, now_nanos: u64) -> f64 {
    candidate.current(now_nanos)
}

impl InspectableCandidate for MutexCandidate {
    fn current(&self, now_nanos: u64) -> f64 {
        self.bucket.current_tokens(self.rate, self.burst, now_nanos)
    }
}

impl InspectableCandidate for SplitAtomicsCandidate {
    fn current(&self, now_nanos: u64) -> f64 {
        let tokens = f64::from_bits(self.tokens.load(Ordering::Acquire));
        let last_refill = self.last_refill.load(Ordering::Acquire);
        (tokens + nanos_to_secs(now_nanos.saturating_sub(last_refill)) * self.rate).min(self.burst)
    }
}

impl InspectableCandidate for LockedSplitAtomicsCandidate {
    fn current(&self, now_nanos: u64) -> f64 {
        self.lock();
        let tokens = f64::from_bits(self.tokens.load(Ordering::Relaxed));
        let last_refill = self.last_refill.load(Ordering::Relaxed);
        let current = (tokens + nanos_to_secs(now_nanos.saturating_sub(last_refill)) * self.rate).min(self.burst);
        self.unlock();
        current
    }
}

// ----------------------------------------------------------------------------
// Benchmarks
// ----------------------------------------------------------------------------

criterion_group!(benches, bench_token_bucket);
criterion_main!(benches);

/// Run sequential, contended, rejected, and introspection workloads.
fn bench_token_bucket(c: &mut Criterion) {
    let mut success = c.benchmark_group("token_bucket/acquire_success");
    bench_sequential::<MutexCandidate>(&mut success, "mutex", 0);
    bench_sequential::<SplitAtomicsCandidate>(&mut success, "split_atomics", 0);
    bench_sequential::<LockedSplitAtomicsCandidate>(&mut success, "split_atomics_locked", 0);
    bench_sequential::<VirtualTimeCandidate>(&mut success, "virtual_time_gcra", 0);
    bench_sequential::<MutexCandidate>(&mut success, "mutex_refill", INTERVAL_NANOS);
    bench_sequential::<SplitAtomicsCandidate>(&mut success, "split_atomics_refill", INTERVAL_NANOS);
    bench_sequential::<LockedSplitAtomicsCandidate>(&mut success, "split_atomics_locked_refill", INTERVAL_NANOS);
    bench_sequential::<VirtualTimeCandidate>(&mut success, "virtual_time_gcra_refill", INTERVAL_NANOS);
    success.finish();

    let mut rejection = c.benchmark_group("token_bucket/acquire_rejection");
    bench_rejection::<MutexCandidate>(&mut rejection, "mutex");
    bench_rejection::<SplitAtomicsCandidate>(&mut rejection, "split_atomics");
    bench_rejection::<LockedSplitAtomicsCandidate>(&mut rejection, "split_atomics_locked");
    bench_rejection::<VirtualTimeCandidate>(&mut rejection, "virtual_time_gcra");
    rejection.finish();

    let mut introspection = c.benchmark_group("token_bucket/introspection");
    bench_introspection::<MutexCandidate>(&mut introspection, "mutex");
    bench_introspection::<SplitAtomicsCandidate>(&mut introspection, "split_atomics");
    bench_introspection::<LockedSplitAtomicsCandidate>(&mut introspection, "split_atomics_locked");
    introspection.finish();

    for (group_name, rejected) in [("contention_success", false), ("contention_rejection", true)] {
        let mut contention = c.benchmark_group(format!("token_bucket/{group_name}"));
        for threads in [1, 2, 4, 8, 16] {
            bench_contention::<MutexCandidate>(&mut contention, "mutex", threads, rejected);
            bench_contention::<SplitAtomicsCandidate>(&mut contention, "split_atomics", threads, rejected);
            bench_contention::<LockedSplitAtomicsCandidate>(&mut contention, "split_atomics_locked", threads, rejected);
            bench_contention::<VirtualTimeCandidate>(&mut contention, "virtual_time_gcra", threads, rejected);
        }
        contention.finish();
    }
}

/// Benchmark sequential acquisitions, optionally advancing the clock per call.
fn bench_sequential<C: Candidate>(group: &mut BenchmarkGroup<'_, WallTime>, name: &str, now_step: u64) {
    group.bench_function(name, |b| {
        let candidate = C::full();
        let mut now_nanos = 0;
        b.iter(|| {
            let result = acquire_candidate(&candidate, black_box(now_nanos));
            now_nanos = now_nanos.saturating_add(now_step);
            black_box(result)
        });
    });
}

/// Benchmark the fast rejection path after the bucket is empty.
fn bench_rejection<C: Candidate>(group: &mut BenchmarkGroup<'_, WallTime>, name: &str) {
    group.bench_function(name, |b| {
        let candidate = C::empty();
        b.iter(|| black_box(acquire_candidate(&candidate, black_box(0))));
    });
}

/// Benchmark the current read-only token-count introspection operation.
fn bench_introspection<C: InspectableCandidate>(group: &mut BenchmarkGroup<'_, WallTime>, name: &str) {
    group.bench_function(name, |b| {
        let candidate = C::full();
        b.iter(|| black_box(current_candidate(&candidate, black_box(123_456_789))));
    });
}

/// Benchmark shared-bucket throughput at several thread counts.
fn bench_contention<C: Candidate>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    name: &str,
    threads: usize,
    rejected: bool,
) {
    group.bench_function(BenchmarkId::new(name, threads), |b| {
        b.iter_custom(|iterations| {
            let candidate = Arc::new(if rejected { C::single() } else { C::full() });
            let ready = Arc::new(Barrier::new(threads + 1));
            let go = Arc::new(AtomicBool::new(false));
            let finished = Arc::new(AtomicUsize::new(0));
            let total_acquired = Arc::new(AtomicU64::new(0));
            let threads_u64 = u64::try_from(threads).unwrap();
            let remainder = usize::try_from(iterations % threads_u64).unwrap();

            thread::scope(|scope| {
                let handles: Vec<_> = (0..threads)
                    .map(|worker| {
                        let candidate = Arc::clone(&candidate);
                        let finished = Arc::clone(&finished);
                        let go = Arc::clone(&go);
                        let ready = Arc::clone(&ready);
                        let total_acquired = Arc::clone(&total_acquired);
                        let count = iterations / threads_u64 + u64::from(worker < remainder);
                        scope.spawn(move || {
                            ready.wait();
                            while !go.load(Ordering::Acquire) {
                                std::hint::spin_loop();
                            }

                            let candidate = candidate.as_ref();
                            let mut now_nanos = 0_u64;
                            let mut acquired = 0_u64;
                            for _ in 0..count {
                                now_nanos = now_nanos.saturating_add(1);
                                let now = if rejected { now_nanos % 10 } else { now_nanos };
                                acquired += u64::from(acquire_candidate(candidate, black_box(now)).is_some());
                            }
                            total_acquired.fetch_add(acquired, Ordering::Relaxed);
                            finished.fetch_add(1, Ordering::Release);
                        })
                    })
                    .collect();

                ready.wait();
                let start = Instant::now();
                go.store(true, Ordering::Release);
                while finished.load(Ordering::Acquire) != threads {
                    std::hint::spin_loop();
                }
                let elapsed = start.elapsed();

                handles.into_iter().for_each(|handle| handle.join().unwrap());

                let actual_acquired = total_acquired.load(Ordering::Acquire);
                // Rejection cases start with one token to exercise the transition.
                let expected_acquired = if rejected { 1 } else { iterations };
                assert_eq!(actual_acquired, expected_acquired);
                black_box(actual_acquired);
                elapsed
            })
        });
    });
}

/// Convert nanoseconds to seconds without overflowing the floating mantissa.
#[expect(
    clippy::cast_precision_loss,
    reason = "benchmark timestamps are well below f64's exact integer range"
)]
fn nanos_to_secs(nanos: u64) -> f64 {
    let whole_secs = nanos / 1_000_000_000;
    let remainder = nanos % 1_000_000_000;
    whole_secs as f64 + remainder as f64 / 1_000_000_000.0
}
