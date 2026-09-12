//! STP fires only on a same-user maker the taker can actually reach.
//!
//! `check_stp_at_level` reports a conflict whenever a same-user maker rests
//! at a crossed level, but the taker only self-trades if it can still
//! execute at that price after consuming the non-self depth in front of
//! that maker. A spent budget is a complete fill, and quote-notional dust
//! below one unit leaves the maker untouched and walks on to the next
//! level, where a cheaper bid may still be affordable. A base-quantity
//! residual keeps the STP verdict whatever its size, since walked past it
//! would rest crossed against the taker's own maker.
//! `check_modify_stp_self_cross` applies the same per-level FIFO rule and
//! the same lot-rounded per-level cap on the modify path.

#[cfg(test)]
mod tests_stp_reachability {
    use orderbook_rs::orderbook::order_state::{CancelReason, OrderStateTracker, OrderStatus};
    use orderbook_rs::orderbook::stp::STPMode;
    use orderbook_rs::{DefaultOrderBook, OrderBook, OrderBookError, TradeResult};
    use pricelevel::{
        Hash32, Id, MatchResult, OrderType, OrderUpdate, Price, Quantity, Side, TimeInForce,
        TimestampMs,
    };
    use std::num::NonZeroU64;

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

    fn executed(result: &MatchResult) -> u64 {
        result
            .trades()
            .as_vec()
            .iter()
            .map(|t| t.quantity().as_u64())
            .fold(0u64, u64::saturating_add)
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

    /// A market taker takes the same path with the STP flag dropped, so an
    /// unreachable maker used to be cancelled under `CancelBoth` while the
    /// caller saw `Ok`. The maker must survive untouched.
    #[test]
    fn market_cancel_both_leaves_unreachable_maker_intact() {
        let book = book_with_self_maker_behind(STPMode::CancelBoth);

        let result = book
            .submit_market_order_with_user(Id::from_u64(TAKER), 3, Side::Buy, user(1))
            .expect("market taker never reaches the same-user maker");

        assert_eq!(executed(&result), 3, "filled against the non-self maker");
        assert_self_maker_intact(&book);
    }

    /// The quote-amount twin: a notional budget normally ends in dust below
    /// one unit, never at exactly zero, so an exact-zero guard alone still
    /// cancelled the untouched maker (and, under `CancelBoth`, returned
    /// `Ok` while doing it). 350 at 100 buys 3 lots and leaves 50 — not
    /// enough for a fourth unit, so the same-user maker is never reached.
    #[test]
    fn quote_amount_buy_dust_leaves_unreachable_maker_intact() {
        for mode in [STPMode::CancelTaker, STPMode::CancelBoth] {
            let book = book_with_self_maker_behind(mode);

            let result = book
                .submit_market_order_by_amount_with_user(
                    Id::from_u64(TAKER),
                    350,
                    Side::Buy,
                    user(1),
                )
                .unwrap_or_else(|e| panic!("{mode}: dust never reaches the maker, got {e:?}"));

            assert_eq!(
                executed(&result),
                3,
                "{mode}: filled against the non-self maker"
            );
            assert_self_maker_intact(&book);
        }
    }

    /// Unchanged behaviour on the quote-amount path: a budget that can still
    /// fund a whole unit at the level does reach the same-user maker.
    #[test]
    fn quote_amount_buy_with_a_reachable_maker_still_fires() {
        for mode in [STPMode::CancelTaker, STPMode::CancelBoth] {
            let book = book_with_self_maker_behind(mode);

            // 600 buys the 5 non-self lots and still funds one more unit.
            let result = book
                .submit_market_order_by_amount_with_user(
                    Id::from_u64(TAKER),
                    600,
                    Side::Buy,
                    user(1),
                )
                .unwrap_or_else(|e| panic!("{mode}: non-self fills make this Ok, got {e:?}"));

            assert_eq!(
                executed(&result),
                5,
                "{mode}: the non-self depth ahead of the maker was consumed"
            );
            match mode {
                STPMode::CancelBoth => {
                    assert!(
                        book.get_order(Id::from_u64(SELF_MAKER)).is_none(),
                        "CancelBoth cancels the reached maker"
                    );
                    assert_eq!(
                        book.order_status(Id::from_u64(SELF_MAKER)),
                        Some(OrderStatus::Cancelled {
                            filled_quantity: 0,
                            reason: CancelReason::SelfTradePrevention,
                        }),
                        "the reached maker is cancelled by STP"
                    );
                }
                _ => assert_self_maker_intact(&book),
            }
        }
    }

    /// Dust at one price is not a dead budget. A quote-amount sell that
    /// cannot afford another unit at 100 can still afford one at 50, so the
    /// sweep must preserve the same-user maker at 100 and walk on rather
    /// than stop at the level it cannot execute on.
    #[test]
    fn quote_amount_sell_walks_past_a_level_it_cannot_afford() {
        const OTHER_AT_100: u64 = 11;
        const SELF_AT_100: u64 = 12;
        const OTHER_AT_50: u64 = 13;

        for mode in [STPMode::CancelTaker, STPMode::CancelBoth] {
            let mut book: OrderBook<()> = DefaultOrderBook::new("STPQ");
            book.set_stp_mode(mode);
            book.set_order_state_tracker(OrderStateTracker::new());
            for (id, price, owner) in [
                (OTHER_AT_100, 100, user(2)),
                (SELF_AT_100, 100, user(1)),
                (OTHER_AT_50, 50, user(2)),
            ] {
                book.add_limit_order_with_user(
                    Id::from_u64(id),
                    price,
                    1,
                    Side::Buy,
                    TimeInForce::Gtc,
                    owner,
                    None,
                )
                .expect("seed bid");
            }

            // 150 sells one unit at 100 (the non-self bid), leaving 50: dust
            // at 100, a whole unit at 50.
            let result = book
                .submit_market_order_by_amount_with_user(
                    Id::from_u64(TAKER),
                    150,
                    Side::Sell,
                    user(1),
                )
                .unwrap_or_else(|e| panic!("{mode}: the sweep continues to 50, got {e:?}"));

            assert_eq!(
                executed(&result),
                2,
                "{mode}: one unit at 100 and one at 50"
            );
            assert!(
                book.get_order(Id::from_u64(OTHER_AT_100)).is_none()
                    && book.get_order(Id::from_u64(OTHER_AT_50)).is_none(),
                "{mode}: both non-self bids were consumed"
            );
            let self_bid = book
                .get_order(Id::from_u64(SELF_AT_100))
                .unwrap_or_else(|| panic!("{mode}: the unaffordable same-user bid survives"));
            assert_eq!(self_bid.visible_quantity().as_u64(), 1);
            assert!(
                !matches!(
                    book.order_status(Id::from_u64(SELF_AT_100)),
                    Some(OrderStatus::Cancelled { .. })
                ),
                "{mode}: no cancel recorded for an unreachable maker"
            );
            assert_eq!(
                book.best_bid(),
                Some(100),
                "{mode}: the self bid still tops the book"
            );
        }
    }

    /// Lot rounding produces the same dust: with a lot of 5, a budget of
    /// 700 at 100 caps at 7, rounds to 5, fills the non-self lot, and the
    /// 200 left over cannot fund another whole lot. The maker is untouched
    /// and the taker keeps its fill rather than being cancelled.
    #[test]
    fn lot_rounded_quote_residual_leaves_unreachable_maker_intact() {
        for mode in [STPMode::CancelTaker, STPMode::CancelBoth] {
            let mut book: OrderBook<()> = DefaultOrderBook::new("STPL");
            book.set_stp_mode(mode);
            book.set_lot_size(5);
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
                10,
                Side::Sell,
                TimeInForce::Gtc,
                user(1),
                None,
            )
            .expect("seed same-user maker");

            let result = book
                .submit_market_order_by_amount_with_user(
                    Id::from_u64(TAKER),
                    700,
                    Side::Buy,
                    user(1),
                )
                .unwrap_or_else(|e| {
                    panic!("{mode}: a sub-lot residual never reaches the maker, got {e:?}")
                });

            assert_eq!(
                executed(&result),
                5,
                "{mode}: one whole lot against the non-self maker"
            );
            let maker = book
                .get_order(Id::from_u64(SELF_MAKER))
                .unwrap_or_else(|| panic!("{mode}: same-user maker still rests"));
            assert_eq!(maker.visible_quantity().as_u64(), 10);
            assert!(
                !matches!(
                    book.order_status(Id::from_u64(SELF_MAKER)),
                    Some(OrderStatus::Cancelled { .. })
                ),
                "{mode}: no cancel recorded for an unreachable maker"
            );
        }
    }

    /// The modify precheck applies the same per-level rule: a bid repriced
    /// into a level whose non-self depth covers it is admitted, fills, and
    /// never touches the same-user maker behind that depth. Before, any
    /// same-user order at the level rejected the reprice outright.
    #[test]
    fn repricing_into_covered_same_level_depth_is_admitted() {
        for mode in [STPMode::CancelTaker, STPMode::CancelBoth] {
            let book = book_with_self_maker_behind(mode);
            book.add_limit_order_with_user(
                Id::from_u64(TAKER),
                90,
                3,
                Side::Buy,
                TimeInForce::Gtc,
                user(1),
                None,
            )
            .expect("rest the bid below the market");

            book.update_order(OrderUpdate::UpdatePrice {
                order_id: Id::from_u64(TAKER),
                new_price: Price::new(PRICE),
            })
            .unwrap_or_else(|e| panic!("{mode}: covered by the non-self depth, got {e:?}"))
            .unwrap_or_else(|| panic!("{mode}: the repriced order was found"));

            assert!(
                book.get_order(Id::from_u64(TAKER)).is_none(),
                "{mode}: the repriced bid filled completely"
            );
            assert_eq!(
                book.get_order(Id::from_u64(OTHER_MAKER))
                    .unwrap_or_else(|| panic!("{mode}: the non-self maker still rests"))
                    .visible_quantity()
                    .as_u64(),
                2,
                "{mode}: filled against the non-self maker"
            );
            assert_self_maker_intact(&book);
        }
    }

    /// And a reprice the non-self depth cannot cover is still refused
    /// before the original is cancelled, so the original survives.
    #[test]
    fn repricing_past_same_level_depth_is_refused_before_cancel() {
        for mode in [STPMode::CancelTaker, STPMode::CancelBoth] {
            let book = book_with_self_maker_behind(mode);
            book.add_limit_order_with_user(
                Id::from_u64(TAKER),
                90,
                7,
                Side::Buy,
                TimeInForce::Gtc,
                user(1),
                None,
            )
            .expect("rest the bid below the market");

            let err = book
                .update_order(OrderUpdate::UpdatePrice {
                    order_id: Id::from_u64(TAKER),
                    new_price: Price::new(PRICE),
                })
                .expect_err("7 outruns the 5 non-self lots and reaches the same-user maker");
            assert!(
                matches!(err, OrderBookError::SelfTradePrevented { .. }),
                "{mode}: expected SelfTradePrevented, got {err:?}"
            );

            let original = book
                .get_order(Id::from_u64(TAKER))
                .unwrap_or_else(|| panic!("{mode}: the original survives a refused reprice"));
            assert_eq!(original.price().as_u128(), 90);
            assert_eq!(original.visible_quantity().as_u64(), 7);
            assert_eq!(
                book.get_order(Id::from_u64(OTHER_MAKER))
                    .unwrap_or_else(|| panic!("{mode}: nothing traded"))
                    .visible_quantity()
                    .as_u64(),
                5
            );
            assert_self_maker_intact(&book);
        }
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

    /// A non-self reserve showing 3 (hidden 2) ahead of the same-user
    /// maker, on a book whose lot size becomes 5 after both rest. #226
    /// validates reserve tranches per lot on admission, but a maker that is
    /// already resting keeps its misaligned tranche when the lot changes
    /// (documented on `set_lot_size`), so a base-quantity taker of 5 fills
    /// 3 and is left with 2 in front of its own maker.
    fn book_with_sub_lot_reserve_ahead(mode: STPMode, auto_replenish: bool) -> OrderBook<()> {
        let mut book: OrderBook<()> = DefaultOrderBook::new("STPV");
        book.set_stp_mode(mode);
        book.set_order_state_tracker(OrderStateTracker::new());
        book.add_order(OrderType::ReserveOrder {
            id: Id::from_u64(OTHER_MAKER),
            price: Price::new(PRICE),
            visible_quantity: Quantity::new(3),
            hidden_quantity: Quantity::new(2),
            side: Side::Sell,
            user_id: user(2),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            replenish_threshold: Quantity::new(1),
            replenish_amount: Some(NonZeroU64::new(2).expect("nonzero")),
            auto_replenish,
            extra_fields: (),
        })
        .expect("seed reserve before the lot size is set");
        book.add_limit_order_with_user(
            Id::from_u64(SELF_MAKER),
            PRICE,
            5,
            Side::Sell,
            TimeInForce::Gtc,
            user(1),
            None,
        )
        .expect("seed same-user maker");
        book.set_lot_size(5);
        book
    }

    /// A base-quantity residual is not dust the sweep may walk past: rested,
    /// the 2 left over would cross the taker's own maker at this price and
    /// sit misaligned on the new lot. The
    /// STP verdict stands — the taker is cancelled with its true fill and
    /// nothing rests — and `CancelBoth` still takes the maker with it.
    #[test]
    fn base_quantity_residual_behind_a_sub_lot_reserve_keeps_the_stp_verdict() {
        for mode in [STPMode::CancelTaker, STPMode::CancelBoth] {
            let book = book_with_sub_lot_reserve_ahead(mode, true);

            let err = book
                .add_limit_order_with_user(
                    Id::from_u64(TAKER),
                    PRICE,
                    5,
                    Side::Buy,
                    TimeInForce::Gtc,
                    user(1),
                    None,
                )
                .expect_err("the residual reaches the same-user maker");
            assert!(
                matches!(err, OrderBookError::SelfTradePrevented { .. }),
                "{mode}: expected SelfTradePrevented, got {err:?}"
            );
            assert_eq!(
                book.order_status(Id::from_u64(TAKER)),
                Some(OrderStatus::Cancelled {
                    filled_quantity: 3,
                    reason: CancelReason::SelfTradePrevention,
                }),
                "{mode}: taker cancelled with its true non-self fill"
            );
            assert!(
                book.get_order(Id::from_u64(TAKER)).is_none() && book.best_bid().is_none(),
                "{mode}: no sub-lot residual rests crossed against the taker's own maker"
            );
            match mode {
                STPMode::CancelBoth => assert!(
                    book.get_order(Id::from_u64(SELF_MAKER)).is_none(),
                    "CancelBoth cancels the reached maker"
                ),
                _ => assert_eq!(
                    book.get_order(Id::from_u64(SELF_MAKER))
                        .expect("CancelTaker leaves the maker resting")
                        .visible_quantity()
                        .as_u64(),
                    5
                ),
            }
        }
    }

    /// The precheck reaches the same verdict for that book: a reprice of 5
    /// into the level is refused before the original is cancelled.
    #[test]
    fn repricing_behind_a_sub_lot_reserve_is_refused_before_cancel() {
        for mode in [STPMode::CancelTaker, STPMode::CancelBoth] {
            let book = book_with_sub_lot_reserve_ahead(mode, true);
            book.add_limit_order_with_user(
                Id::from_u64(TAKER),
                90,
                5,
                Side::Buy,
                TimeInForce::Gtc,
                user(1),
                None,
            )
            .expect("rest the bid below the market");

            let err = book
                .update_order(OrderUpdate::UpdatePrice {
                    order_id: Id::from_u64(TAKER),
                    new_price: Price::new(PRICE),
                })
                .expect_err("3 visible lots cannot cover 5");
            assert!(
                matches!(err, OrderBookError::SelfTradePrevented { .. }),
                "{mode}: expected SelfTradePrevented, got {err:?}"
            );
            let original = book
                .get_order(Id::from_u64(TAKER))
                .unwrap_or_else(|| panic!("{mode}: the original survives a refused reprice"));
            assert_eq!(original.price().as_u128(), 90);
            assert_eq!(
                book.get_order(Id::from_u64(OTHER_MAKER))
                    .unwrap_or_else(|| panic!("{mode}: nothing traded"))
                    .visible_quantity()
                    .as_u64(),
                3
            );
            assert_self_maker_intact_at(&book, 5);
        }
    }

    /// Dust the sweep stops on before a deeper same-user level is not an STP
    /// decision at all: the sweep breaks on a zero lot-rounded cap and never
    /// scans that level. The precheck mirrors the same cap, so its verdict
    /// on a reprice equals the engine's verdict on the same order submitted
    /// directly — checked here on twin books rather than by asserting the
    /// engine's dust handling itself.
    #[test]
    fn precheck_agrees_with_the_sweep_on_sub_lot_dust_before_a_deeper_self_level() {
        const DEEPER: u128 = PRICE + 1;
        for mode in [STPMode::CancelTaker, STPMode::CancelBoth] {
            let build = || {
                let mut book: OrderBook<()> = DefaultOrderBook::new("STPD");
                book.set_stp_mode(mode);
                book.set_order_state_tracker(OrderStateTracker::new());
                // Non-replenishing, admitted before the lot size is set: the
                // sweep draws the visible 3 and leaves the hidden 2 undrawn,
                // so a taker of 5 keeps a residual of 2.
                book.add_order(OrderType::ReserveOrder {
                    id: Id::from_u64(OTHER_MAKER),
                    price: Price::new(PRICE),
                    visible_quantity: Quantity::new(3),
                    hidden_quantity: Quantity::new(2),
                    side: Side::Sell,
                    user_id: user(2),
                    timestamp: TimestampMs::new(0),
                    time_in_force: TimeInForce::Gtc,
                    replenish_threshold: Quantity::new(1),
                    replenish_amount: None,
                    auto_replenish: false,
                    extra_fields: (),
                })
                .expect("seed reserve before the lot size is set");
                book.add_limit_order_with_user(
                    Id::from_u64(SELF_MAKER),
                    DEEPER,
                    5,
                    Side::Sell,
                    TimeInForce::Gtc,
                    user(1),
                    None,
                )
                .expect("seed same-user maker one level deeper");
                book.set_lot_size(5);
                book
            };

            let repriced = build();
            repriced
                .add_limit_order_with_user(
                    Id::from_u64(TAKER),
                    90,
                    5,
                    Side::Buy,
                    TimeInForce::Gtc,
                    user(1),
                    None,
                )
                .expect("rest the bid below the market");
            let via_modify = repriced
                .update_order(OrderUpdate::UpdatePrice {
                    order_id: Id::from_u64(TAKER),
                    new_price: Price::new(DEEPER),
                })
                .map(|_| ());

            let direct = build();
            let via_add = direct
                .add_limit_order_with_user(
                    Id::from_u64(TAKER),
                    DEEPER,
                    5,
                    Side::Buy,
                    TimeInForce::Gtc,
                    user(1),
                    None,
                )
                .map(|_| ());

            assert_eq!(
                via_modify.is_ok(),
                via_add.is_ok(),
                "{mode}: precheck verdict {via_modify:?} vs engine verdict {via_add:?}"
            );
            let state = |book: &OrderBook<()>| {
                (
                    book.get_order(Id::from_u64(TAKER))
                        .map(|o| (o.price().as_u128(), o.visible_quantity().as_u64())),
                    book.get_order(Id::from_u64(SELF_MAKER))
                        .map(|o| o.visible_quantity().as_u64()),
                    book.get_order(Id::from_u64(OTHER_MAKER))
                        .map(|o| o.visible_quantity().as_u64()),
                )
            };
            assert_eq!(
                state(&repriced),
                state(&direct),
                "{mode}: the repriced book and the directly-submitted book agree"
            );
        }
    }

    fn assert_self_maker_intact_at(book: &OrderBook<()>, visible: u64) {
        let maker = book
            .get_order(Id::from_u64(SELF_MAKER))
            .expect("same-user maker still rests");
        assert_eq!(maker.visible_quantity().as_u64(), visible);
        assert!(
            !matches!(
                book.order_status(Id::from_u64(SELF_MAKER)),
                Some(OrderStatus::Cancelled { .. })
            ),
            "no cancel recorded for an unreachable maker"
        );
    }
}
