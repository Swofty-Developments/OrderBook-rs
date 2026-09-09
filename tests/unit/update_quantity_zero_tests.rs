//! `UpdateQuantity` with a zero total quantity cancels the order.
//!
//! A zero-quantity maker cannot fill, so resting one published a price
//! level with no depth: it held `best_bid` / `best_ask`, made
//! `will_cross_market` reject a post-only at that price, and was later
//! dropped by a sweep with no trade and no cancel event — leaving the
//! `order_locations` entry behind, so `cancel_order` returned `Ok(None)`
//! and re-adding the id reported `DuplicateOrderId`.

#[cfg(test)]
mod tests_update_quantity_zero {
    use orderbook_rs::orderbook::order_state::{CancelReason, OrderStateTracker, OrderStatus};
    use orderbook_rs::{DefaultOrderBook, OrderBook, OrderBookError};
    use pricelevel::{Id, OrderUpdate, Quantity, Side, TimeInForce};

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
        assert!(book.get_asks().is_empty(), "the empty level was removed");
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
}
