//! `UpdateQuantity` with a zero `new_quantity` cancels the entire order.
//!
//! A zero-quantity maker cannot fill, so resting one published a price
//! level with no depth: it held `best_bid` / `best_ask`, made
//! `will_cross_market` reject a post-only at that price, and was later
//! dropped by a sweep with no trade and no cancel event — leaving the
//! `order_locations` entry behind, so `cancel_order` returned `Ok(None)`
//! and re-adding the id reported `DuplicateOrderId`.
//!
//! Zero is a removal, not a resize: it bypasses the projected-order
//! validator (a configured `min_order_size` does not veto it) and cancels
//! a two-tranche order whole, hidden depth included.

#[cfg(test)]
mod tests_update_quantity_zero {
    use orderbook_rs::orderbook::order_state::{CancelReason, OrderStateTracker, OrderStatus};
    use orderbook_rs::{DefaultOrderBook, OrderBook, OrderBookError};
    use pricelevel::{
        Hash32, Id, OrderType, OrderUpdate, Price, Quantity, Side, TimeInForce, TimestampMs,
    };
    use std::num::NonZeroU64;

    const PRICE: u128 = 100;
    const MAKER: u64 = 1;

    fn book_with_resting_ask(symbol: &str) -> OrderBook<()> {
        let mut book: OrderBook<()> = DefaultOrderBook::new(symbol);
        book.set_order_state_tracker(OrderStateTracker::new());
        book.add_limit_order(
            Id::from_u64(MAKER),
            PRICE,
            10,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("seed ask");
        book
    }

    fn update_to_zero(book: &OrderBook<()>) -> Option<std::sync::Arc<pricelevel::OrderType<()>>> {
        book.update_order(OrderUpdate::UpdateQuantity {
            order_id: Id::from_u64(MAKER),
            new_quantity: Quantity::new(0),
        })
        .expect("update to zero succeeds")
    }

    /// The order leaves the book with a terminal cancel, and the level it
    /// was alone on is removed with it.
    #[test]
    fn update_to_zero_cancels_the_order_and_removes_the_level() {
        let book = book_with_resting_ask("UQZ1");

        let removed = update_to_zero(&book).expect("the cancelled order is returned");
        assert_eq!(removed.id(), Id::from_u64(MAKER));

        assert!(
            book.get_order(Id::from_u64(MAKER)).is_none(),
            "no ghost order"
        );
        assert_eq!(book.best_ask(), None, "no phantom level at zero depth");
        assert!(
            book.create_snapshot(usize::MAX).asks.is_empty(),
            "the empty level was removed"
        );
        assert_eq!(
            book.order_status(Id::from_u64(MAKER)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::UserRequested,
            }),
            "a cancel event is recorded"
        );
    }

    /// The id is free again: the location entry no longer leaks.
    #[test]
    fn cancelled_id_is_reusable() {
        let book = book_with_resting_ask("UQZ2");
        update_to_zero(&book);

        assert!(
            book.cancel_order(Id::from_u64(MAKER))
                .expect("cancel of an absent order is not an error")
                .is_none(),
            "the order is already gone"
        );
        book.add_limit_order(
            Id::from_u64(MAKER),
            PRICE,
            4,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("the id is reusable");
        assert_eq!(book.best_ask(), Some(PRICE));
    }

    /// A post-only order at the vacated price no longer crosses nothing.
    #[test]
    fn post_only_at_the_vacated_price_is_admitted() {
        let book = book_with_resting_ask("UQZ3");
        update_to_zero(&book);

        book.add_post_only_order(Id::from_u64(2), PRICE, 5, Side::Buy, TimeInForce::Gtc, None)
            .expect("nothing to cross at the vacated price");
        assert_eq!(book.best_bid(), Some(PRICE));
    }

    /// The removal is atomic with the rest of the level: only the zeroed
    /// order goes.
    #[test]
    fn a_shared_level_keeps_its_other_makers() {
        let book = book_with_resting_ask("UQZ4");
        book.add_limit_order(
            Id::from_u64(2),
            PRICE,
            7,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("seed second maker");

        update_to_zero(&book);

        assert_eq!(book.best_ask(), Some(PRICE), "the level still has depth");
        assert_eq!(
            book.get_order(Id::from_u64(2))
                .expect("the other maker rests")
                .visible_quantity()
                .as_u64(),
            7
        );
    }

    /// Zeroing an absent order stays `Ok(None)`, as every other update
    /// variant reports a missing order.
    #[test]
    fn update_to_zero_on_an_absent_order_is_none() {
        let book = book_with_resting_ask("UQZ5");

        let result = book
            .update_order(OrderUpdate::UpdateQuantity {
                order_id: Id::from_u64(99),
                new_quantity: Quantity::new(0),
            })
            .expect("absent order is not an error");
        assert!(result.is_none());
        assert_eq!(book.best_ask(), Some(PRICE), "the real maker is untouched");
    }

    /// The kill switch still gates it: this is a modify, not a cancel.
    #[test]
    fn update_to_zero_is_rejected_while_the_kill_switch_is_engaged() {
        let book = book_with_resting_ask("UQZ6");
        book.engage_kill_switch();

        let err = book
            .update_order(OrderUpdate::UpdateQuantity {
                order_id: Id::from_u64(MAKER),
                new_quantity: Quantity::new(0),
            })
            .expect_err("modifications are halted");
        assert!(
            matches!(err, OrderBookError::KillSwitchActive),
            "expected KillSwitchActive, got {err:?}"
        );
        assert!(
            book.get_order(Id::from_u64(MAKER)).is_some(),
            "the maker survives a rejected modify"
        );
    }

    fn assert_cancelled_whole(book: &OrderBook<()>) {
        assert!(
            book.get_order(Id::from_u64(MAKER)).is_none(),
            "the whole order is gone, hidden depth included"
        );
        assert_eq!(book.best_ask(), None, "no level survives on hidden depth");
        assert!(
            book.create_snapshot(usize::MAX).asks.is_empty(),
            "the empty level was removed"
        );
        assert_eq!(
            book.order_status(Id::from_u64(MAKER)),
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::UserRequested,
            }),
            "a cancel event is recorded"
        );
    }

    /// `new_quantity` is the visible tranche for a two-tranche order, so a
    /// zero visible quantity says nothing about the total. Zero still
    /// cancels the entire order: it is a removal, not a resize, and the
    /// hidden depth goes with it rather than surviving as a 0-visible
    /// ghost pinning `best_ask`.
    #[test]
    fn iceberg_zeroed_with_hidden_depth_is_cancelled_whole() {
        let mut book: OrderBook<()> = DefaultOrderBook::new("UQZ7");
        book.set_order_state_tracker(OrderStateTracker::new());
        book.add_iceberg_order(
            Id::from_u64(MAKER),
            PRICE,
            10,
            50,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("seed iceberg");
        assert_eq!(
            book.get_order(Id::from_u64(MAKER))
                .expect("iceberg rests")
                .hidden_quantity()
                .as_u64(),
            50,
            "hidden depth is resting before the update"
        );

        let removed = update_to_zero(&book).expect("the cancelled order is returned");
        assert_eq!(removed.id(), Id::from_u64(MAKER));
        assert_eq!(
            removed.hidden_quantity().as_u64(),
            50,
            "the order was removed intact, not resized to zero first"
        );
        assert_cancelled_whole(&book);
    }

    /// Same contract for a reserve order, whose hidden tranche is otherwise
    /// drawn down by replenishment rather than by `UpdateQuantity`.
    #[test]
    fn reserve_zeroed_with_hidden_depth_is_cancelled_whole() {
        let mut book: OrderBook<()> = DefaultOrderBook::new("UQZ8");
        book.set_order_state_tracker(OrderStateTracker::new());
        book.add_order(OrderType::ReserveOrder {
            id: Id::from_u64(MAKER),
            price: Price::new(PRICE),
            visible_quantity: Quantity::new(10),
            hidden_quantity: Quantity::new(40),
            side: Side::Sell,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            replenish_threshold: Quantity::new(5),
            replenish_amount: Some(NonZeroU64::new(10).expect("nonzero")),
            auto_replenish: true,
            extra_fields: (),
        })
        .expect("seed reserve order");

        let removed = update_to_zero(&book).expect("the cancelled order is returned");
        assert_eq!(removed.id(), Id::from_u64(MAKER));
        assert_eq!(
            removed.hidden_quantity().as_u64(),
            40,
            "the order was removed intact, not resized to zero first"
        );
        assert_cancelled_whole(&book);
    }

    /// A size floor has no say over a removal: the zero branch runs before
    /// the projected-order validator, so a configured `min_order_size`
    /// cancels the order exactly as it would without one. The validator
    /// still guards every nonzero resize on the same book.
    #[test]
    fn min_order_size_does_not_veto_the_zero_update() {
        let mut book: OrderBook<()> = DefaultOrderBook::new("UQZ9");
        book.set_order_state_tracker(OrderStateTracker::new());
        book.set_min_order_size(5);
        book.add_limit_order(
            Id::from_u64(MAKER),
            PRICE,
            10,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("seed ask");

        let err = book
            .update_order(OrderUpdate::UpdateQuantity {
                order_id: Id::from_u64(MAKER),
                new_quantity: Quantity::new(3),
            })
            .expect_err("a nonzero resize below the floor is validated");
        assert!(
            matches!(err, OrderBookError::OrderSizeOutOfRange { .. }),
            "expected OrderSizeOutOfRange, got {err:?}"
        );
        assert_eq!(
            book.get_order(Id::from_u64(MAKER))
                .expect("a rejected resize leaves the maker untouched")
                .visible_quantity()
                .as_u64(),
            10
        );

        let removed = update_to_zero(&book).expect("zero is a removal, not a resize");
        assert_eq!(removed.id(), Id::from_u64(MAKER));
        assert_cancelled_whole(&book);
    }
}
