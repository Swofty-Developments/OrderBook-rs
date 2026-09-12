// reserve_sweep_hdr: IOC probes into a reserve-maker book, comparing
// strandable (non-auto-replenishing) vs auto-replenishing hidden depth
// (#230).
//
// #230 adds `capture_strandable_makers` to `match_order_inner`
// (`src/orderbook/matching.rs`). Each sweep reads `non_auto_reserve_rested`
// (a monotonic per-book flag set at admission time) exactly once, before
// any level is touched. On a book that has never rested a
// non-auto-replenishing `ReserveOrder` with hidden depth, that single
// relaxed atomic load is the sweep's entire cost: no pool buffer is
// acquired, `capture_strandable_makers` is never called for any level, and
// the post-sweep drain does no lookup. Only when the flag is true does
// each matched level get checked (`hidden_quantity() > 0`) and, if it
// still holds hidden depth, walked with `PriceLevel::iter_orders()` to
// record which resting non-auto reserves have hidden quantity behind
// them. That capture is what lets the engine report the hidden depth a
// sweep strands when `pricelevel` drops a depleted maker's hidden tranche
// instead of refreshing from it: an `INFO` trace plus the
// `orderbook_reserve_discards_total` / `orderbook_reserve_hidden_discarded_total`
// metrics. `iter_orders` is `DashMap::iter` upstream, which read-locks
// every shard of the map regardless of how few orders rest at the level,
// so the walk is not free on any level holding hidden depth. This bench
// measures that path; run it on `main` and on this branch and diff.
//
// Workload: the shape of `thin_book_sweep_hdr` (same refill cadence and
// probe distribution, only the resting side is reserve makers). Refill
// `RESTING_PER_REFILL` resting asks every `REFILL_EVERY` ops (not timed),
// then time one IOC buy probe against them. Two scenarios, run back to
// back on fresh books:
//
// - `reserve_sweep_nonauto` (`auto_replenish: false`): every resting
//   maker is strandable, `non_auto_reserve_rested` flips true on the
//   first rest, and every level-match that still holds hidden depth pays
//   the `iter_orders()` walk plus (on a fully-consumed maker) the discard
//   report. This is the cost `capture_strandable_makers` adds.
// - `reserve_sweep_auto` (`auto_replenish: true`): hidden depth still
//   rests and is still consumed by sweeps, but nothing is strandable, so
//   this book never sets `non_auto_reserve_rested`. Each sweep pays the
//   one hoisted atomic load and nothing else; `capture_strandable_makers`
//   is never called. Must cost the same as `main`, where none of this
//   exists at all.

#[path = "hdr_common.rs"]
mod common;

use common::{Rng, new_histogram, owner, persist, record, report};
use hdrhistogram::Histogram;
use pricelevel::{Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};

// Re-seed a thin slice of reserve makers every REFILL_EVERY ops so the
// book never goes fully empty across the measurement window, mirroring
// `thin_book_sweep_hdr`.
const RESTING_PER_REFILL: u64 = 3;
const REFILL_EVERY: u64 = 5;
const MEASURED_OPS: u64 = 200_000;
const SEED: u64 = 0xA5A5_A5A5_A5A5_A5A5;

const SCENARIOS: [(&str, bool); 2] = [
    ("reserve_sweep_nonauto", false),
    ("reserve_sweep_auto", true),
];

/// Run one scenario on a fresh book: refill `RESTING_PER_REFILL` resting
/// `ReserveOrder` asks (`auto_replenish` as given) every `REFILL_EVERY`
/// ops (not timed), then time an IOC buy probe against the book. Returns
/// the probe-latency histogram.
fn run_scenario(auto_replenish: bool) -> Histogram<u64> {
    let book = common::fresh_book();
    let mut rng = Rng::new(SEED);
    let mut hist = new_histogram();
    let maker = owner(0xAA);
    let taker = owner(0xBB);
    let mut next_id: u64 = 1;

    for i in 0..MEASURED_OPS {
        if i % REFILL_EVERY == 0 {
            // Drop a few resting reserve asks. No measurement around the
            // refill; only the IOC probe below is timed.
            for _ in 0..RESTING_PER_REFILL {
                let _ = book.add_order(OrderType::ReserveOrder {
                    id: Id::from_u64(next_id),
                    price: Price::new(rng.range(99, 101) as u128),
                    visible_quantity: Quantity::new(rng.range(1, 5)),
                    hidden_quantity: Quantity::new(rng.range(4, 12)),
                    side: Side::Sell,
                    user_id: maker,
                    timestamp: TimestampMs::new(0),
                    time_in_force: TimeInForce::Gtc,
                    replenish_threshold: Quantity::new(0),
                    replenish_amount: None,
                    auto_replenish,
                    extra_fields: (),
                });
                next_id += 1;
            }
        }

        // IOC buy probe, frequently larger than the resting visible
        // tranche, so the engine partial-fills and, on the nonauto
        // scenario, strands (and reports) hidden depth.
        let id = Id::from_u64(next_id);
        next_id += 1;
        let qty = rng.range(1, 20);
        record(&mut hist, || {
            let _ = book.submit_market_order_with_user(id, qty, Side::Buy, taker);
        });
    }

    hist
}

fn main() {
    for (scenario, auto_replenish) in SCENARIOS {
        let hist = run_scenario(auto_replenish);
        report(scenario, &hist);
        persist(scenario, &hist).expect("persist hgrm");
    }
}
