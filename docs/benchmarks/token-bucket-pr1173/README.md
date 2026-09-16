# PR #1173 Token-Bucket Benchmark Artifacts

This directory preserves the benchmark code's supporting reports and raw outputs
so the measurements can be revisited without relying on temporary workstation or
remote-host paths.

The readable summary is [`summary.html`](summary.html). A shareable copy was also
published as a secret [GitHub Gist](https://gist.github.com/jordigilh/5c2f230d9e12c690c44f8c1bd3015b4b).

## Scope

The executable benchmark is `tests/benches/benches/token_bucket.rs`. It compares:

- `mutex`: the production compound state from PR #1173.
- `split_atomics_locked`: a correctness-preserving two-field variant protected
  by an atomic spin lock. It is a serialization baseline, not lock-free.
- `virtual_time_gcra`: a single-atomic theoretical-arrival/GCRA alternative.
  It changes the state and introspection semantics and is not production code.

The current suite has 41 cases. It excludes the historical unsafe split-atomic
implementation from runnable comparisons. Its old logs remain in `results/`
only as provenance for why that option was rejected.

## Method

Runs were performed on `helios08`:

- Intel Xeon Gold 5218R, 20 physical cores / 40 logical CPUs.
- One socket and one NUMA node, approximately 257 GB RAM.
- Fedora 43, Rust 1.98.0.
- Corrected contention timing uses synchronized worker starts, excludes worker
  setup and joins, and checks expected acquisition totals.
- Pinned runs use process affinity on physical CPU ranges `0-(N)` plus one
  coordinating core. This is not individual worker-thread binding.
- Corrected rounds use 50 samples, 3 seconds of warmup, and 2 seconds of
  measurement.
- The telemetry pass uses `turbostat`, `mpstat`, `numactl`, and `perf`.

The VM exposes no cpufreq driver or governor policy. Telemetry showed a 2.095-2.096
GHz TSC, observed busy clocks from 2.875 to 3.800 GHz, zero steal time, no thermal
throttling, and core temperatures of 40-47 C. Absolute nanoseconds are therefore
host-specific; the relative ordering is the useful result.

## Result

Median of three corrected locked-retry rounds for successful contention:

| Workers | Mutex | Locked split fields | Virtual time/GCRA |
| --- | ---: | ---: | ---: |
| 1 | 14.515 ns | 10.028 ns | 10.814 ns |
| 2 | 30.760 ns | 171.320 ns | 74.811 ns |
| 4 | 79.599 ns | 201.980 ns | 128.850 ns |
| 8 | 239.820 ns | 441.860 ns | 233.670 ns |
| 16 | 817.370 ns | 855.690 ns | 357.140 ns |

The locked split version is correct, but its apparent split-atomic advantage
disappears after serialization is restored. Virtual time is faster at high
contention, but it is an algorithmic alternative rather than an API-preserving
replacement.

## Reproduction

Run the complete suite:

```console
cargo bench -p praxis-tests-benches --bench token_bucket
```

Run one corrected contention case:

```console
taskset -c 0-16 token_bucket-bench --bench --noplot \
    --sample-size 50 --measurement-time 2 --warm-up-time 3 \
    --discard-baseline \
    --exact token_bucket/contention_success/split_atomics_locked/16
```

Remote verification passed with `cargo check --benches` and `cargo test --benches`.
The only warning was the existing unfulfilled lint expectation in
`crates/filter/src/builtins/http/traffic_management/router/json_alias.rs`.

## Raw Outputs

- `results/token_bucket-final-*.log`: original three 41-case runs.
- `results/token-bucket-corrected-pinned-r*.log`: corrected three-round runs;
  historical unsafe split results are archived in these mixed-candidate logs but
  are excluded from current comparisons.
- `results/token-bucket-corrected-pinned-spike.log`: exploratory corrected run.
- `results/preflight-corrected-pinned.txt`: host and workload preflight.
- `results/perf-*.log`: hardware-counter runs.
- `results/telemetry-corrected/`: turbostat-wrapped representative runs.
- `results/split-locked-corrected/`: three locked split-field retry rounds.
