---
name: bench-hdr
description: Add or update an orderbook-rs hot-path latency benchmark that reports p50 / p99 / p99.9 / p99.99 via hdrhistogram, not the criterion default mean. Use when adding a benchmark for an end-to-end scenario (add-only, cancel-only, aggressive walk, mixed 70/20/10, thin-book IOC sweep, mass-cancel burst, snapshot capture), or when updating an existing bench after a hot-path change. Handles warmup, coordinated-omission disclosure, and a short interpretation block for BENCH.md.
allowed-tools: Read, Write, Edit, Grep, Glob, Bash
---

# Skill: bench-hdr

Generates or updates a reproducible local benchmark for the `orderbook-rs` hot path.
Output is an HDR-histogram dump with tail quantiles and a short interpretive paragraph
that goes into `BENCH.md`.

Criterion ships with `html_reports` in this crate, but its default output is mean-centric
and drops the tails. The hot-path SLO for a matching engine is p99 / p99.9 / p99.99; mean
is a vanity metric. This skill adds a parallel hdrhistogram pipeline that lives alongside
the Criterion benches.

## When to invoke

- Adding a benchmark for a new scenario.
- Re-running benchmarks after a change to matching / operations / modifications /
  repricing / mass_cancel / fees to document p99 movement.
- User says "benchmark this", "add a bench", "measure p99.9", "update BENCH.md".

## Prerequisites

The crate already depends on `criterion = { version = "0.8", features = ["html_reports"] }`
in `Cargo.toml` and has a `benches/` directory organized under `benches/order_book/`.
Before the first `bench-hdr` bench, add `hdrhistogram` as a dev-dependency:

```
cargo add --dev hdrhistogram@^7
```

Confirm the addition with `rg -n 'hdrhistogram' Cargo.toml`.

## Procedure

### 1. Scenario taxonomy

Pick exactly one per bench file. Each lives under `benches/order_book/` alongside the
existing Criterion benches.

| Scenario              | Input profile                                                          |
|-----------------------|------------------------------------------------------------------------|
| `add_only`            | Pure passive limit submissions, no crossings. Measures insert cost.    |
| `cancel_only`         | Pre-loaded book, cancel workload. Measures `DashMap` lookup + unlink.  |
| `aggressive_walk`     | Taker IOC sweeps across several levels. Measures fill-loop tail.       |
| `mixed_70_20_10`      | 70% submits, 20% cancels, 10% aggressive IOC. Most "realistic".        |
| `thin_book_sweep`     | Book near-empty, IOC probing. Exercises partial-fill / reject path.    |
| `mass_cancel_burst`   | Dense book, then one mass-cancel. Measures bulk-cancel worst case.     |
| `snapshot_capture`    | Dense book, repeated `snapshot()` / `snapshot_package()` calls.        |

### 2. File placement

- Bench file: `benches/order_book/<scenario>_hdr.rs` (the `_hdr` suffix keeps it visually
  distinct from the existing Criterion benches; they coexist).
- Workload helpers: `benches/order_book/hdr_common.rs`.
- HDR recorder wrapper: a small `record` helper inline in `hdr_common.rs`.
- Output directory for raw histograms: `target/bench-hdr/<scenario>.hgrm` (already
  inside `target/`, so already gitignored).
- Summary table: `BENCH.md` at the repo root (committed).

### 3. Register the bench in `Cargo.toml`

```toml
[[bench]]
name = "mixed_70_20_10_hdr"
path = "benches/order_book/mixed_70_20_10_hdr.rs"
harness = false
```

`harness = false` so the bench is a plain binary and we control the measurement loop. The
default Criterion harness is not suitable for tail latency — we need per-sample recording
into an HDR histogram, not a wall-clock-bound iteration count driven by statistical
convergence.

### 4. Template — measurement core (`benches/order_book/hdr_common.rs`)

```rust
use hdrhistogram::Histogram;
use std::time::Instant;

/// Histogram sized for 1 ns .. 1 s with 3 significant figures.
pub fn new_histogram() -> Histogram<u64> {
    Histogram::<u64>::new_with_bounds(1, 1_000_000_000, 3).expect("hist bounds")
}

/// Measure a closure once, record nanoseconds into the histogram.
#[inline(always)]
pub fn record<F, R>(h: &mut Histogram<u64>, f: F) -> R
where
    F: FnOnce() -> R,
{
    let t0 = Instant::now();
    let r = std::hint::black_box(f());
    let elapsed = t0.elapsed().as_nanos() as u64;
    h.record(elapsed.max(1)).expect("record");
    r
}

pub fn report(name: &str, h: &Histogram<u64>) {
    println!("scenario   : {}", name);
    println!("samples    : {}", h.len());
    println!("p50   (ns) : {}", h.value_at_quantile(0.50));
    println!("p99   (ns) : {}", h.value_at_quantile(0.99));
    println!("p99.9 (ns) : {}", h.value_at_quantile(0.999));
    println!("p99.99(ns) : {}", h.value_at_quantile(0.9999));
    println!("max   (ns) : {}", h.max());
    println!("min   (ns) : {}", h.min());
}

pub fn persist(name: &str, h: &Histogram<u64>) -> std::io::Result<()> {
    use hdrhistogram::serialization::V2Serializer;
    std::fs::create_dir_all("target/bench-hdr")?;
    let path = format!("target/bench-hdr/{}.hgrm", name);
    let mut f = std::fs::File::create(&path)?;
    V2Serializer::new()
        .serialize(h, &mut f)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    eprintln!("wrote {}", path);
    Ok(())
}
```

### 5. Template — the bench binary

Example `benches/order_book/mixed_70_20_10_hdr.rs` (adapt the exact call signatures to the
public API as it exists at the time the bench is written; verify with
`rg -n 'pub fn submit|pub fn cancel|pub fn mass_cancel' src/orderbook/operations.rs src/orderbook/modifications.rs src/orderbook/mass_cancel.rs`):

```rust
#[path = "hdr_common.rs"]
mod hdr_common;

use hdr_common::{new_histogram, persist, record, report};
use orderbook_rs::prelude::*;

const WARMUP_OPS:   usize =   200_000;
const MEASURED_OPS: usize = 1_000_000;
const SEED:         u64   = 0xA5A5_A5A5;

enum Op {
    Submit { id: Id, owner: u64, side: Side, price: Price, qty: Quantity, tif: TimeInForce },
    Cancel(Id),
    Aggressive { id: Id, owner: u64, side: Side, qty: Quantity },
}

fn build_mixed_workload(n: usize, seed: u64) -> Vec<Op> {
    // Deterministic PRNG from `seed`; no rand crate — use a small xorshift so the bench
    // is self-contained and reproducible. Tight price band (99..=101) for frequent
    // crossings on the aggressive slice.
    let mut s = seed;
    let mut next = || { s ^= s << 13; s ^= s >> 7; s ^= s << 17; s };
    let mut ops = Vec::with_capacity(n);
    for i in 0..n {
        let bucket = next() % 100;
        let id = Id::from_u64(i as u64 + 1);
        let owner = (next() % 4) + 1;
        let side = if next() % 2 == 0 { Side::Buy } else { Side::Sell };
        let price = Price::from_u64(99 + (next() % 3));
        let qty = Quantity::from_u64(1 + (next() % 100));
        if bucket < 70 {
            ops.push(Op::Submit { id, owner, side, price, qty, tif: TimeInForce::Gtc });
        } else if bucket < 90 {
            ops.push(Op::Cancel(Id::from_u64(1 + (next() % (i as u64).max(1)))));
        } else {
            ops.push(Op::Aggressive { id, owner, side, qty });
        }
    }
    ops
}

fn apply(book: &OrderBook<()>, op: &Op) {
    match op {
        Op::Submit { id, owner, side, price, qty, tif } => {
            let _ = book.submit_limit(*id, *owner, *side, *price, *qty, *tif);
        }
        Op::Cancel(id) => {
            let _ = book.cancel(*id);
        }
        Op::Aggressive { id, owner, side, qty } => {
            let _ = book.submit_market(*id, *owner, *side, *qty);
        }
    }
}

fn main() {
    let workload = build_mixed_workload(WARMUP_OPS + MEASURED_OPS, SEED);
    let book = OrderBook::<()>::new("BENCH");
    let mut h = new_histogram();

    // Warmup — discarded.
    for op in &workload[..WARMUP_OPS] { apply(&book, op); }

    // Measurement.
    for op in &workload[WARMUP_OPS..] {
        record(&mut h, || apply(&book, op));
    }

    report("mixed_70_20_10", &h);
    persist("mixed_70_20_10", &h).expect("persist");
}
```

### 6. Coordinated-omission handling

The loop above is a **closed-loop** benchmark (the driver waits for each op to finish
before issuing the next). Under saturation this systematically *under-reports* tail
latency because coordinated omission hides queueing stalls. Two options; pick one and
document the choice in `BENCH.md`:

- **Option A — open-loop with expected arrival interval.** Record `now - scheduled_arrival`,
  not `now - ingest_start`. Requires picking a target rate (e.g. 500k ops/s). CO is
  handled by construction, no separate disclosure.
- **Option B — closed-loop with explicit "pure service time" caveat.** Call out in
  `BENCH.md` that the numbers are pure service time, not tail under load. Useful as a
  lower bound and a regression signal, but not a production SLO.

For a regression-signal bench inside a lock-free crate, Option B is acceptable **if** you
are explicit. Option A, done sloppily, is worse than Option B done honestly.

### 7. Run conditions — document in `BENCH.md`

Missing entries are a negative signal to any reviewer.

- CPU model, core count, frequency governor (`performance` or `powersave`).
- Whether the bench was CPU-pinned (`taskset -c 2 cargo bench --bench mixed_70_20_10_hdr`)
  and, if so, which core and whether it was isolated.
- Hyperthreads, `nohz_full`, `rcu_nocbs`, SMT state.
- Warmup ops, measured ops, workload seed (fixed per the template).
- Rust version, `--release`, LTO setting, `RUSTFLAGS`.
- Allocator — system allocator unless the crate has been configured otherwise.

### 8. `BENCH.md` template block

```markdown
## mixed_70_20_10

Workload: 70% submits, 20% cancels, 10% aggressive market (IOC-like). Seed 0xA5A5A5A5.
Samples: 1,000,000 after 200,000 warmup ops.
Loop: closed-loop. Reported numbers are pure service time; see Methodology §CO.

| Quantile  | Latency     |
|-----------|-------------|
| p50       | XXX ns      |
| p99       | XXX ns      |
| p99.9     | XXX ns      |
| p99.99    | XXX ns      |

**Where the tail comes from.**
[One honest paragraph. Acceptable content: cache miss on `SkipMap` price lookup beyond L2,
branch mispredict on the fill loop when the book is thin, allocator jitter from
`BookChangeEvent` emission when the outbound `Vec` resizes, `DashMap` shard contention on
the order-id index under concurrent writers. Do not write "probably jitter" — if you
don't know, say "the dominant contributor is not yet identified; next step is
`perf stat -e <events>` on the measured window."]
```

### 9. After writing

- `cargo bench --bench <scenario>_hdr`.
- Fill the `BENCH.md` table with the output.
- `target/bench-hdr/*.hgrm` is already under `target/` so it is gitignored; do not commit
  the histograms. Commit `BENCH.md`.
- Commit with a conventional prefix: `bench: add <scenario> HDR histogram bench`.

### 10. Relationship to existing Criterion benches

The Criterion benches under `benches/order_book/` (`add_orders.rs`, `match_orders.rs`,
`mass_cancel.rs`, `matching.rs`, `mixed_operations.rs`, `replay.rs`, `snapshot.rs`,
`update_orders.rs`) stay as they are — they provide the mean-centric statistical
comparison that Criterion does well and publish HTML reports to `target/criterion/`.

The `_hdr` benches coexist with them and are the source of truth for tail-latency claims
in `BENCH.md` and any release notes that quote p99 / p99.9 / p99.99 numbers.
