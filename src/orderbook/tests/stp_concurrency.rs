//! Deterministic interleaving tests for self-trade prevention under
//! concurrent book mutation (#225).
//!
//! The matching engine decides the [`STPAction`](crate::orderbook::stp::STPAction)
//! for a price level by snapshotting its queue, then acts on that decision in
//! a second operation on the same level. Before #225 both steps ran under the
//! *shared* side of the submit gate, so another thread could admit, cancel or
//! re-price an order in between and the verdict was applied to state it had
//! never been taken on — producing a same-user trade.
//!
//! These tests park a taker exactly inside that window using the test-only
//! `stp_interleave_hook`, drive a competing mutation against it, and assert
//! both that the competitor was excluded and that the resulting book state is
//! correct. The interleaving is driven entirely by channel rendezvous: no
//! sleeps, and every timeout is a hung-test detector that panics rather than
//! a branch selector.
//!
//! # What `Blocked` does and does not prove
//!
//! The `Blocked` / `Landed` branch is decided by one `submit_gate.try_read()`
//! in the competing thread. `std::sync::RwLock::try_read` is permitted to
//! return `WouldBlock` spuriously, and that failure mode is asymmetric: in a
//! pre-fix build a spurious `WouldBlock` would take the `Blocked` branch and
//! turn a real defect into a false *green*, never a false red. So `Blocked`
//! alone is not independent evidence that the gate was exclusive. The
//! post-hoc state assertions carry that weight — the trade log, the resting
//! quantities and the terminal statuses are only reachable if the
//! competitor's operation actually ran after the taker completed, and they
//! are what fail loudly when the gate is shared.

#[cfg(test)]
mod tests {
    use crate::orderbook::book::OrderBook;
    use crate::orderbook::error::OrderBookError;
    use crate::orderbook::order_state::{CancelReason, OrderStateTracker, OrderStatus};
    use crate::orderbook::stp::STPMode;
    use crate::orderbook::trade::TradeResult;
    use pricelevel::{Hash32, Id, OrderUpdate, Price, Quantity, Side, TimeInForce};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    /// Upper bound on every channel rendezvous. Expiry means a thread never
    /// reached the point it was supposed to reach, i.e. the test itself hung —
    /// it never selects an outcome.
    const RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(10);

    /// What the competing thread observed about the submit gate at the instant
    /// the taker was parked inside the STP window.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum GateObservation {
        /// The gate was held exclusively: the competing mutation could not
        /// land inside the window. This is the post-#225 behaviour.
        Blocked,
        /// The gate was held in shared mode, so the competing mutation landed
        /// between the STP scan and the fill. This is the #225 defect.
        Landed,
    }

    /// Taker-side half of the rendezvous, handed to the competing thread.
    struct Rendezvous {
        /// Receives the price of the level whose STP verdict has just been
        /// taken; the taker is parked until `resume_tx` is written.
        scanned_rx: Receiver<u128>,
        /// Releases the parked taker, carrying what the competitor observed.
        resume_tx: Sender<GateObservation>,
    }

    /// Every trade emitted during a test, as `(maker_id, taker_id, quantity)`.
    type TradeLog = Arc<Mutex<Vec<(Id, Id, u64)>>>;

    /// Build a non-zero user hash from a single byte value.
    fn user(byte: u8) -> Hash32 {
        Hash32::new([byte; 32])
    }

    /// Install a trade listener that records every emitted trade.
    fn install_trade_log(book: &mut OrderBook<()>) -> TradeLog {
        let log: TradeLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        book.trade_listener = Some(Arc::new(move |result: &TradeResult| {
            let mut trades = sink.lock().expect("trade log mutex");
            for trade in result.match_result.trades().as_vec() {
                trades.push((
                    trade.maker_order_id(),
                    trade.taker_order_id(),
                    trade.quantity().as_u64(),
                ));
            }
        }));
        log
    }

    /// Read the recorded trades out of the log.
    fn trades_of(log: &TradeLog) -> Vec<(Id, Id, u64)> {
        log.lock().expect("trade log mutex").clone()
    }

    /// Install the test-only STP interleaving hook on `book`.
    ///
    /// The hook fires at most once, and only for the level at `price`: it
    /// announces that the STP verdict for that level has been taken and then
    /// parks the taker until the competing thread releases it. Everything the
    /// competing thread needs is returned.
    fn install_interleave_hook(book: &mut OrderBook<()>, price: u128) -> Rendezvous {
        let (scanned_tx, scanned_rx) = channel::<u128>();
        let (resume_tx, resume_rx) = channel::<GateObservation>();

        let armed = AtomicBool::new(true);
        let scanned_tx = Mutex::new(scanned_tx);
        let resume_rx = Mutex::new(resume_rx);

        book.stp_interleave_hook = Some(Arc::new(move |scanned_price: u128| {
            if scanned_price != price {
                return;
            }
            if !armed.swap(false, Ordering::SeqCst) {
                return;
            }
            scanned_tx
                .lock()
                .expect("scan channel mutex")
                .send(scanned_price)
                .expect("competing thread dropped the scan channel");
            let released = resume_rx
                .lock()
                .expect("resume channel mutex")
                .recv_timeout(RENDEZVOUS_TIMEOUT);
            assert!(
                released.is_ok(),
                "hung test: the competing thread never released the parked taker within {RENDEZVOUS_TIMEOUT:?}"
            );
        }));

        Rendezvous {
            scanned_rx,
            resume_tx,
        }
    }

    /// Spawn the competing thread.
    ///
    /// It waits for the taker to park inside the STP window, makes an
    /// observable attempt on the submit gate, and then performs `op`. The
    /// ordering differs per observation on purpose:
    ///
    /// - gate exclusive: release the taker first, then run `op`, which blocks
    ///   on the gate until the taker is done;
    /// - gate shared: run `op` first — it lands inside the window, which is
    ///   the defect — and only then release the taker.
    fn spawn_competitor<R>(
        book: Arc<OrderBook<()>>,
        rendezvous: Rendezvous,
        op: impl FnOnce(&OrderBook<()>) -> R + Send + 'static,
    ) -> thread::JoinHandle<(GateObservation, R)>
    where
        R: Send + 'static,
    {
        thread::spawn(move || {
            let scanned = rendezvous.scanned_rx.recv_timeout(RENDEZVOUS_TIMEOUT);
            assert!(
                scanned.is_ok(),
                "hung test: the taker never reached the STP scan hook within {RENDEZVOUS_TIMEOUT:?}"
            );

            match book.submit_gate.try_read() {
                Err(std::sync::TryLockError::WouldBlock) => {
                    rendezvous
                        .resume_tx
                        .send(GateObservation::Blocked)
                        .expect("parked taker dropped the resume channel");
                    let outcome = op(&book);
                    (GateObservation::Blocked, outcome)
                }
                Ok(guard) => {
                    drop(guard);
                    let outcome = op(&book);
                    rendezvous
                        .resume_tx
                        .send(GateObservation::Landed)
                        .expect("parked taker dropped the resume channel");
                    (GateObservation::Landed, outcome)
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    panic!("submit gate poisoned: a prior panic unwound while it was held")
                }
            }
        })
    }

    // ---------------------------------------------------------------------
    // T1 — CancelMaker vs a concurrent same-user post-only admission
    // ---------------------------------------------------------------------

    /// A same-user post-only order must not be able to land between the STP
    /// scan of a bid level and the sweep of that same level, where the taker's
    /// own sell order would immediately fill it.
    ///
    /// The seeded same-user bid at 100 is what makes the level exist so the
    /// scan reaches it; under `CancelMaker` it is cancelled by the taker,
    /// leaving the level empty — which is exactly the slot the post-only used
    /// to slip into.
    #[test]
    fn test_stp_cancel_maker_concurrent_same_user_post_only_never_trades() {
        let u = user(1);
        let mut book: OrderBook<()> = OrderBook::new("STP-T1");
        book.set_stp_mode(STPMode::CancelMaker);
        book.set_order_state_tracker(OrderStateTracker::new());
        let trades = install_trade_log(&mut book);

        let seed_bid = Id::from_u64(1);
        let seeded =
            book.add_limit_order_with_user(seed_bid, 100, 1, Side::Buy, TimeInForce::Gtc, u, None);
        assert!(seeded.is_ok(), "seed bid must rest: {seeded:?}");

        let rendezvous = install_interleave_hook(&mut book, 100);
        let book = Arc::new(book);

        let ask_id = Id::from_u64(2);
        let post_only_id = Id::from_u64(3);

        let competitor = spawn_competitor(Arc::clone(&book), rendezvous, move |b| {
            b.add_post_only_order_with_user(
                post_only_id,
                100,
                5,
                Side::Buy,
                TimeInForce::Gtc,
                u,
                None,
            )
        });

        let ask_outcome =
            book.add_limit_order_with_user(ask_id, 100, 5, Side::Sell, TimeInForce::Gtc, u, None);
        let (observation, post_only_outcome) = competitor.join().expect("competing thread");

        assert_eq!(
            observation,
            GateObservation::Blocked,
            "the STP-active submit must hold the submit gate exclusively; \
             trades recorded={:?}",
            trades_of(&trades)
        );
        assert!(
            trades_of(&trades).is_empty(),
            "a same-user post-only must never trade against the same user's ask: {:?}",
            trades_of(&trades)
        );

        let Ok(ask) = ask_outcome else {
            panic!("the ask must rest after its same-user maker is cancelled: {ask_outcome:?}");
        };
        assert_eq!(ask.visible_quantity().as_u64(), 5, "the ask never filled");
        assert_eq!(book.best_ask(), Some(100), "the ask rests at 100");

        match post_only_outcome {
            Err(OrderBookError::PriceCrossing { .. }) => {}
            other => panic!("the post-only must be rejected as crossing, got {other:?}"),
        }
        assert!(
            book.get_order(post_only_id).is_none(),
            "a rejected post-only never rests"
        );

        match book.order_status(ask_id) {
            Some(OrderStatus::Open) => {}
            other => panic!("the ask must be Open, not cancelled, got {other:?}"),
        }
        match book.order_status(seed_bid) {
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::SelfTradePrevention,
            }) => {}
            other => panic!("the seeded same-user bid must be STP-cancelled, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // T2 — CancelTaker vs a concurrent cancel of the foreign maker
    // ---------------------------------------------------------------------

    /// Under `CancelTaker` the safe quantity is computed from the foreign
    /// depth resting ahead of the first same-user maker. Cancelling that
    /// foreign maker after the scan must not let the sweep spend the stale
    /// safe quantity on the same-user maker behind it.
    #[test]
    fn test_stp_cancel_taker_concurrent_cancel_of_foreign_maker_never_self_trades() {
        let u = user(1);
        let v = user(2);
        let mut book: OrderBook<()> = OrderBook::new("STP-T2");
        book.set_stp_mode(STPMode::CancelTaker);
        book.set_order_state_tracker(OrderStateTracker::new());
        let trades = install_trade_log(&mut book);

        let foreign_ask = Id::from_u64(1);
        let self_ask = Id::from_u64(2);
        let seeded_foreign = book.add_limit_order_with_user(
            foreign_ask,
            100,
            5,
            Side::Sell,
            TimeInForce::Gtc,
            v,
            None,
        );
        assert!(
            seeded_foreign.is_ok(),
            "seed foreign ask: {seeded_foreign:?}"
        );
        let seeded_self =
            book.add_limit_order_with_user(self_ask, 100, 9, Side::Sell, TimeInForce::Gtc, u, None);
        assert!(seeded_self.is_ok(), "seed same-user ask: {seeded_self:?}");

        let rendezvous = install_interleave_hook(&mut book, 100);
        let book = Arc::new(book);

        let taker_id = Id::from_u64(3);
        let competitor = spawn_competitor(Arc::clone(&book), rendezvous, move |b| {
            b.cancel_order(foreign_ask)
        });

        let taker_outcome =
            book.add_limit_order_with_user(taker_id, 100, 7, Side::Buy, TimeInForce::Gtc, u, None);
        let (observation, cancel_outcome) = competitor.join().expect("competing thread");

        assert_eq!(
            observation,
            GateObservation::Blocked,
            "the STP-active submit must hold the submit gate exclusively; \
             trades recorded={:?}",
            trades_of(&trades)
        );

        let recorded = trades_of(&trades);
        assert_eq!(
            recorded,
            vec![(foreign_ask, taker_id, 5)],
            "exactly one trade, against the foreign maker only"
        );

        match taker_outcome {
            Err(OrderBookError::SelfTradePrevented { .. }) => {}
            other => panic!("the taker must be STP-cancelled, got {other:?}"),
        }
        match book.order_status(taker_id) {
            Some(OrderStatus::Cancelled {
                filled_quantity: 5,
                reason: CancelReason::SelfTradePrevention,
            }) => {}
            other => panic!("taker terminal state must record the 5 real fills, got {other:?}"),
        }

        let Some(resting_self) = book.get_order(self_ask) else {
            panic!("the same-user maker must survive under CancelTaker");
        };
        assert_eq!(
            resting_self.visible_quantity().as_u64(),
            9,
            "the same-user maker was never touched"
        );

        match cancel_outcome {
            Ok(None) => {}
            other => {
                panic!("the foreign maker was already filled, expected Ok(None), got {other:?}")
            }
        }
    }

    // ---------------------------------------------------------------------
    // T3 — CancelBoth vs a concurrent cancel of the foreign maker
    // ---------------------------------------------------------------------

    /// Same window as T2 under `CancelBoth`, which additionally cancels the
    /// same-user maker the verdict identified.
    #[test]
    fn test_stp_cancel_both_concurrent_cancel_of_foreign_maker_never_self_trades() {
        let u = user(1);
        let v = user(2);
        let mut book: OrderBook<()> = OrderBook::new("STP-T3");
        book.set_stp_mode(STPMode::CancelBoth);
        book.set_order_state_tracker(OrderStateTracker::new());
        let trades = install_trade_log(&mut book);

        let foreign_ask = Id::from_u64(1);
        let self_ask = Id::from_u64(2);
        let seeded_foreign = book.add_limit_order_with_user(
            foreign_ask,
            100,
            5,
            Side::Sell,
            TimeInForce::Gtc,
            v,
            None,
        );
        assert!(
            seeded_foreign.is_ok(),
            "seed foreign ask: {seeded_foreign:?}"
        );
        let seeded_self =
            book.add_limit_order_with_user(self_ask, 100, 9, Side::Sell, TimeInForce::Gtc, u, None);
        assert!(seeded_self.is_ok(), "seed same-user ask: {seeded_self:?}");

        let rendezvous = install_interleave_hook(&mut book, 100);
        let book = Arc::new(book);

        let taker_id = Id::from_u64(3);
        let competitor = spawn_competitor(Arc::clone(&book), rendezvous, move |b| {
            b.cancel_order(foreign_ask)
        });

        let taker_outcome =
            book.add_limit_order_with_user(taker_id, 100, 7, Side::Buy, TimeInForce::Gtc, u, None);
        let (observation, cancel_outcome) = competitor.join().expect("competing thread");

        assert_eq!(
            observation,
            GateObservation::Blocked,
            "the STP-active submit must hold the submit gate exclusively; \
             trades recorded={:?}",
            trades_of(&trades)
        );

        let recorded = trades_of(&trades);
        assert_eq!(
            recorded,
            vec![(foreign_ask, taker_id, 5)],
            "exactly one trade, against the foreign maker only"
        );

        match taker_outcome {
            Err(OrderBookError::SelfTradePrevented { .. }) => {}
            other => panic!("the taker must be STP-cancelled, got {other:?}"),
        }
        match book.order_status(taker_id) {
            Some(OrderStatus::Cancelled {
                filled_quantity: 5,
                reason: CancelReason::SelfTradePrevention,
            }) => {}
            other => panic!("taker terminal state must record the 5 real fills, got {other:?}"),
        }

        assert!(
            book.get_order(self_ask).is_none(),
            "CancelBoth removes the same-user maker"
        );
        match book.order_status(self_ask) {
            Some(OrderStatus::Cancelled {
                filled_quantity: 0,
                reason: CancelReason::SelfTradePrevention,
            }) => {}
            other => panic!("the same-user maker must be STP-cancelled, got {other:?}"),
        }

        match cancel_outcome {
            Ok(None) => {}
            other => {
                panic!("the foreign maker was already filled, expected Ok(None), got {other:?}")
            }
        }
    }

    // ---------------------------------------------------------------------
    // T4 — CancelMaker vs a concurrent re-price into the level being swept
    // ---------------------------------------------------------------------

    /// The cancel-then-add modify variants re-admit an order that can match,
    /// so they carry the same window as a fresh submit — from both sides. A
    /// same-user order re-priced into the level a parked taker is about to
    /// sweep must not become that taker's counterparty.
    #[test]
    fn test_stp_cancel_maker_concurrent_reprices_never_self_trade() {
        let u = user(1);
        let v = user(2);
        let mut book: OrderBook<()> = OrderBook::new("STP-T4");
        book.set_stp_mode(STPMode::CancelMaker);
        book.set_order_state_tracker(OrderStateTracker::new());
        let trades = install_trade_log(&mut book);

        let a_id = Id::from_u64(1);
        let v_id = Id::from_u64(2);
        let b_id = Id::from_u64(3);
        let seeded_a =
            book.add_limit_order_with_user(a_id, 90, 5, Side::Buy, TimeInForce::Gtc, u, None);
        assert!(seeded_a.is_ok(), "seed same-user bid: {seeded_a:?}");
        let seeded_v =
            book.add_limit_order_with_user(v_id, 100, 1, Side::Sell, TimeInForce::Gtc, v, None);
        assert!(seeded_v.is_ok(), "seed foreign ask: {seeded_v:?}");
        let seeded_b =
            book.add_limit_order_with_user(b_id, 120, 5, Side::Sell, TimeInForce::Gtc, u, None);
        assert!(seeded_b.is_ok(), "seed same-user ask: {seeded_b:?}");

        let rendezvous = install_interleave_hook(&mut book, 100);
        let book = Arc::new(book);

        let competitor = spawn_competitor(Arc::clone(&book), rendezvous, move |b| {
            b.update_order(OrderUpdate::UpdatePrice {
                order_id: b_id,
                new_price: Price::new(100),
            })
        });

        let a_update = book.update_order(OrderUpdate::UpdatePrice {
            order_id: a_id,
            new_price: Price::new(100),
        });
        let (observation, b_update) = competitor.join().expect("competing thread");

        assert_eq!(
            observation,
            GateObservation::Blocked,
            "an STP-active re-price must hold the submit gate exclusively; \
             trades recorded={:?}",
            trades_of(&trades)
        );

        let recorded = trades_of(&trades);
        assert_eq!(
            recorded,
            vec![(v_id, a_id, 1)],
            "exactly one trade, against the foreign maker only"
        );

        assert!(matches!(a_update, Ok(Some(_))), "A re-priced: {a_update:?}");
        assert!(matches!(b_update, Ok(Some(_))), "B re-priced: {b_update:?}");

        let Some(resting_b) = book.get_order(b_id) else {
            panic!("B must rest at its new price");
        };
        assert_eq!(resting_b.visible_quantity().as_u64(), 5, "B never filled");
        assert_eq!(resting_b.price().as_u128(), 100, "B moved to 100");
        assert_eq!(resting_b.side(), Side::Sell, "B is still an ask");
        assert_eq!(book.best_ask(), Some(100), "B is the only ask");
        assert_eq!(book.best_bid(), None, "A was cancelled, no bids remain");

        assert!(book.get_order(a_id).is_none(), "A no longer rests");
        // `filled_quantity: 1` is the end of a four-step lifecycle, not a
        // single transition: `Open` (seed) -> `Cancelled { 0, UserRequested }`
        // (the re-price's own cancel, `modifications.rs` ~400) -> `Open` /
        // `PartiallyFilled { 5, 1 }` (the re-add, after filling V) ->
        // `Cancelled { 1, SelfTradePrevention }` (B's re-add cancelling A
        // under CancelMaker). It holds only because
        // `OrderStateTracker::transition` is unconditional — it overwrites a
        // terminal state instead of refusing the `Cancelled -> Open` step —
        // so `cancel_resting_maker_on_level` can read the prior
        // `filled_quantity` back out. A lifecycle cleanup that makes
        // terminal states sticky, or that stops the re-price from recording
        // an intermediate `Cancelled`, must update this assertion.
        match book.order_status(a_id) {
            Some(OrderStatus::Cancelled {
                filled_quantity: 1,
                reason: CancelReason::SelfTradePrevention,
            }) => {}
            other => panic!("A must be STP-cancelled keeping its one fill, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------
    // T7 / T8 — the market entry points (#225, review finding F4)
    // ---------------------------------------------------------------------
    //
    // T2 covers `add_order`, whose gate lives in `modifications.rs`. The two
    // market paths reach the same STP window through different gate
    // acquisitions: `submit_market_order_with_user` inherits the one in
    // `match_order_with_user` (`matching.rs`), and
    // `submit_market_order_by_amount_with_user` takes its own in `book.rs`.
    // Each is exercised separately so neither call site can regress unseen.
    //
    // Both differ from T2 in what they *return*: `match_order_inner` only
    // converts the STP taker-cancel into `Err(SelfTradePrevented)` when no
    // fills happened at all, and the `Err` conversion after partial fills
    // lives in `add_order_inner`, which the market paths do not use. A
    // partially-filled, STP-cancelled market taker therefore comes back as
    // `Ok(MatchResult)` and is never recorded by the order state tracker
    // (market orders are not tracked at all). These tests assert that shape
    // literally rather than mirroring the limit path.

    /// Seed the shared T2 fixture: a foreign ask of 5 and a same-user ask of
    /// 9, both at 100, in that queue order. Returns `(foreign_id, self_id)`.
    fn seed_foreign_then_self_asks(book: &OrderBook<()>, u: Hash32, v: Hash32) -> (Id, Id) {
        let foreign_ask = Id::from_u64(1);
        let self_ask = Id::from_u64(2);
        let seeded_foreign = book.add_limit_order_with_user(
            foreign_ask,
            100,
            5,
            Side::Sell,
            TimeInForce::Gtc,
            v,
            None,
        );
        assert!(
            seeded_foreign.is_ok(),
            "seed foreign ask: {seeded_foreign:?}"
        );
        let seeded_self =
            book.add_limit_order_with_user(self_ask, 100, 9, Side::Sell, TimeInForce::Gtc, u, None);
        assert!(seeded_self.is_ok(), "seed same-user ask: {seeded_self:?}");
        (foreign_ask, self_ask)
    }

    /// `submit_market_order_with_user` reaches the STP window through the
    /// gate in `match_order_with_user`. Cancelling the foreign maker inside
    /// that window must not let the stale `safe_quantity` be spent on the
    /// same-user maker behind it.
    #[test]
    fn test_stp_cancel_taker_market_by_quantity_concurrent_cancel_never_self_trades() {
        let u = user(1);
        let v = user(2);
        let mut book: OrderBook<()> = OrderBook::new("STP-T7");
        book.set_stp_mode(STPMode::CancelTaker);
        book.set_order_state_tracker(OrderStateTracker::new());
        let trades = install_trade_log(&mut book);
        let (foreign_ask, self_ask) = seed_foreign_then_self_asks(&book, u, v);

        let rendezvous = install_interleave_hook(&mut book, 100);
        let book = Arc::new(book);

        let taker_id = Id::from_u64(3);
        let competitor = spawn_competitor(Arc::clone(&book), rendezvous, move |b| {
            b.cancel_order(foreign_ask)
        });

        let taker_outcome = book.submit_market_order_with_user(taker_id, 7, Side::Buy, u);
        let (observation, cancel_outcome) = competitor.join().expect("competing thread");

        assert_eq!(
            observation,
            GateObservation::Blocked,
            "an STP-active market sweep must hold the submit gate exclusively; \
             trades recorded={:?}",
            trades_of(&trades)
        );

        assert_eq!(
            trades_of(&trades),
            vec![(foreign_ask, taker_id, 5)],
            "exactly one trade, against the foreign maker only"
        );

        // The market paths discard `taker_stp_cancelled`: the `Err`
        // conversion after partial fills lives in `add_order_inner`, which
        // they do not use, so an STP-cancelled taker that already filled
        // comes back as `Ok` with no indication that STP stopped it. Here
        // that happens to surface as a short fill (5 of 7), but that is a
        // reporting gap rather than a deliberate contract, so it is not
        // asserted. The STP evidence is the trade list, the untouched
        // same-user maker, the competitor's `Ok(None)` and the `Blocked`
        // observation.
        let Ok(result) = taker_outcome else {
            panic!("a partially filled market taker returns Ok: {taker_outcome:?}");
        };
        assert_eq!(result.trades().as_vec().len(), 1, "one trade in the result");
        assert_eq!(
            result.filled_order_ids(),
            [foreign_ask],
            "only the foreign maker was filled"
        );
        assert_eq!(
            book.order_status(taker_id),
            None,
            "market takers are never registered with the order state tracker"
        );

        let Some(resting_self) = book.get_order(self_ask) else {
            panic!("the same-user maker must survive under CancelTaker");
        };
        assert_eq!(
            resting_self.visible_quantity().as_u64(),
            9,
            "the same-user maker was never touched"
        );

        match cancel_outcome {
            Ok(None) => {}
            other => {
                panic!("the foreign maker was already filled, expected Ok(None), got {other:?}")
            }
        }
    }

    /// `submit_market_order_by_amount_with_user` reaches the STP window
    /// through its own gate in `book.rs`. Budget 700 ticks at a price of 100
    /// is a 7-unit sweep, i.e. the same shape as T7.
    #[test]
    fn test_stp_cancel_taker_market_by_amount_concurrent_cancel_never_self_trades() {
        let u = user(1);
        let v = user(2);
        let mut book: OrderBook<()> = OrderBook::new("STP-T8");
        book.set_stp_mode(STPMode::CancelTaker);
        book.set_order_state_tracker(OrderStateTracker::new());
        let trades = install_trade_log(&mut book);
        let (foreign_ask, self_ask) = seed_foreign_then_self_asks(&book, u, v);

        let rendezvous = install_interleave_hook(&mut book, 100);
        let book = Arc::new(book);

        let taker_id = Id::from_u64(3);
        let competitor = spawn_competitor(Arc::clone(&book), rendezvous, move |b| {
            b.cancel_order(foreign_ask)
        });

        let taker_outcome =
            book.submit_market_order_by_amount_with_user(taker_id, 700, Side::Buy, u);
        let (observation, cancel_outcome) = competitor.join().expect("competing thread");

        assert_eq!(
            observation,
            GateObservation::Blocked,
            "an STP-active notional sweep must hold the submit gate exclusively; \
             trades recorded={:?}",
            trades_of(&trades)
        );

        assert_eq!(
            trades_of(&trades),
            vec![(foreign_ask, taker_id, 5)],
            "exactly one trade, against the foreign maker only"
        );

        // Like T7 the notional path discards `taker_stp_cancelled`, and it
        // hides the short fill on top: its internal sweep runs on a
        // `u64::MAX` base-quantity budget, so
        // `normalize_notional_match_result` rebuilds the public result with
        // `requested == executed` — `remaining_quantity` is 0 and the result
        // reports complete, even though 200 of the 700 ticks of budget were
        // never spent. Both are pre-existing reporting gaps rather than
        // desired behaviour, so neither is asserted here; the STP evidence
        // is the trade list, the untouched same-user maker, the
        // competitor's `Ok(None)` and the `Blocked` observation.
        let Ok(result) = taker_outcome else {
            panic!("a partially filled notional taker returns Ok: {taker_outcome:?}");
        };
        assert_eq!(result.trades().as_vec().len(), 1, "one trade in the result");
        assert_eq!(
            result.filled_order_ids(),
            [foreign_ask],
            "only the foreign maker was filled"
        );
        assert_eq!(
            book.order_status(taker_id),
            None,
            "market takers are never registered with the order state tracker"
        );

        let Some(resting_self) = book.get_order(self_ask) else {
            panic!("the same-user maker must survive under CancelTaker");
        };
        assert_eq!(
            resting_self.visible_quantity().as_u64(),
            9,
            "the same-user maker was never touched"
        );

        match cancel_outcome {
            Ok(None) => {}
            other => {
                panic!("the foreign maker was already filled, expected Ok(None), got {other:?}")
            }
        }
    }

    // ---------------------------------------------------------------------
    // T5 — gate-mode decision helpers
    // ---------------------------------------------------------------------

    #[test]
    fn test_submit_needs_exclusive_gate_selects_mode_per_stp_and_tif() {
        let u = user(1);

        let plain: OrderBook<()> = OrderBook::new("GATE-NONE");
        assert!(
            !plain.submit_needs_exclusive_gate(false, u, false),
            "STPMode::None keeps the shared path even with a real user"
        );
        assert!(
            !plain.submit_needs_exclusive_gate(false, Hash32::zero(), false),
            "STPMode::None with an anonymous taker stays shared"
        );
        assert!(
            !plain.submit_needs_exclusive_gate(false, u, true),
            "STPMode::None with a post-only taker stays shared"
        );
        assert!(
            plain.submit_needs_exclusive_gate(true, Hash32::zero(), false),
            "fill-or-kill is exclusive regardless of STP (#209)"
        );
        assert!(
            plain.submit_needs_exclusive_gate(true, u, false),
            "fill-or-kill is exclusive regardless of user"
        );

        for mode in [
            STPMode::CancelTaker,
            STPMode::CancelMaker,
            STPMode::CancelBoth,
        ] {
            let mut book: OrderBook<()> = OrderBook::new("GATE-STP");
            book.set_stp_mode(mode);
            assert!(
                !book.submit_needs_exclusive_gate(false, Hash32::zero(), false),
                "{mode}: an anonymous taker skips STP, so it stays shared"
            );
            assert!(
                book.submit_needs_exclusive_gate(false, u, false),
                "{mode}: an STP-relevant submit takes the exclusive gate"
            );
            assert!(
                book.submit_needs_exclusive_gate(true, u, false),
                "{mode}: fill-or-kill stays exclusive"
            );
            // A post-only taker resolves before the STP scan is reached, so
            // it carries no check-then-act window of its own and keeps the
            // shared side even on an STP book with a real identity (#225,
            // review finding F3).
            assert!(
                !book.submit_needs_exclusive_gate(false, u, true),
                "{mode}: a post-only taker never reaches the STP scan"
            );
            assert!(
                !book.submit_needs_exclusive_gate(false, Hash32::zero(), true),
                "{mode}: an anonymous post-only taker stays shared"
            );
            // Not representable today — `OrderType::PostOnly` carries no
            // `TimeInForce`, so a post-only can never be fill-or-kill — but
            // if it ever becomes representable the #209 all-or-nothing
            // window must still win over the post-only exemption.
            assert!(
                book.submit_needs_exclusive_gate(true, u, true),
                "{mode}: fill-or-kill outranks the post-only exemption"
            );
        }
    }

    /// The post-only exemption is a property of the *submitted order*, not
    /// of the caller: `add_order` reads it off the order itself, so a
    /// post-only submit on an STP book must be observably non-blocking while
    /// an identified limit submit on the same book blocks.
    #[test]
    fn test_add_order_post_only_keeps_shared_gate_under_stp() {
        let u = user(1);
        let mut book: OrderBook<()> = OrderBook::new("GATE-PO");
        book.set_stp_mode(STPMode::CancelMaker);
        let book = Arc::new(book);

        // Hold the shared side from this thread: a competing *shared*
        // acquisition succeeds, a competing *exclusive* one would block.
        let held = book.submit_gate.try_read();
        assert!(held.is_ok(), "the gate starts uncontended");

        let po_book = Arc::clone(&book);
        let po = thread::spawn(move || {
            po_book.add_post_only_order_with_user(
                Id::from_u64(1),
                100,
                5,
                Side::Buy,
                TimeInForce::Gtc,
                u,
                None,
            )
        });
        let po_outcome = po.join().expect("post-only thread");
        assert!(
            po_outcome.is_ok(),
            "a post-only submit must not block behind a shared reader: {po_outcome:?}"
        );

        drop(held);
        assert_eq!(
            book.best_bid(),
            Some(100),
            "the post-only rested while the shared side was held"
        );
    }

    #[test]
    fn test_modify_needs_exclusive_gate_selects_mode_per_stp_and_variant() {
        let order_id = Id::from_u64(1);
        let variants = [
            (
                OrderUpdate::UpdatePrice {
                    order_id,
                    new_price: Price::new(100),
                },
                true,
            ),
            (
                OrderUpdate::UpdatePriceAndQuantity {
                    order_id,
                    new_price: Price::new(100),
                    new_quantity: Quantity::new(5),
                },
                true,
            ),
            (
                OrderUpdate::Replace {
                    order_id,
                    price: Price::new(100),
                    quantity: Quantity::new(5),
                    side: Side::Buy,
                },
                true,
            ),
            (
                OrderUpdate::UpdateQuantity {
                    order_id,
                    new_quantity: Quantity::new(5),
                },
                false,
            ),
            (OrderUpdate::Cancel { order_id }, false),
        ];

        let plain: OrderBook<()> = OrderBook::new("MODGATE-NONE");
        for (update, _) in &variants {
            assert!(
                !plain.modify_needs_exclusive_gate(update),
                "STPMode::None keeps every modify variant on the shared path: {update:?}"
            );
        }

        for mode in [
            STPMode::CancelTaker,
            STPMode::CancelMaker,
            STPMode::CancelBoth,
        ] {
            let mut book: OrderBook<()> = OrderBook::new("MODGATE-STP");
            book.set_stp_mode(mode);
            for (update, expected) in &variants {
                assert_eq!(
                    book.modify_needs_exclusive_gate(update),
                    *expected,
                    "{mode}: unexpected gate mode for {update:?}"
                );
            }
        }
    }

    // ---------------------------------------------------------------------
    // T6 — randomized-interleaving stress
    // ---------------------------------------------------------------------

    /// Stress the #218 scenario: two threads released by a barrier submit a
    /// same-user ask and a same-user post-only at the same price under
    /// `CancelMaker`. No interleaving may produce a trade.
    ///
    /// Long-running and excluded from the default test run. Execute with:
    ///
    /// ```text
    /// cargo test --lib stress_stp -- --ignored
    /// ```
    #[test]
    #[ignore = "60k-round stress test; run explicitly with --ignored"]
    fn stress_stp_same_user_admission_60k_rounds() {
        use std::sync::Barrier;

        const ROUNDS: u64 = 60_000;
        let u = user(7);

        for round in 0..ROUNDS {
            let mut book: OrderBook<()> = OrderBook::new("STP-STRESS");
            book.set_stp_mode(STPMode::CancelMaker);
            let trades = install_trade_log(&mut book);
            let book = Arc::new(book);

            let ask_id = Id::from_u64(1_000_000 + round);
            let post_only_id = Id::from_u64(1 + round);
            let barrier = Arc::new(Barrier::new(2));

            let ask_book = Arc::clone(&book);
            let ask_barrier = Arc::clone(&barrier);
            let ask_thread = thread::spawn(move || {
                ask_barrier.wait();
                ask_book.add_limit_order_with_user(
                    ask_id,
                    100,
                    5,
                    Side::Sell,
                    TimeInForce::Gtc,
                    u,
                    None,
                )
            });

            let post_only_book = Arc::clone(&book);
            let post_only_barrier = Arc::clone(&barrier);
            let post_only_thread = thread::spawn(move || {
                post_only_barrier.wait();
                post_only_book.add_post_only_order_with_user(
                    post_only_id,
                    100,
                    5,
                    Side::Buy,
                    TimeInForce::Gtc,
                    u,
                    None,
                )
            });

            let ask_outcome = ask_thread.join().expect("ask thread");
            let post_only_outcome = post_only_thread.join().expect("post-only thread");

            let recorded = trades_of(&trades);
            assert!(
                recorded.is_empty(),
                "round {round}: same-user ask vs same-user post-only must never trade \
                 (trades={recorded:?}, ask_ok={}, post_only_ok={})",
                ask_outcome.is_ok(),
                post_only_outcome.is_ok()
            );
        }
    }
}
