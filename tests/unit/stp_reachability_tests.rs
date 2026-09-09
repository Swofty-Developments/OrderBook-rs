//! STP fires only on a same-user maker the taker can actually reach.
//!
//! `check_stp_at_level` reports a conflict whenever a same-user maker rests
//! at a crossed level, but the taker only self-trades if it still has
//! quantity left after consuming the non-self depth in front of that maker.
//! The `CancelTaker` / `CancelBoth` arms are entered only when that residual
//! is non-zero — the same reachability rule `check_modify_stp_self_cross`
//! already applies on the modify path.

#[cfg(test)]
mod tests_stp_reachability {
    use orderbook_rs::orderbook::order_state::{CancelReason, OrderStateTracker, OrderStatus};
    use orderbook_rs::orderbook::stp::STPMode;
    use orderbook_rs::{DefaultOrderBook, OrderBook, OrderBookError, TradeResult};
    use pricelevel::{Hash32, Id, Side, TimeInForce};

    const PRICE: u128 = 100;
    /// Ahead in the queue and owned by someone else: reachable depth.
    const OTHER_MAKER: u64 = 1;
    /// Behind it and owned by the taker: the STP trigger.
    const SELF_MAKER: u64 = 2;
    const TAKER: u64 = 3;

    fn user(byte: u8) -> Hash32 {
        Hash32::new([byte; 32])
    }

    /// Ask queue at 100: 5 lots from user 2, then 9 lots from user 1.
    fn book_with_self_maker_behind(mode: STPMode) -> OrderBook<()> {
        let mut book: OrderBook<()> = DefaultOrderBook::new("STPR");
        book.set_stp_mode(mode);
        book.set_order_state_tracker(OrderStateTracker::new());
        book.add_limit_order_with_user(
            Id::from_u64(OTHER_MAKER),
            PRICE,
            5,
            Side::Sell,
            TimeInForce::Gtc,
            user(2),
            None,
        )
        .expect("seed non-self maker");
        book.add_limit_order_with_user(
            Id::from_u64(SELF_MAKER),
            PRICE,
            9,
            Side::Sell,
            TimeInForce::Gtc,
            user(1),
            None,
        )
        .expect("seed same-user maker");
        book
    }

    fn filled(result: &Option<TradeResult>) -> u64 {
        result
            .as_ref()
            .map(|tr| {
                tr.match_result
                    .trades()
                    .as_vec()
                    .iter()
                    .map(|t| t.quantity().as_u64())
                    .fold(0u64, u64::saturating_add)
            })
            .unwrap_or(0)
    }

    fn assert_self_maker_intact(book: &OrderBook<()>) {
        let maker = book
            .get_order(Id::from_u64(SELF_MAKER))
            .expect("same-user maker still rests");
        assert_eq!(
            maker.visible_quantity().as_u64(),
            9,
            "an unreachable maker is neither filled nor cancelled"
        );
        assert!(
            !matches!(
                book.order_status(Id::from_u64(SELF_MAKER)),
                Some(OrderStatus::Cancelled { .. })
            ),
            "no cancel recorded for an unreachable maker"
        );
    }

    /// Buy 3 against 5 non-self lots: the taker is satisfied before the
    /// same-user maker, so it fills and rests nothing.
    #[test]
    fn cancel_taker_does_not_fire_on_unreachable_maker() {
        let book = book_with_self_maker_behind(STPMode::CancelTaker);

        let (_, trades) = book
            .add_limit_order_with_user_and_result(
                Id::from_u64(TAKER),
                PRICE,
                3,
                Side::Buy,
                TimeInForce::Gtc,
                user(1),
                None,
            )
            .expect("taker never reaches the same-user maker");

        assert_eq!(filled(&trades), 3, "filled against the non-self maker");
        assert_self_maker_intact(&book);
    }

    /// Same reachability rule under `CancelBoth`, which additionally
    /// destroyed the untouched maker.
    #[test]
    fn cancel_both_does_not_fire_on_unreachable_maker() {
        let book = book_with_self_maker_behind(STPMode::CancelBoth);

        let (_, trades) = book
            .add_limit_order_with_user_and_result(
                Id::from_u64(TAKER),
                PRICE,
                3,
                Side::Buy,
                TimeInForce::Gtc,
                user(1),
                None,
            )
            .expect("taker never reaches the same-user maker");

        assert_eq!(filled(&trades), 3, "filled against the non-self maker");
        assert_self_maker_intact(&book);
    }

    /// The boundary: a taker consuming exactly the non-self depth stops one
    /// unit short of the same-user maker.
    #[test]
    fn exact_non_self_depth_fills_without_stp() {
        for mode in [STPMode::CancelTaker, STPMode::CancelBoth] {
            let book = book_with_self_maker_behind(mode);

            let (_, trades) = book
                .add_limit_order_with_user_and_result(
                    Id::from_u64(TAKER),
                    PRICE,
                    5,
                    Side::Buy,
                    TimeInForce::Gtc,
                    user(1),
                    None,
                )
                .unwrap_or_else(|e| panic!("{mode}: exact-depth taker must fill, got {e:?}"));

            assert_eq!(
                filled(&trades),
                5,
                "{mode}: consumed the whole non-self maker"
            );
            assert_self_maker_intact(&book);
            assert!(
                book.get_order(Id::from_u64(OTHER_MAKER)).is_none(),
                "{mode}: the non-self maker was fully consumed"
            );
        }
    }

    /// A fill-or-kill taker covered exactly by the non-self depth is
    /// feasible and executes — the feasibility check and the sweep agree.
    #[test]
    fn fok_covered_by_non_self_depth_executes() {
        let book = book_with_self_maker_behind(STPMode::CancelBoth);

        let (_, trades) = book
            .add_limit_order_with_user_and_result(
                Id::from_u64(TAKER),
                PRICE,
                5,
                Side::Buy,
                TimeInForce::Fok,
                user(1),
                None,
            )
            .expect("feasible FOK must not be killed after executing");

        assert_eq!(filled(&trades), 5, "FOK filled its complete quantity");
        assert_self_maker_intact(&book);
    }

    /// A market taker takes the same path with the STP flag dropped, so
    /// `CancelBoth` cancelled the maker while returning `Ok`.
    #[test]
    fn market_cancel_both_leaves_unreachable_maker_intact() {
        let book = book_with_self_maker_behind(STPMode::CancelBoth);

        let result = book
            .submit_market_order_with_user(Id::from_u64(TAKER), 3, Side::Buy, user(1))
            .expect("market taker never reaches the same-user maker");

        let executed: u64 = result
            .trades()
            .as_vec()
            .iter()
            .map(|t| t.quantity().as_u64())
            .sum();
        assert_eq!(executed, 3, "filled against the non-self maker");
        assert_self_maker_intact(&book);
    }

    /// Unchanged behaviour: a taker with quantity left over after the
    /// non-self depth does reach the same-user maker and is cancelled.
    #[test]
    fn reachable_self_maker_reports_self_trade_prevented() {
        for mode in [STPMode::CancelTaker, STPMode::CancelBoth] {
            let book = book_with_self_maker_behind(mode);

            let err = book
                .add_limit_order_with_user(
                    Id::from_u64(TAKER),
                    PRICE,
                    7,
                    Side::Buy,
                    TimeInForce::Gtc,
                    user(1),
                    None,
                )
                .expect_err("reachable self-trade is prevented");
            assert!(
                matches!(err, OrderBookError::SelfTradePrevented { .. }),
                "{mode}: expected SelfTradePrevented, got {err:?}"
            );
            assert_eq!(
                book.order_status(Id::from_u64(TAKER)),
                Some(OrderStatus::Cancelled {
                    filled_quantity: 5,
                    reason: CancelReason::SelfTradePrevention,
                }),
                "{mode}: taker cancelled with its true non-self fill"
            );

            match mode {
                STPMode::CancelBoth => assert!(
                    book.get_order(Id::from_u64(SELF_MAKER)).is_none(),
                    "CancelBoth cancels the reached maker"
                ),
                _ => assert!(
                    book.get_order(Id::from_u64(SELF_MAKER)).is_some(),
                    "CancelTaker leaves the maker resting"
                ),
            }
        }
    }
}
