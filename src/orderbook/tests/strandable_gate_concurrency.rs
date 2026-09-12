//! #230: admitting a strandable maker takes the exclusive submit gate, in
//! every `STPMode`.
//!
//! A sweep decides **once**, before walking any level, whether to run its
//! per-level strandable-maker capture, by reading
//! `OrderBook::strandable_makers_resting`. That read is only coherent with
//! what the sweep later encounters if no strandable maker can be admitted
//! while the sweep is in flight. Without the exclusive gate, this
//! interleaving loses a discard entirely:
//!
//! ```text
//! asks 1@100, 1@101; a buy of 3@101 starts and reads the count as 0
//! ...the 100 level fills...
//! another thread admits a reserve {visible 1, hidden 20, auto off} @101
//! ...the sweep reaches 101, consumes and removes it, reports nothing
//! ```
//!
//! These tests park a plain (`STPMode::None`) sweep between the two levels
//! with the test-only `level_interleave_hook` and drive a competing
//! admission against it. The interleaving is channel rendezvous only: no
//! sleeps, and every timeout is a hung-test detector that panics rather than
//! a branch selector.
//!
//! The competitor branches on `submit_needs_exclusive_gate`, the same pure
//! predicate the production path consults, so the test is deterministic
//! under **both** policies: with the exclusive gate it must release the
//! parked sweep before its admission can proceed, and with the shared gate
//! it lands inside the window, which is the defect the final assertions
//! catch.

#[cfg(test)]
mod tests {
    use crate::orderbook::book::OrderBook;
    use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    /// Hung-test detector only: every rendezvous is expected to complete
    /// immediately, so a timeout means a deadlock, never a slow machine
    /// taking a different branch.
    const RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(10);

    const NEAR_PRICE: u128 = 100;
    const FAR_PRICE: u128 = 101;

    /// What the competitor observed about the gate while the sweep was
    /// parked.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum GateObservation {
        /// `try_write` failed: the parked sweep still holds the shared side,
        /// so an exclusive acquire would block behind it.
        SweepHoldsShared,
        /// `try_read` failed: the parked sweep holds the **exclusive** side,
        /// so nothing at all can interleave with it.
        SweepHoldsExclusive,
        /// Nothing held the gate, so the sweep was not actually in flight
        /// and the test proves nothing.
        Free,
    }

    /// Channels joining the parked sweep and the competing thread.
    struct Rendezvous {
        parked_rx: Receiver<u128>,
        resume_tx: Sender<()>,
    }

    /// The book's strandable-maker count.
    fn count_of(book: &OrderBook<()>) -> usize {
        book.strandable_makers_resting.load(Ordering::Relaxed)
    }

    /// A reserve SELL at `FAR_PRICE`: 1 visible / 20 hidden, no automatic
    /// replenishment. The strandable shape, resting on the side the buy
    /// sweep consumes.
    fn strandable_reserve(id: Id) -> OrderType<()> {
        OrderType::ReserveOrder {
            id,
            price: Price::new(FAR_PRICE),
            visible_quantity: Quantity::new(1),
            hidden_quantity: Quantity::new(20),
            side: Side::Sell,
            user_id: Hash32::zero(),
            timestamp: TimestampMs::new(0),
            time_in_force: TimeInForce::Gtc,
            replenish_threshold: Quantity::new(0),
            replenish_amount: None,
            auto_replenish: false,
            extra_fields: (),
        }
    }

    /// Install the test-only level hook so the sweep parks once, just before
    /// it matches `price`.
    fn install_level_hook(book: &mut OrderBook<()>, price: u128) -> Rendezvous {
        let (parked_tx, parked_rx) = channel::<u128>();
        let (resume_tx, resume_rx) = channel::<()>();

        let armed = AtomicBool::new(true);
        let parked_tx = Mutex::new(parked_tx);
        let resume_rx = Mutex::new(resume_rx);

        book.level_interleave_hook = Some(Arc::new(move |level_price: u128| {
            if level_price != price {
                return;
            }
            if !armed.swap(false, Ordering::SeqCst) {
                return;
            }
            parked_tx
                .lock()
                .expect("park channel mutex")
                .send(level_price)
                .expect("competing thread dropped the park channel");
            let released = resume_rx
                .lock()
                .expect("resume channel mutex")
                .recv_timeout(RENDEZVOUS_TIMEOUT);
            assert!(
                released.is_ok(),
                "hung test: the competing thread never released the parked sweep \
                 within {RENDEZVOUS_TIMEOUT:?}"
            );
        }));

        Rendezvous {
            parked_rx,
            resume_tx,
        }
    }

    /// The book with the two ask levels the sweep will walk.
    fn book_with_two_asks() -> OrderBook<()> {
        let book: OrderBook<()> = OrderBook::new("STRANDABLE-GATE");
        book.add_limit_order(
            Id::from_u64(1),
            NEAR_PRICE,
            1,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("near ask");
        book.add_limit_order(
            Id::from_u64(2),
            FAR_PRICE,
            1,
            Side::Sell,
            TimeInForce::Gtc,
            None,
        )
        .expect("far ask");
        book
    }

    /// The reviewer's interleaving: a strandable maker admitted while a
    /// sweep is in flight must not be swallowed by that sweep.
    ///
    /// With the exclusive gate the admission blocks until the sweep ends, so
    /// the sweep executes 2 (1@100 + 1@101) against the two plain asks and
    /// the reserve then rests 1 / 20 intact, counted by
    /// `strandable_makers_resting`. A second sweep consumes it and reports
    /// the discard.
    #[test]
    fn test_strandable_admission_blocks_until_an_in_flight_sweep_ends() {
        let executed_log: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let mut book = book_with_two_asks();
        let listener_log = Arc::clone(&executed_log);
        book.trade_listener = Some(Arc::new(move |result| {
            let filled: u64 = result
                .match_result
                .trades()
                .as_vec()
                .iter()
                .map(|print| print.quantity().as_u64())
                .sum();
            *listener_log.lock().expect("executed log mutex") += filled;
        }));
        let rendezvous = install_level_hook(&mut book, FAR_PRICE);
        let book = Arc::new(book);
        assert_eq!(
            book.strandable_makers_resting.load(Ordering::Relaxed),
            0,
            "no strandable maker rests before the interleaving"
        );

        let reserve_id = Id::from_u64(3);
        let competitor_book = Arc::clone(&book);
        let competitor = thread::spawn(move || {
            let parked = rendezvous.parked_rx.recv_timeout(RENDEZVOUS_TIMEOUT);
            assert!(
                parked.is_ok(),
                "hung test: the sweep never reached the level hook within \
                 {RENDEZVOUS_TIMEOUT:?}"
            );

            // `try_write` fails while the sweep holds the shared side, which
            // is what an exclusive admission would have to wait for.
            let observation = match competitor_book.submit_gate.try_write() {
                Err(std::sync::TryLockError::WouldBlock) => GateObservation::SweepHoldsShared,
                Ok(guard) => {
                    drop(guard);
                    GateObservation::Free
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    panic!("submit gate poisoned: a prior panic unwound while it was held")
                }
            };

            let reserve = strandable_reserve(reserve_id);
            // Branch on the production predicate so the rendezvous is
            // deadlock-free under either policy. Exclusive: the admission
            // cannot proceed until the parked sweep releases, so release
            // first. Shared (the defect): run the admission inside the
            // window, where it lands in front of the sweep.
            let exclusive = competitor_book.submit_needs_exclusive_gate(
                false,
                Hash32::zero(),
                false,
                OrderBook::<()>::is_strandable_maker(&reserve),
            );
            if exclusive {
                rendezvous
                    .resume_tx
                    .send(())
                    .expect("parked sweep dropped the resume channel");
                let admitted = competitor_book.add_order(reserve);
                assert!(
                    admitted.is_ok(),
                    "the reserve must be admitted: {admitted:?}"
                );
            } else {
                let admitted = competitor_book.add_order(reserve);
                assert!(
                    admitted.is_ok(),
                    "the reserve must be admitted: {admitted:?}"
                );
                rendezvous
                    .resume_tx
                    .send(())
                    .expect("parked sweep dropped the resume channel");
            }
            (observation, exclusive)
        });

        let sweep_book = Arc::clone(&book);
        let sweeper = thread::spawn(move || {
            // IOC: the reviewer's buy of 3 walks both levels and its
            // remainder is dropped rather than rested, so no residual bid is
            // left at FAR_PRICE for the reserve's own admission to cross.
            sweep_book.add_limit_order(
                Id::from_u64(4),
                FAR_PRICE,
                3,
                Side::Buy,
                TimeInForce::Ioc,
                None,
            )
        });

        let (observation, exclusive) = competitor.join().expect("competitor thread panicked");
        // An IOC with an unfilled remainder reports `InsufficientLiquidity`;
        // the executed quantity is read off the trade listener instead.
        let _ = sweeper.join().expect("sweeping thread panicked");

        assert_eq!(
            observation,
            GateObservation::SweepHoldsShared,
            "the sweep must still hold the shared side while parked, otherwise \
             this test proves nothing about the interleaving"
        );
        assert!(
            exclusive,
            "admitting a strandable maker must take the exclusive gate (#230)"
        );

        let executed = *executed_log.lock().expect("executed log mutex");
        assert_eq!(
            executed, 2,
            "the sweep must execute only the two plain asks it started against; \
             executing 3 means it swallowed the reserve admitted mid-flight"
        );

        // The decisive assertion: the reserve was NOT swallowed by the sweep.
        match book.get_order(reserve_id) {
            Some(order) => assert_eq!(
                (
                    order.visible_quantity().as_u64(),
                    order.hidden_quantity().as_u64()
                ),
                (1, 20),
                "the reserve must rest intact, admitted after the sweep ended"
            ),
            None => panic!(
                "the reserve was consumed by a sweep that never captured it: \
                 its hidden tranche was discarded with no report (#230)"
            ),
        }
        assert_eq!(
            book.strandable_makers_resting.load(Ordering::Relaxed),
            1,
            "the rested reserve is counted, so the next sweep will capture it"
        );

        // A second sweep now sees the maker, captures it, and removes it with
        // its hidden tranche discarded.
        book.add_limit_order(
            Id::from_u64(5),
            FAR_PRICE,
            1,
            Side::Buy,
            TimeInForce::Gtc,
            None,
        )
        .expect("the second sweep is accepted");
        assert!(
            book.get_order(reserve_id).is_none(),
            "the depleted non-replenishing reserve leaves the book"
        );
        assert_eq!(
            book.strandable_makers_resting.load(Ordering::Relaxed),
            0,
            "the count returns to zero, closing the gate again"
        );
    }

    /// Capture attribution survives a concurrent cancel and id reuse
    /// (#230). This is the window exclusive *admission* alone does not
    /// close, and the reason every sweep in a book holding strandable
    /// makers runs exclusively.
    ///
    /// Level 101 holds a plain ask A (1) **and** the strandable reserve X
    /// (1 visible / 20 hidden), so removing X alone never empties the
    /// level. A buy of 2@101 parks after capturing that level and before
    /// matching it; the competitor tries to cancel X and re-admit a plain
    /// `Standard` 1@101 reusing X's id.
    ///
    /// With the fix the competitor's cancel blocks on the gate the sweep
    /// holds exclusively, so the sweep consumes A and X, strands X's 20
    /// hidden and reports them; the cancel then fails with `OrderNotFound`
    /// and the re-admission rests as a fresh order. Without it the cancel
    /// and the re-admission land inside the window, the sweep fills the
    /// impostor, and the stale capture reports 20 units discarded that
    /// nothing ever stranded.
    #[test]
    fn test_capture_attribution_survives_id_reuse() {
        let discarded: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let mut book: OrderBook<()> = OrderBook::new("STRANDABLE-ID-REUSE");

        // Level 101: a plain ask first, then the strandable reserve behind
        // it, so the level survives X's removal either way.
        let plain_ask = Id::from_u64(31);
        book.add_limit_order(plain_ask, FAR_PRICE, 1, Side::Sell, TimeInForce::Gtc, None)
            .expect("plain ask rests");
        let reused_id = Id::from_u64(32);
        book.add_order(strandable_reserve(reused_id))
            .expect("the strandable reserve rests behind it");
        assert_eq!(count_of(&book), 1, "the reserve is counted");

        let rendezvous = install_level_hook(&mut book, FAR_PRICE);
        let book = Arc::new(book);

        let competitor_book = Arc::clone(&book);
        let competitor = thread::spawn(move || {
            let parked = rendezvous.parked_rx.recv_timeout(RENDEZVOUS_TIMEOUT);
            assert!(
                parked.is_ok(),
                "hung test: the sweep never reached the level hook within \
                 {RENDEZVOUS_TIMEOUT:?}"
            );

            // A sweep in this book runs exclusively, so even the shared side
            // is unavailable while it is parked.
            let observation = match competitor_book.submit_gate.try_read() {
                Err(std::sync::TryLockError::WouldBlock) => GateObservation::SweepHoldsExclusive,
                Ok(guard) => {
                    drop(guard);
                    GateObservation::SweepHoldsShared
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    panic!("submit gate poisoned: a prior panic unwound while it was held")
                }
            };

            // Branch on what the gate actually allows, so the rendezvous is
            // deadlock-free under either policy and the defect is reachable
            // under the shared one. Exclusive: these operations cannot run
            // until the sweep releases, so release first. Shared (the
            // defect): run them *inside* the capture window, where the
            // cancel and the id reuse invalidate the sweep's capture.
            let inside_window = observation == GateObservation::SweepHoldsShared;
            let run_competing_ops = |book: &OrderBook<()>| {
                let cancelled = book.cancel_order(reused_id);
                let readmitted = book.add_limit_order(
                    reused_id,
                    FAR_PRICE,
                    1,
                    Side::Sell,
                    TimeInForce::Gtc,
                    None,
                );
                (cancelled, readmitted)
            };
            let (cancelled, readmitted) = if inside_window {
                let outcome = run_competing_ops(&competitor_book);
                rendezvous
                    .resume_tx
                    .send(())
                    .expect("parked sweep dropped the resume channel");
                outcome
            } else {
                rendezvous
                    .resume_tx
                    .send(())
                    .expect("parked sweep dropped the resume channel");
                run_competing_ops(&competitor_book)
            };
            // `cancel_order` reports "nothing to cancel" as `Ok(None)`, not
            // an error, so the meaningful question is whether it actually
            // removed an order.
            (
                observation,
                matches!(cancelled, Ok(Some(_))),
                readmitted.is_ok(),
            )
        });

        let sweep_book = Arc::clone(&book);
        let sweep_log = Arc::clone(&discarded);
        let sweeper = thread::spawn(move || {
            // Capture what the sweep reports as stranded, by observing the
            // reserve's terminal state rather than the metrics recorder,
            // which this test binary does not install.
            let result = sweep_book.add_limit_order(
                Id::from_u64(33),
                FAR_PRICE,
                2,
                Side::Buy,
                TimeInForce::Gtc,
                None,
            );
            sweep_log
                .lock()
                .expect("discard log mutex")
                .push(u64::from(result.is_ok()));
            result.is_ok()
        });

        let (observation, cancel_removed, readmit_ok) =
            competitor.join().expect("competitor thread panicked");
        let swept_ok = sweeper.join().expect("sweeping thread panicked");
        assert!(swept_ok, "the sweep itself must succeed");

        assert!(
            !cancel_removed,
            "the cancel must run only after the sweep consumed the reserve, so \
             it finds nothing to remove (`Ok(None)`); removing it means the \
             cancel landed inside the sweep's capture window and the sweep \
             went on to fill an impostor under the captured id"
        );
        assert!(
            readmit_ok,
            "the re-admission under the freed id must succeed as a fresh order"
        );
        match book.get_order(reused_id) {
            Some(order) => assert_eq!(
                (
                    order.visible_quantity().as_u64(),
                    order.hidden_quantity().as_u64()
                ),
                (1, 0),
                "the id now belongs to the plain re-admitted order, not the reserve"
            ),
            None => panic!("the re-admitted order must rest"),
        }
        assert_eq!(
            count_of(&book),
            0,
            "the reserve was consumed by the sweep and the plain order that \
             reused its id is not strandable"
        );
        assert_eq!(
            observation,
            GateObservation::SweepHoldsExclusive,
            "a sweep in a book holding a strandable maker must hold the \
             exclusive side, so a concurrent cancel cannot land inside its \
             capture window"
        );

        assert_eq!(
            observation,
            GateObservation::SweepHoldsExclusive,
            "the mechanism behind the assertions above: a sweep in a book \
             holding a strandable maker holds the exclusive side, so nothing \
             can land inside its capture window"
        );
    }

    /// The same race through the **anonymous match-only** entry point
    /// (#230). `match_order` is a sweep like any other, so it must take the
    /// exclusive side in a book holding strandable makers; before the fix it
    /// took `submit_gate_read()` directly and a concurrent cancel could land
    /// inside its capture window, leaving the drain to report a discard that
    /// never happened and to decrement the count a second time.
    #[test]
    fn test_capture_attribution_survives_id_reuse_through_match_order() {
        let mut book: OrderBook<()> = OrderBook::new("STRANDABLE-MATCH-ORDER");
        let plain_ask = Id::from_u64(51);
        book.add_limit_order(plain_ask, FAR_PRICE, 1, Side::Sell, TimeInForce::Gtc, None)
            .expect("plain ask rests");
        let reused_id = Id::from_u64(52);
        book.add_order(strandable_reserve(reused_id))
            .expect("the strandable reserve rests behind it");
        assert_eq!(count_of(&book), 1, "the reserve is counted");

        let rendezvous = install_level_hook(&mut book, FAR_PRICE);
        let book = Arc::new(book);

        let competitor_book = Arc::clone(&book);
        let competitor = thread::spawn(move || {
            let parked = rendezvous.parked_rx.recv_timeout(RENDEZVOUS_TIMEOUT);
            assert!(
                parked.is_ok(),
                "hung test: the sweep never reached the level hook within \
                 {RENDEZVOUS_TIMEOUT:?}"
            );

            let observation = match competitor_book.submit_gate.try_read() {
                Err(std::sync::TryLockError::WouldBlock) => GateObservation::SweepHoldsExclusive,
                Ok(guard) => {
                    drop(guard);
                    GateObservation::SweepHoldsShared
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    panic!("submit gate poisoned: a prior panic unwound while it was held")
                }
            };

            let inside_window = observation == GateObservation::SweepHoldsShared;
            let run_competing_ops = |book: &OrderBook<()>| {
                let cancelled = book.cancel_order(reused_id);
                let readmitted = book.add_limit_order(
                    reused_id,
                    FAR_PRICE,
                    1,
                    Side::Sell,
                    TimeInForce::Gtc,
                    None,
                );
                (cancelled, readmitted)
            };
            let (cancelled, _readmitted) = if inside_window {
                let outcome = run_competing_ops(&competitor_book);
                rendezvous
                    .resume_tx
                    .send(())
                    .expect("parked sweep dropped the resume channel");
                outcome
            } else {
                rendezvous
                    .resume_tx
                    .send(())
                    .expect("parked sweep dropped the resume channel");
                run_competing_ops(&competitor_book)
            };
            (observation, matches!(cancelled, Ok(Some(_))))
        });

        let sweep_book = Arc::clone(&book);
        let sweeper = thread::spawn(move || {
            // The anonymous match-only entry point: no order is submitted,
            // so only the strandable rule can make this exclusive.
            sweep_book.match_order(Id::from_u64(53), Side::Buy, 2, Some(FAR_PRICE))
        });

        let (observation, cancel_removed) = competitor.join().expect("competitor thread panicked");
        let matched = sweeper.join().expect("sweeping thread panicked");
        assert!(
            matched.is_ok(),
            "the sweep itself must succeed: {matched:?}"
        );

        assert!(
            !cancel_removed,
            "the cancel must run only after the sweep consumed the reserve; \
             removing it means the cancel landed inside `match_order`'s \
             capture window and the drain reported a discard that never \
             happened"
        );
        assert_eq!(
            count_of(&book),
            0,
            "the reserve was consumed once; the plain order that reused its \
             id is not strandable and must not decrement again"
        );
        assert_eq!(
            observation,
            GateObservation::SweepHoldsExclusive,
            "`match_order` is a sweep and must take the exclusive side in a \
             book holding a strandable maker"
        );
    }

    /// The ordinary case for contrast: a strandable maker admitted *before*
    /// the sweep starts is captured and reported, and the count tracks it.
    #[test]
    fn test_strandable_maker_admitted_before_the_sweep_is_captured() {
        let book: OrderBook<()> = OrderBook::new("STRANDABLE-BEFORE");
        let reserve_id = Id::from_u64(11);
        book.add_order(strandable_reserve(reserve_id))
            .expect("the reserve rests");
        assert_eq!(
            book.strandable_makers_resting.load(Ordering::Relaxed),
            1,
            "the resting maker is counted"
        );

        book.add_limit_order(
            Id::from_u64(12),
            FAR_PRICE,
            1,
            Side::Buy,
            TimeInForce::Gtc,
            None,
        )
        .expect("the sweep is accepted");

        assert!(
            book.get_order(reserve_id).is_none(),
            "the depleted maker leaves the book"
        );
        assert_eq!(
            book.strandable_makers_resting.load(Ordering::Relaxed),
            0,
            "the fill drain decrements the count"
        );
    }

    /// A re-price of a strandable reserve holds the **exclusive** side for
    /// the whole cancel-then-add, so its residual dry run cannot race a
    /// concurrent mutation of the opposite side.
    #[test]
    fn test_reprice_of_strandable_reserve_takes_the_exclusive_gate() {
        let book: OrderBook<()> = OrderBook::new("STRANDABLE-REPRICE");
        let reserve_id = Id::from_u64(21);
        book.add_order(strandable_reserve(reserve_id))
            .expect("the reserve rests");

        let plain_id = Id::from_u64(22);
        book.add_limit_order(plain_id, NEAR_PRICE, 5, Side::Sell, TimeInForce::Gtc, None)
            .expect("a plain maker rests too");

        let reprice = pricelevel::OrderUpdate::UpdatePrice {
            order_id: reserve_id,
            new_price: Price::new(NEAR_PRICE),
        };
        assert!(
            book.modify_needs_exclusive_gate(&reprice),
            "re-pricing a strandable reserve must take the exclusive gate (#230)"
        );

        // The rule is the book's count, not the order being modified: while
        // a strandable maker rests, re-pricing *any* order is exclusive.
        // Deciding from a lookup of the order would read state outside the
        // gate, where a concurrent cancel and id reuse can invalidate it —
        // which is the case `test_capture_attribution_survives_id_reuse`
        // pins.
        let plain_reprice = pricelevel::OrderUpdate::UpdatePrice {
            order_id: plain_id,
            new_price: Price::new(NEAR_PRICE - 1),
        };
        assert!(
            book.modify_needs_exclusive_gate(&plain_reprice),
            "every re-price is exclusive while the book holds a strandable maker"
        );

        // Once the strandable maker is gone the count is zero and re-prices
        // return to the shared side.
        book.cancel_order(reserve_id).expect("cancel the reserve");
        assert_eq!(
            count_of(&book),
            0,
            "the cancel decrements the strandable-maker count"
        );
        assert!(
            !book.modify_needs_exclusive_gate(&plain_reprice),
            "a book holding none is unaffected: re-prices stay shared"
        );

        // And the non-matching variants stay shared throughout: they cannot
        // match, so they cannot invalidate a capture. They simply never
        // overlap an exclusive sweep.
        let cancel = pricelevel::OrderUpdate::Cancel {
            order_id: reserve_id,
        };
        assert!(
            !book.modify_needs_exclusive_gate(&cancel),
            "Cancel cannot match, so it keeps the shared side"
        );
        let resize = pricelevel::OrderUpdate::UpdateQuantity {
            order_id: plain_id,
            new_quantity: Quantity::new(3),
        };
        assert!(
            !book.modify_needs_exclusive_gate(&resize),
            "UpdateQuantity adjusts in place and keeps the shared side"
        );
    }
}
