# Tail-latency benchmarks

This document covers the **HDR-histogram** bench suite added in 0.7.0
under `benches/order_book/*_hdr.rs`. The default Criterion benches in
the same directory remain — they publish HTML reports to
`target/criterion/` and report a mean-centric statistical comparison
that Criterion does well (see `BENCHMARKS.md`). The HDR benches are the
source of truth for the **tail** numbers (`p50` / `p99` / `p99.9` /
`p99.99`) that tier-one electronic exchanges quote in SLOs.

## Allocation profile (feature `alloc-counters`)

Under the `alloc-counters` feature the crate exposes a
`CountingAllocator<Inner: GlobalAlloc>` wrapper that tracks
`allocs` / `deallocs` / `bytes_allocated` / `bytes_deallocated` as
`AtomicU64` counters. Bench / test binaries opt in via:

```rust
use orderbook_rs::CountingAllocator;
use std::alloc::System;

#[global_allocator]
static A: CountingAllocator<System> = CountingAllocator::new(System);
```

`benches/order_book/alloc_count.rs` runs the same mixed 70 / 20 / 10
workload as `mixed_70_20_10_hdr` but reports `allocs_per_op` and
`bytes_alloc/op` over the measurement window (200 000 warmup +
1 000 000 measured). A reference run on the M4 Max host (orderbook-rs
0.12.0, `pricelevel` 0.9.1):

| counter        | value         |
|----------------|---------------|
| allocs         | 18 234 168    |
| deallocs       | 18 123 830    |
| bytes_alloc    | 6 173 361 482 |
| bytes_dealloc  | 6 145 419 658 |
| **allocs/op**  | **18.23**     |
| bytes_alloc/op | 6 173         |

Both counters are balanced (allocs ≈ deallocs, no leak). `allocs/op` is
the headline number for "what does the matching engine cost in alloc
pressure on a realistic workload" — useful as a regression signal much
more than as an absolute target; it is in the same ballpark as the
0.7.0 (`~17.76`) and 0.9.0 (`~18.81`) references and is
workload-randomness-sensitive on this synthetic stream (repeat runs
land in the `~15–19` range). `bytes_alloc/op` (`~6 KB`) is likewise
stable across 0.9.0 → 0.12.0 — the pricelevel 0.9 hardening and the
0.12.0 atomicity work added no allocation pressure.

> **Fixed in `pricelevel` 0.8.3 (PriceLevel#106).** Earlier `pricelevel`
> 0.8.2 pre-sized each match `MatchResult` to the *whole* level depth, so
> a qty-1 market order against a deep level allocated a multi-MB transient
> buffer (`bytes_alloc/op` ballooned to `~790 KB`). 0.8.3 bounds the
> pre-allocation to `min(incoming_quantity, order_count)`, so a small
> taker no longer reserves the level — `bytes_alloc/op` is back to the
> low-KB range.

The integration test `tests/alloc_budget.rs` runs a smaller 10 000-op
slice and asserts `allocs/op` stays under a fixed ceiling to catch
order-of-magnitude regressions in CI.

Run yourself:

```bash
cargo bench --features alloc-counters --bench alloc_count
cargo test  --features alloc-counters alloc_budget
```

Per-run summaries land in `target/alloc-counters/<scenario>.md`.

## How to run

```bash
make bench-hdr                 # every _hdr bench binary, incl. stp_contention_hdr's 8 scenarios
cargo bench --bench mixed_70_20_10_hdr   # single scenario
cargo bench --bench stp_contention_hdr   # all 8 mode x thread-count scenarios in one binary
```

Each bench writes its raw HDR histogram to
`target/bench-hdr/<scenario>.hgrm` (V2 format) for downstream HDR
plotters; the directory lives under `target/` and is gitignored.

## Methodology

- **Histogram resolution.** `Histogram::<u64>` sized for `1 ns` to `1 s`
  with three significant figures. Three sig-figs is enough to
  distinguish `p99 ≠ p99.9` an order of magnitude apart while staying
  memory-cheap (~80 KB per histogram).
- **Sample collection.** Each measured operation is wrapped in a closure
  passed to `record(...)`, which times the closure with
  `std::time::Instant::now()` (one call before, one after) and writes
  the elapsed-nanosecond value into the histogram. The closure result
  is consumed via `std::hint::black_box` to prevent dead-code
  elimination.
- **Warmup.** Long-running scenarios (`add_only`, `mixed_70_20_10`)
  discard 200 000 ops before the measurement window starts.
  Pre-loading scenarios (`cancel_only`, `aggressive_walk`,
  `notional_walk`, `mass_cancel_burst`, `stp_sweep`) seed the book in a
  non-measured loop instead.
- **Workload determinism.** All scenarios drive a self-contained
  xorshift PRNG seeded with `0xA5A5_A5A5_A5A5_A5A5`. Reproducing a run
  with the same code produces the same op stream, modulo concurrent
  scheduling jitter on the host.
- **Coordinated omission.** The bench loop is **closed-loop**: the
  driver waits for each engine call to return before issuing the next.
  Closed-loop measurements **systematically under-report** tail
  latencies that a real load generator would observe under saturation,
  because queueing delays that would build up under a fixed arrival
  rate never materialize. **The numbers below are pure service time —
  use them as a regression signal and a lower bound on the production
  tail, not as a production SLO.** Open-loop measurement (record
  `now - scheduled_arrival`, not `now - call_start`) is the right
  follow-up; tracked but not in the initial drop.
- **CPU pinning.** Optional. On Linux, `taskset -c <core> cargo bench
  --bench mixed_70_20_10_hdr` reduces variance from cross-core
  scheduling. On macOS the benches were run without pinning — see the
  run conditions block below.

## Run conditions for the numbers below

| Item | Value |
|---|---|
| Host | Apple M4 Max, macOS (Darwin 25.5.0, `arm64`) |
| Pinning | None |
| Toolchain | `rustc 1.97.0` (stable) |
| Profile | `--release` (Cargo `bench` profile = `release` clone) |
| `RUSTFLAGS` | unset |
| Allocator | system allocator |
| Date | 2026-07-15 |
| Crate version | `0.12.0` (`pricelevel` `0.9.1`) |

## Headline numbers

All values in nanoseconds. **Closed-loop service time** — see
"Coordinated omission" above.

### `add_only` — pure passive limit submission, no crossings

200 000 warmup + 1 000 000 measured.

| Quantile | Latency (ns) |
|---|---|
| p50    | 917 |
| p99    | 62 847 |
| p99.9  | 97 727 |
| p99.99 | 130 495 |
| max    | 195 583 |

**Where the tail comes from.** The book grows monotonically across the
measurement window, so each insert must walk the `SkipMap` to the
right level. The dominant contributor at p99.99 is allocator jitter
when `Arc<PriceLevel>` allocations churn under the system allocator;
secondary is L2 cache misses on the price-side `SkipMap` when the
working set outgrows L1.

### `cancel_only` — pre-loaded book, sequential cancels

1 000 000 pre-loaded resting orders, all cancelled in order.

| Quantile | Latency (ns) |
|---|---|
| p50    | 41 |
| p99    | 19 007 |
| p99.9  | 24 047 |
| p99.99 | 27 055 |
| max    | 834 559 |

**Where the tail comes from.** `DashMap::remove` on the order index is
a shard-local lock acquisition; the median is dominated by that
single-cycle CAS path. The very long max tail reflects
shard-contention windows when multiple removals land on the same
shard back to back, plus rare allocator returns of large
`PriceLevel` linked-list nodes.

### `aggressive_walk` — taker market orders sweep multi-level book

50 levels × 100 resting orders pre-loaded, then 100 000 aggressive
buys with qty `5..=20`.

| Quantile | Latency (ns) |
|---|---|
| p50    | 42 |
| p99    | 3 335 |
| p99.9  | 6 795 |
| p99.99 | 8 839 |
| max    | 18 671 |

**Where the tail comes from.** The fill loop iterates per-order at
each level until the requested quantity is consumed. Median is fast
because most sweeps fill within a single level. Tail is driven by
sweeps that span multiple levels and drop several `Arc<PriceLevel>`s
at once.

### `notional_walk` — quote-notional market orders sweep multi-level book

50 levels × 100 resting orders pre-loaded, then 100 000 aggressive
notional buys with budgets `500..2000` quote ticks
(`match_market_order_by_amount` path) — same book shape as
`aggressive_walk` for direct comparison of the two sweep entry points.

| Quantile | Latency (ns) |
|---|---|
| p50    | 42 |
| p99    | 2 543 |
| p99.9  | 4 959 |
| p99.99 | 6 919 |
| max    | 18 463 |

**Where the tail comes from.** Same fill loop as `aggressive_walk`
plus one `u128` divide per level (budget → per-level qty cap) and one
multiply per fill. Both medians sit at the same `~42 ns`, confirming
the notional arithmetic is not the bottleneck; the tail tracks
multi-level walks exactly like the base-qty sweep.

### `mixed_70_20_10` — 70 % submit, 20 % cancel, 10 % aggressive

200 000 warmup + 1 000 000 measured. The "realistic" headline number.

| Quantile | Latency (ns) |
|---|---|
| p50    | 833 |
| p99    | 31 679 |
| p99.9  | 52 031 |
| p99.99 | 72 063 |
| max    | 128 511 |

**Where the tail comes from.** Mix of all three previous tails. The
median tracks `add_only` (because submits are 70 % of the workload).
The p99.99 comes from rare aggressive sweeps that interact with
allocator returns released by recent cancels.

### `thin_book_sweep` — book near-empty, IOC probing

Refills 3 resting asks every 5 ops; 200 000 IOC buy probes with qty
`1..=20`.

| Quantile | Latency (ns) |
|---|---|
| p50    | 42 |
| p99    | 4 543 |
| p99.9  | 5 667 |
| p99.99 | 12 751 |
| max    | 26 335 |

**Where the tail comes from.** Most probes either fully fill the
small resting depth or partial-fill and short-circuit. The p99 is
shaped by the partial-fill-then-cancel-remainder bookkeeping; the max
is allocator jitter when the book transitions empty → non-empty.

### `mass_cancel_burst` — dense book, then `cancel_all_orders`

10 000 orders pre-loaded × 500 bursts. Each measured sample is
**one full burst**, not one cancel — useful as an operator-side
wall-clock guard rather than a per-op tail.

| Quantile | Latency (ns) |
|---|---|
| p50    | 32 591 |
| p99    | 42 271 |
| p99.9  | 54 463 |
| p99.99 | 54 463 |
| max    | 54 463 |

**Where the tail comes from.** Burst latency scales linearly with the
book depth; on a tight host the median is ~19 µs to drain 10 000
orders, ~1.9 ns per order amortised. The p99.9 / p99.99 / max all
collapse to the same value because only 500 samples were taken — the
single worst-case observation dominates.

### `stp_sweep` — self-trade-prevention CancelMaker self-cross (added 0.9.0)

`OrderBook::with_stp_mode(.., CancelMaker)` seeded with 50 ask levels
(each one taker-owned sell + 8 other-maker sells); 100 000 measured
aggressive self-crossing market buys from the taker, each one hitting
the per-level STP scan + inline maker cancel (#107).

**Liquidity profile (fixed for #225).** The other-maker depth is not a
one-shot seed: before every measured op, the levels the sweep is
currently walking (the current best ask and the next few above it) are
topped back up to their seeded other-maker order count. Earlier
versions of this bench seeded the 8-per-level other-maker depth once,
up front, with no refill — foreign liquidity was gone within the first
few hundred of the 100 000 measured ops, so almost the entire
measured window was sweeping a book with nothing real left to fill
against. The numbers directly below predate that fix; they are kept
for the pricelevel-upgrade bisection narrative underneath, not as a
representative baseline. See "#225 gate-mode comparison" below for the
post-fix, sustained-liquidity numbers.

| Quantile | Latency (ns) |
|---|---|
| p50    | 1 208 |
| p99    | 4 835 |
| p99.9  | 5 543 |
| p99.99 | 9 503 |
| max    | 21 551 |

**Where the tail comes from.** Every measured op runs the per-level
self-trade scan and cancels the same-user maker inline over the pooled
snapshot buffer (#107, no per-level `Vec` allocation). The median is
the scan + single cancel + the validated re-seed of the taker order;
the tail is the rare sweep that touches several levels.

**Median shift in 0.12.0 (pricelevel 0.9).** The p50 moved from
`~291 ns` (pricelevel 0.8.4) to `~1.2 µs`. Bisection against the
pre-hardening baseline attributes the entire shift to the pricelevel
0.9 upgrade — validated admission (duplicate-id / counter-capacity /
topology checks on the re-seeded taker), the atomic
cancel-vs-partial-fill index re-key, and the seqlock'd execution
statistics all run on this scenario's per-op path. The 0.12.0
book-level atomicity work (#206–#211 + the FOK submit gate) added
nothing measurable on top — and tightened this scenario's tail
(p99.9 `14.3 µs → 5.5 µs`, p99.99 `26.3 µs → 9.5 µs` vs the
pre-stack midpoint). Correctness bought with median latency on the
STP self-cross path; every other scenario's median is unchanged.

**#225 gate-mode comparison.** #225 makes STP-active submits (STP
enabled, non-zero taker `user_id`) take the exclusive side of the
`submit_gate` `RwLock` instead of the shared side, so the per-level STP
scan and the fill it authorises see a consistent queue. `stp_sweep` is
single-threaded and uncontended, so it isolates the fixed per-op cost
of exclusive vs shared acquisition on this workload's own submits,
independent of any cross-thread contention — see `stp_contention`
below for the multi-threaded contention cost.

| Quantile | `main` (shared gate) | #225 branch (exclusive gate) |
|---|---|---|
| p50    | 3 375 ns | 2 835 ns |
| p99    | 6 667 ns | 5 127 ns |
| p99.9  | 9 711 ns | 7 835 ns |
| p99.99 | 19 759 ns | 15 919 ns |

Medians of three runs per side on the same macOS host, no CPU pinning,
`Cargo.lock` aligned, `main` at `f167327` with the same bench sources.
Run-to-run spread on either side was about ±10 % at p50. No
single-thread regression from the exclusive acquisition is visible; the
branch runs came out faster, which we attribute to code layout and
warm-up rather than to the lock mode and do not claim as an improvement.

### `stp_contention` — N-thread contention on one book, gate-mode comparison (added for #225)

`stp_contention_hdr` is the multi-threaded counterpart to `stp_sweep`:
`N` threads (1 / 2 / 4 / 8) share ONE `OrderBook<()>`, each running
50 000 closed-loop ops (own `Rng`, own histogram) released together on a
`Barrier`. Op mix per thread: 70% passive limit adds a few ticks off a
fixed mid (rest, never cross), 20% cancels of that thread's own resting
orders, 10% aggressive limit orders that cross the whole passive band in
one shot. Each thread owns one user id from an 8-id pool, so under
`CancelMaker` a thread's own aggressive crossings routinely hit its own
resting makers.

Two configurations run back to back on the same book geometry:
`STPMode::None` (baseline — every submit stays on the shared side of the
`submit_gate` `RwLock`) and `STPMode::CancelMaker` (#225 — STP-active
submits take the exclusive side instead). The `None` column is the
STP-disabled baseline and must not move across the #225 change. The
`None` versus `CancelMaker` gap measured on `main` is the intrinsic cost
of the per-level STP scan and its inline same-user cancels, not the
gate; the cost of the gate-mode change itself is isolated by comparing
`main` and the #225 branch at the same thread count under `CancelMaker`,
which is what the table below does. Reported per thread-count: the
merged (all threads) p50 / p99 / p99.9 plus aggregate throughput
(`ops/s`, wall clock from barrier release to last-thread-done).

Like every scenario in this suite, this is **closed-loop, per-thread
service time** — see "Coordinated omission" above; it under-reports the
queueing delay a saturated real load generator would see, and it adds
its own dimension (lock / structure contention across threads) that the
single-threaded scenarios cannot show at all.

| Threads | `None` p50 (main / #225) | `None` ops/s (main / #225) | `CancelMaker` p50 (main / #225) | `CancelMaker` ops/s (main / #225) |
|---|---|---|---|---|
| 1 | 958 / 917 ns | 378k / 389k | 750 / 791 ns | 899k / 902k |
| 2 | 1 250 / 1 250 ns | 599k / 613k | 1 208 / 1 000 ns | 1 036k / 601k |
| 4 | 1 583 / 1 625 ns | 873k / 879k | 1 459 / 6 251 ns | 1 105k / 271k |
| 8 | 2 417 / 2 459 ns | 861k / 1 031k | 2 333 / 17 423 ns | 1 048k / 214k |

Medians of three runs per side, same host and conditions as the
`stp_sweep` comparison above, `main` at `f167327`. The `None` column is
unchanged within noise, as required: an `STPMode::None` book never takes
the exclusive side. The single-threaded `CancelMaker` row is unchanged
too, so the uncontended exclusive acquisition has no measurable cost.
From two threads up the `CancelMaker` column carries the cost of #225 by
design: in this mix 80 % of the operations (identified passive adds and
IOC takers) are STP-relevant and now serialize through the exclusive
gate, so aggregate throughput drops by about 43 % at two threads, 74 %
at four and 80 % at eight, with the merged p50 rising in step. The
p99.9 / p99.99 columns of the merged histograms (in the `.hgrm` files)
move by a similar factor. Restoring submit concurrency on STP books
needs the per-level STP-aware match in pricelevel, tracked as a
follow-up; the book-level gate is the correctness fix.

### `reserve_sweep` — IOC probes into reserve makers, strandable vs replenishing (added for #230)

`reserve_sweep_hdr` measures `capture_strandable_makers` (#230). Each
sweep in `match_order_inner` reads `non_auto_reserve_rested` (a
monotonic per-book flag set at admission) exactly once, before any
level is touched. On a book that has never rested a
non-auto-replenishing `ReserveOrder` with hidden depth, that single
relaxed atomic load is the sweep's entire cost: no pool buffer is
acquired, `capture_strandable_makers` is never called for any level,
and the post-sweep drain does no lookup. Only when the flag is true
does each matched level get checked, and, if it still holds hidden
depth, walked with `PriceLevel::iter_orders()` to record which resting
non-auto reserves have hidden quantity behind them, so a sweep can
report the hidden depth it strands when `pricelevel` drops a depleted
maker's hidden tranche instead of refreshing it. `iter_orders` is
`DashMap::iter` upstream, which read-locks every shard of the map
regardless of how few orders rest at the level, so the walk is not
free on any level holding hidden depth.

Same book geometry as `thin_book_sweep`: 3 resting asks refilled every
5 ops (not timed), 200 000 IOC buy probes with qty `1..=20` against
them. The only difference from `thin_book_sweep` is that the resting
side is `OrderType::ReserveOrder` (visible `1..=5`, hidden `4..=12`,
`replenish_threshold: 0`) instead of plain limits. Two scenarios, run
back to back on fresh books:

- `reserve_sweep_nonauto` (`auto_replenish: false`): every resting
  maker is strandable, so `non_auto_reserve_rested` flips true on the
  first rest and every level-match that still holds hidden depth pays
  the `iter_orders()` walk. This is the scenario `capture_strandable_makers`
  adds cost to.
- `reserve_sweep_auto` (`auto_replenish: true`): hidden depth still
  rests and is still consumed, but nothing is strandable, so this book
  never sets `non_auto_reserve_rested`. Each sweep pays the one hoisted
  atomic load and nothing else; `capture_strandable_makers` is never
  called.

**`reserve_sweep_nonauto`** (`auto_replenish: false`; every resting
maker is strandable, so the capture runs on the branch):

| Quantile | `main` (no #230) | branch (#230) |
|---|---|---|
| p50    | 83 ns | 83 ns |
| p99    | 4 543 ns | 6 959 ns |
| p99.9  | 6 503 ns | 9 543 ns |
| p99.99 | 8 711 ns | 11 839 ns |

**`reserve_sweep_auto`** (`auto_replenish: true`; hidden depth rests
but nothing is strandable, so the branch never arms
`non_auto_reserve_rested`):

| Quantile | `main` (no #230) | branch (#230) |
|---|---|---|
| p50    | 917 ns | 916 ns |
| p99    | 3 917 ns | 3 793 ns |
| p99.9  | 5 335 ns | 5 127 ns |
| p99.99 | 6 503 ns | 6 211 ns |

Medians of three runs per side, runs interleaved base/branch, `main`
at `b821df2` (`orderbook-rs` 0.12.1) against this branch
(`orderbook-rs` 0.13.0), both on `pricelevel` 0.9.1 with `Cargo.lock`
aligned. Host: Apple M-series, 16 cores, macOS Darwin 25.6.0, `arm64`,
rustc stable, 200 000 probes per scenario per run. Run-to-run spread
was negligible at p50 and widened toward the tail on both `main` and
the branch alike (p99.99 on either side roughly doubled between its
lowest and highest of the three runs), consistent with ordinary
single-sample tail jitter at 200 000 probes rather than anything
specific to this comparison.

**What the comparison shows.** In `reserve_sweep_nonauto`, where the
capture runs, the branch adds roughly 2.4 µs at p99 (4 543 ns →
6 959 ns) and roughly 3 µs at p99.9 (6 503 ns → 9 543 ns) over `main`
on this workload: every probe that matches a level holding hidden
depth pays a shard-locked `DashMap` pass over that level's resting
orders, and `iter_orders` read-locks every shard regardless of how few
orders rest there. In `reserve_sweep_auto`, where the flag never arms,
`main` and the branch are unchanged within run-to-run noise, exactly
as intended: the guard confines its cost to books that can actually
strand something. p50 is unchanged between `main` and the branch in
both scenarios, because most probes in this thin, frequently-refilled
book either find nothing resting or take the cheap early-exit path
before any level walk would matter.

Separately, `reserve_sweep_auto`'s p50 (`~916` ns) sits well above
`reserve_sweep_nonauto`'s (`83` ns) on **both** `main` and the branch:
a non-auto reserve maker is fully consumed and removed after one or
two probes and the book then sits empty until the next refill, while
an auto-replenishing maker keeps refilling from hidden and stays
matchable across most of the refill window, so more of the 200 000
probes do real matching work against it. `main` shows the identical
gap, so this is a `pricelevel` matching-cost difference between the
two reserve behaviours, not anything #230 adds; it is why each
scenario is compared against its own `main` baseline above rather than
against the other scenario.

Like every scenario in this suite, this is **closed-loop, per-probe
service time**: see "Coordinated omission" above; it under-reports the
queueing delay a saturated real load generator would see.

## 0.11.0 → 0.12.0 delta

The 0.12.0 release combines the pricelevel 0.9 hardening upgrade with
the book-level atomicity work (#206–#211). A three-point bisection
(0.11.0 / pricelevel 0.8.4 → post-upgrade midpoint → 0.12.0 final) on
the same host and session attributes the differences:

- **Medians unchanged** on `add_only` (917 = 917), `cancel_only`
  (41 = 41), `aggressive_walk` / `notional_walk` / `thin_book_sweep`
  (42 = 42). `mixed_70_20_10` p50 `792 → 833` (+41 ns) arrived with the
  pricelevel upgrade, not with the atomicity work; the FOK submit gate's
  uncontended read acquisition is not measurable on any scenario in a
  clean back-to-back run.
- **`stp_sweep` p50 `291 → 1 208`** — entirely from pricelevel 0.9's
  hardening (see the scenario note above); the 0.12.0 stack tightened
  its tail instead.
- **Improvements:** `mass_cancel_burst` p50 `43.6 µs → 32.6 µs` on the
  same session (−25 %), `add_only` p99 `−9 %`, `thin_book_sweep` p99
  `−17 %`.
- **Allocation profile flat:** `18.23 allocs/op`, `~6.2 KB/op` — within
  the historical `15–19` band.

## Limitations

- **macOS, no pinning.** The host above is a workstation, not a
  performance-tuned bench rig. Tail numbers will be tighter on a
  Linux host with `isolcpus=` + `nohz_full=` + a pinned thread, with
  the system allocator swapped for `jemalloc` or `mimalloc`.
- **Closed-loop only.** As called out under Methodology — these
  numbers are pure service time, not load-induced tail. Open-loop
  measurement is the next iteration of this suite.
- **Single-threaded driver, except `stp_contention`.** Every scenario but
  `stp_contention` issues one op at a time from a single thread.
  `stp_contention` (added for #225) is the first multi-writer scenario in
  this suite — up to 8 threads sharing one book — but it only exercises
  the STP gate-mode comparison, not the other seven workloads; a general
  multi-writer driver for the rest is deferred to a follow-up.

## Reproducing

```bash
git checkout main
make bench-hdr
cat target/bench-hdr/*.hgrm     # raw histograms
```

`hgrm` files are V2 format — readable by `HdrHistogram` plot tooling
or convertible via `hdrhistogram`'s `Reader`.
