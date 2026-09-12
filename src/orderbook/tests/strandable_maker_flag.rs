//! #230: `OrderBook::non_auto_reserve_rested`, the gate on the sweep's
//! strandable-maker scan.
//!
//! The scan that captures makers whose hidden depth a sweep would strand has
//! to walk a level's resting orders, and `PriceLevel::iter_orders` is a
//! `DashMap` iterator that read-locks every shard of the map per level match.
//! Only a `ReserveOrder { auto_replenish: false, .. }` carrying hidden
//! quantity can ever be captured, so the book records whether it has rested
//! one and the sweep skips the scan entirely otherwise.
//!
//! The flag is monotonic — set, never cleared — so these tests pin what sets
//! it, not what leaves it alone after a removal.

#[cfg(test)]
mod tests {
    use crate::orderbook::book::OrderBook;
    use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
    use std::num::NonZeroU64;
    use std::sync::atomic::Ordering;

    const PRICE: u128 = 100;

    /// Read the gate.
    fn flag(book: &OrderBook<()>) -> bool {
        book.non_auto_reserve_rested.load(Ordering::Relaxed)
    }

    /// A reserve BUY at `PRICE` with the given tranches and replenishment
    /// policy.
    fn reserve_buy(
        id: Id,
        visible: u64,
        hidden: u64,
        replenish_amount: Option<u64>,
        auto_replenish: bool,
    ) -> OrderType<()> {
        OrderType::ReserveOrder {
            id,
            price: Price::new(PRICE),
            visible_quantity: Quantity::new(visible),
            hidden_quantity: Quantity::new(hidden),
            side: Side::Buy,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            replenish_threshold: Quantity::new(0),
            replenish_amount: replenish_amount.and_then(NonZeroU64::new),
            auto_replenish,
            extra_fields: (),
        }
    }

    /// An iceberg BUY at `PRICE`.
    fn iceberg_buy(id: Id, visible: u64, hidden: u64) -> OrderType<()> {
        OrderType::IcebergOrder {
            id,
            price: Price::new(PRICE),
            visible_quantity: Quantity::new(visible),
            hidden_quantity: Quantity::new(hidden),
            side: Side::Buy,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            extra_fields: (),
        }
    }

    /// A fresh book has nothing to scan for.
    #[test]
    fn test_non_auto_reserve_rested_starts_false() {
        let book: OrderBook<()> = OrderBook::new("FLAG-NEW");
        assert!(!flag(&book), "a fresh book has rested nothing");
    }

    /// Icebergs and auto-replenishing reserves both carry hidden depth, so
    /// they make levels the scan would have to walk — but neither can ever
    /// strand anything, so neither arms the gate, not even across a sweep
    /// that consumes them.
    #[test]
    fn test_non_auto_reserve_rested_stays_false_for_iceberg_and_auto_reserve() {
        let book: OrderBook<()> = OrderBook::new("FLAG-SAFE");
        let iceberg_id = Id::new();
        let auto_id = Id::new();

        assert!(
            book.add_order(iceberg_buy(iceberg_id, 10, 20)).is_ok(),
            "iceberg must rest"
        );
        assert!(
            book.add_order(reserve_buy(auto_id, 10, 20, Some(10), true))
                .is_ok(),
            "auto-replenishing reserve must rest"
        );
        assert!(!flag(&book), "neither kind can strand hidden depth");

        // A sweep across both leaves the gate closed as well.
        assert!(
            book.add_limit_order(Id::new(), PRICE, 15, Side::Sell, TimeInForce::Gtc, None)
                .is_ok(),
            "the crossing sell must be accepted"
        );
        assert!(!flag(&book), "matching does not arm the gate by itself");
    }

    /// Resting a non-auto reserve with hidden depth arms the gate.
    #[test]
    fn test_non_auto_reserve_rested_set_by_resting_non_auto_reserve() {
        let book: OrderBook<()> = OrderBook::new("FLAG-ARM");
        assert!(
            book.add_order(reserve_buy(Id::new(), 10, 20, None, false))
                .is_ok(),
            "the reserve must rest"
        );
        assert!(flag(&book), "a strandable maker is now on the book");
    }

    /// Without hidden depth there is nothing to strand, so the same
    /// non-auto reserve leaves the gate closed.
    #[test]
    fn test_non_auto_reserve_rested_stays_false_without_hidden_depth() {
        let book: OrderBook<()> = OrderBook::new("FLAG-NO-HIDDEN");
        assert!(
            book.add_order(reserve_buy(Id::new(), 10, 0, None, false))
                .is_ok(),
            "the reserve must rest"
        );
        assert!(!flag(&book), "no hidden tranche, nothing to strand");
    }

    /// The residual-resting path arms the gate too: a partially filled
    /// non-auto reserve that keeps a positive visible tranche rests with its
    /// hidden depth intact and can strand it later.
    #[test]
    fn test_non_auto_reserve_rested_set_by_rested_residual() {
        let book: OrderBook<()> = OrderBook::new("FLAG-RESIDUAL");
        assert!(
            book.add_limit_order(Id::new(), PRICE, 5, Side::Sell, TimeInForce::Gtc, None)
                .is_ok(),
            "contra depth must rest"
        );
        let taker_id = Id::new();
        assert!(
            book.add_order(reserve_buy(taker_id, 10, 20, None, false))
                .is_ok(),
            "the aggressive reserve must rest its residual"
        );
        assert!(
            book.get_order(taker_id).is_some(),
            "5 of the visible tranche was taken, so the residual rests"
        );
        assert!(flag(&book), "the rested residual is strandable");
    }

    /// The flag is not part of the snapshot format; a restore re-derives it
    /// from the orders it installs, so a package carrying a strandable maker
    /// arms the gate on the restored book.
    #[test]
    fn test_non_auto_reserve_rested_rederived_by_snapshot_package_restore() {
        let source: OrderBook<()> = OrderBook::new("FLAG-RESTORE");
        let order_id = Id::new();
        assert!(
            source
                .add_order(reserve_buy(order_id, 10, 20, None, false))
                .is_ok(),
            "the reserve must rest on the source book"
        );
        assert!(flag(&source), "the source book is armed");
        let package = match source.create_snapshot_package(usize::MAX) {
            Ok(package) => package,
            Err(error) => panic!("snapshot package must build: {error}"),
        };

        let mut restored: OrderBook<()> = OrderBook::new("FLAG-RESTORE");
        assert!(!flag(&restored), "the destination starts closed");
        assert!(
            restored.restore_from_snapshot_package(package).is_ok(),
            "the package must restore"
        );

        assert!(
            restored.get_order(order_id).is_some(),
            "the strandable maker is on the restored book"
        );
        assert!(
            flag(&restored),
            "restore must re-derive the gate from the installed orders"
        );
    }

    /// A package with no strandable maker leaves the gate closed, so the
    /// re-derivation is not a blanket `true` on every restore.
    #[test]
    fn test_non_auto_reserve_rested_stays_false_restoring_a_safe_package() {
        let source: OrderBook<()> = OrderBook::new("FLAG-RESTORE-SAFE");
        assert!(
            source.add_order(iceberg_buy(Id::new(), 10, 20)).is_ok(),
            "the iceberg must rest on the source book"
        );
        let package = match source.create_snapshot_package(usize::MAX) {
            Ok(package) => package,
            Err(error) => panic!("snapshot package must build: {error}"),
        };

        let mut restored: OrderBook<()> = OrderBook::new("FLAG-RESTORE-SAFE");
        assert!(
            restored.restore_from_snapshot_package(package).is_ok(),
            "the package must restore"
        );
        assert!(!flag(&restored), "nothing installed can strand anything");
    }

    /// The gate only controls *reporting*, never matching: a book whose
    /// flag was never armed still discards a non-auto reserve maker's
    /// hidden tranche exactly as before. (It cannot actually happen — the
    /// maker could not be resting without arming the gate — so this pins
    /// that the two concerns stay separate.)
    #[test]
    fn test_strandable_scan_gate_does_not_change_matching_outcome() {
        let book: OrderBook<()> = OrderBook::new("FLAG-SEMANTICS");
        let maker_id = Id::new();
        assert!(
            book.add_order(reserve_buy(maker_id, 10, 20, None, false))
                .is_ok(),
            "the maker must rest"
        );
        // Close the gate behind the maker's back, then sweep it.
        book.non_auto_reserve_rested.store(false, Ordering::Relaxed);
        assert!(
            book.add_limit_order(Id::new(), PRICE, 10, Side::Sell, TimeInForce::Gtc, None)
                .is_ok(),
            "the crossing sell must be accepted"
        );

        assert!(
            book.get_order(maker_id).is_none(),
            "the depleted non-replenishing maker leaves the book either way"
        );
        assert!(
            book.best_bid().is_none(),
            "the emptied level is removed either way"
        );
    }
}
