/******************************************************************************
   A submit that traded and then returned `Err` must replay.

   `add_order` emits real fills before failing on an unfilled IOC remainder
   or an STP-cancelled taker, so a sequencer records those commands as
   rejections over a book they already mutated. Replay used to skip every
   rejected event — resurrecting the consumed liquidity — and to abort on
   any `Err` from a submit it did apply. It now decides by the recorded
   reject code: a `RejectedWithCode` submit whose code replay can reproduce
   is re-executed and must fail the same way again, a code replay cannot
   reproduce (the kill switch) is skipped, and the string-only `Rejected`
   keeps the historical skip. Each test builds its journal the way a
   sequencer does: run the command against a live book, record the outcome
   the command API returned.
******************************************************************************/

use orderbook_rs::orderbook::sequencer::{
    InMemoryJournal, Journal, ReplayBookConfig, ReplayEngine, ReplayError, SequencerCommand,
    SequencerEvent, SequencerResult, snapshots_match,
};
use orderbook_rs::{Clock, OrderBook, OrderBookError, RejectReason, STPMode, StubClock};
use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
use std::cell::RefCell;
use std::sync::Arc;

const SYMBOL: &str = "RSE";

fn stub_clock() -> Arc<dyn Clock> {
    Arc::new(StubClock::starting_at(0))
}

fn user(byte: u8) -> Hash32 {
    Hash32::new([byte; 32])
}

fn order(
    id: u64,
    price: u128,
    qty: u64,
    side: Side,
    tif: TimeInForce,
    user_id: Hash32,
) -> OrderType<()> {
    OrderType::Standard {
        id: Id::from_u64(id),
        price: Price::new(price),
        quantity: Quantity::new(qty),
        side,
        time_in_force: tif,
        user_id,
        timestamp: TimestampMs::new(0),
        extra_fields: (),
    }
}

fn append(
    journal: &InMemoryJournal<()>,
    seq: u64,
    command: SequencerCommand<()>,
    result: SequencerResult,
) {
    journal
        .append(&SequencerEvent {
            sequence_num: seq,
            timestamp_ns: seq,
            command,
            result,
        })
        .expect("journal append");
}

/// A minimal sequencer: execute against the live book, journal the command
/// with the outcome the command API returned — `OrderAdded` on `Ok`,
/// `RejectedWithCode` on `Err` via the `From<&OrderBookError>` impl.
fn sequence(
    live: &OrderBook<()>,
    journal: &InMemoryJournal<()>,
    seq: u64,
    order: OrderType<()>,
) -> Result<(), OrderBookError> {
    let id = order.id();
    let outcome = live.add_order(order);
    let result = match &outcome {
        Ok(_) => SequencerResult::OrderAdded { order_id: id },
        Err(e) => SequencerResult::from(e),
    };
    append(journal, seq, SequencerCommand::AddOrder(order), result);
    outcome.map(|_| ())
}

fn replay(
    journal: &InMemoryJournal<()>,
    config: &ReplayBookConfig,
) -> Result<(OrderBook<()>, u64), ReplayError> {
    ReplayEngine::<()>::replay_from_with_clock_and_config(journal, 0, SYMBOL, stub_clock(), config)
}

/// Structural equality plus the last trade price, which `snapshots_match`
/// cannot see once a level has been emptied: a maker that was traded and a
/// maker that was cancelled leave the same (absent) level behind.
fn assert_books_match(replayed: &OrderBook<()>, live: &OrderBook<()>) {
    assert!(
        snapshots_match(
            &replayed.create_snapshot(usize::MAX),
            &live.create_snapshot(usize::MAX)
        ),
        "replayed book diverged from the live book"
    );
    assert_eq!(
        replayed.last_trade_price(),
        live.last_trade_price(),
        "the replayed book traded differently from the live book"
    );
}

fn code_of(result: &SequencerResult) -> Option<RejectReason> {
    match result {
        SequencerResult::RejectedWithCode { code, .. } => Some(*code),
        _ => None,
    }
}

/// An IOC that consumes the whole book and then reports its unfillable
/// remainder: the fills happened, so replay must consume them too, and the
/// re-executed rejection counts as an applied event.
#[test]
fn ioc_remainder_error_replays_its_fills() {
    let live = OrderBook::<()>::with_clock(SYMBOL, stub_clock());
    let journal: InMemoryJournal<()> = InMemoryJournal::new();

    sequence(
        &live,
        &journal,
        0,
        order(1, 100, 10, Side::Sell, TimeInForce::Gtc, Hash32::zero()),
    )
    .expect("seed ask");
    let err = sequence(
        &live,
        &journal,
        1,
        order(2, 100, 15, Side::Buy, TimeInForce::Ioc, Hash32::zero()),
    )
    .expect_err("the IOC remainder is unfillable");
    assert!(
        matches!(err, OrderBookError::InsufficientLiquidity { .. }),
        "expected InsufficientLiquidity, got {err:?}"
    );
    assert_eq!(live.best_ask(), None, "the live ask was consumed");

    let (replayed, last_applied) =
        replay(&journal, &ReplayBookConfig::default()).expect("replay succeeds");
    assert_eq!(
        replayed.best_ask(),
        None,
        "the replayed ask was consumed too"
    );
    assert_books_match(&replayed, &live);
    assert_eq!(last_applied, 1, "the re-executed rejection was applied");
}

/// A taker that fills against another user and is then cancelled by STP:
/// the non-self fills happened, so replay must consume them too.
#[test]
fn stp_cancelled_taker_replays_its_non_self_fills() {
    let mut live = OrderBook::<()>::with_clock(SYMBOL, stub_clock());
    live.set_stp_mode(STPMode::CancelTaker);
    let journal: InMemoryJournal<()> = InMemoryJournal::new();

    sequence(
        &live,
        &journal,
        0,
        order(1, 100, 5, Side::Sell, TimeInForce::Gtc, user(2)),
    )
    .expect("seed non-self maker");
    sequence(
        &live,
        &journal,
        1,
        order(2, 100, 9, Side::Sell, TimeInForce::Gtc, user(1)),
    )
    .expect("seed same-user maker");
    let err = sequence(
        &live,
        &journal,
        2,
        order(3, 100, 9, Side::Buy, TimeInForce::Gtc, user(1)),
    )
    .expect_err("the taker reaches its own maker");
    assert!(
        matches!(err, OrderBookError::SelfTradePrevented { .. }),
        "expected SelfTradePrevented, got {err:?}"
    );
    assert!(
        live.get_order(Id::from_u64(1)).is_none(),
        "the non-self maker was consumed live"
    );

    let config = ReplayBookConfig::new(None, STPMode::CancelTaker, None, None, None, None);
    let (replayed, last_applied) = replay(&journal, &config).expect("replay succeeds");
    assert!(
        replayed.get_order(Id::from_u64(1)).is_none(),
        "the non-self maker was consumed on replay too"
    );
    assert_eq!(
        replayed
            .get_order(Id::from_u64(2))
            .expect("the same-user maker still rests")
            .visible_quantity()
            .as_u64(),
        9
    );
    assert_books_match(&replayed, &live);
    assert_eq!(last_applied, 2, "the re-executed rejection was applied");
}

/// The market commands take the same path. Without a user id they cannot
/// carry STP effects, so their only rejection is the no-fill one, which
/// re-executes to the same no-op and counts as applied.
#[test]
fn market_rejections_without_fills_replay_as_the_same_no_op() {
    let live = OrderBook::<()>::with_clock(SYMBOL, stub_clock());
    let journal: InMemoryJournal<()> = InMemoryJournal::new();

    let err = live
        .submit_market_order(Id::from_u64(1), 5, Side::Buy)
        .expect_err("empty book");
    assert!(matches!(err, OrderBookError::InsufficientLiquidity { .. }));
    append(
        &journal,
        0,
        SequencerCommand::MarketOrder {
            id: Id::from_u64(1),
            quantity: 5,
            side: Side::Buy,
        },
        SequencerResult::from(&err),
    );

    let err = live
        .submit_market_order_by_amount(Id::from_u64(2), 500, Side::Buy)
        .expect_err("empty book");
    assert!(matches!(
        err,
        OrderBookError::InsufficientLiquidityNotional { .. }
    ));
    append(
        &journal,
        1,
        SequencerCommand::MarketOrderByAmount {
            id: Id::from_u64(2),
            amount: 500,
            side: Side::Buy,
        },
        SequencerResult::from(&err),
    );

    let (replayed, last_applied) =
        replay(&journal, &ReplayBookConfig::default()).expect("replay succeeds");
    assert_books_match(&replayed, &live);
    assert_eq!(last_applied, 1, "both market rejections were re-executed");
}

/// A kill-switch rejection never touches the book and its trigger is not
/// part of `ReplayBookConfig`, so replay must skip it rather than re-execute
/// it: re-executing would consume the ask the live book never touched.
/// The skip does not advance the applied sequence or the progress callback.
#[test]
fn kill_switch_rejection_is_skipped_and_the_books_match() {
    let live = OrderBook::<()>::with_clock(SYMBOL, stub_clock());
    let journal: InMemoryJournal<()> = InMemoryJournal::new();

    sequence(
        &live,
        &journal,
        0,
        order(1, 100, 10, Side::Sell, TimeInForce::Gtc, Hash32::zero()),
    )
    .expect("seed ask");
    live.engage_kill_switch();
    let err = sequence(
        &live,
        &journal,
        1,
        order(2, 100, 15, Side::Buy, TimeInForce::Ioc, Hash32::zero()),
    )
    .expect_err("halted");
    assert!(matches!(err, OrderBookError::KillSwitchActive));
    assert_eq!(live.best_ask(), Some(100), "the halted IOC touched nothing");

    let progress: RefCell<Vec<(u64, u64)>> = RefCell::new(Vec::new());
    let (replayed, last_applied) = ReplayEngine::<()>::replay_from_with_clock_and_progress(
        &journal,
        0,
        SYMBOL,
        stub_clock(),
        |count, seq| progress.borrow_mut().push((count, seq)),
    )
    .expect("replay succeeds");

    assert_eq!(replayed.best_ask(), Some(100), "the ask survived replay");
    assert_books_match(&replayed, &live);
    assert_eq!(
        last_applied, 0,
        "the skipped rejection did not advance the applied sequence"
    );
    assert_eq!(
        progress.into_inner(),
        vec![(1, 0)],
        "the progress callback saw only the dispatched event"
    );
}

/// A pure admission rejection replay can reproduce (a tick violation, with
/// the tick carried in the config) re-executes to the same error, leaves
/// the book untouched, and counts as applied.
#[test]
fn tick_rejection_with_a_matching_config_replays_as_the_same_no_op() {
    let mut live = OrderBook::<()>::with_clock(SYMBOL, stub_clock());
    live.set_tick_size(10);
    let journal: InMemoryJournal<()> = InMemoryJournal::new();

    sequence(
        &live,
        &journal,
        0,
        order(1, 100, 10, Side::Sell, TimeInForce::Gtc, Hash32::zero()),
    )
    .expect("a tick-aligned order is admitted");
    let err = sequence(
        &live,
        &journal,
        1,
        order(2, 105, 10, Side::Sell, TimeInForce::Gtc, Hash32::zero()),
    )
    .expect_err("105 is not a multiple of the tick");
    assert!(
        matches!(err, OrderBookError::InvalidTickSize { .. }),
        "expected InvalidTickSize, got {err:?}"
    );

    let config = ReplayBookConfig::new(None, STPMode::None, Some(10), None, None, None);
    let (replayed, last_applied) = replay(&journal, &config).expect("replay succeeds");
    assert!(
        replayed.get_order(Id::from_u64(2)).is_none(),
        "the tick-rejected order did not rest on replay"
    );
    assert_books_match(&replayed, &live);
    assert_eq!(last_applied, 1, "the re-executed rejection was applied");
}

/// The same journal replayed without the tick in its config: the rejected
/// order now rests, which is a divergence, and replay says so instead of
/// returning a book that quietly differs from the live one.
#[test]
fn rejected_submit_that_succeeds_on_replay_aborts() {
    let mut live = OrderBook::<()>::with_clock(SYMBOL, stub_clock());
    live.set_tick_size(10);
    let journal: InMemoryJournal<()> = InMemoryJournal::new();

    sequence(
        &live,
        &journal,
        0,
        order(1, 100, 10, Side::Sell, TimeInForce::Gtc, Hash32::zero()),
    )
    .expect("seed ask");
    sequence(
        &live,
        &journal,
        1,
        order(2, 105, 10, Side::Sell, TimeInForce::Gtc, Hash32::zero()),
    )
    .expect_err("105 is not a multiple of the tick");

    let err = replay(&journal, &ReplayBookConfig::default())
        .err()
        .expect("a rejected submit that rests on replay is a divergence");
    match err {
        ReplayError::OutcomeMismatch {
            sequence_num,
            recorded,
            actual,
        } => {
            assert_eq!(sequence_num, 1);
            assert_eq!(recorded, RejectReason::InvalidPrice);
            assert!(actual.is_none(), "replay succeeded where live rejected");
        }
        other => panic!("expected OutcomeMismatch, got {other:?}"),
    }
}

/// A re-execution that fails under a different code is just as much a
/// divergence as one that succeeds.
#[test]
fn rejected_submit_that_fails_differently_on_replay_aborts() {
    let journal: InMemoryJournal<()> = InMemoryJournal::new();
    let ask = order(1, 100, 10, Side::Sell, TimeInForce::Gtc, Hash32::zero());
    append(
        &journal,
        0,
        SequencerCommand::AddOrder(ask),
        SequencerResult::OrderAdded {
            order_id: Id::from_u64(1),
        },
    );
    // Journaled as a liquidity rejection, but the re-add of an id that
    // already rests is a duplicate-id rejection on replay.
    append(
        &journal,
        1,
        SequencerCommand::AddOrder(ask),
        SequencerResult::RejectedWithCode {
            reason: "insufficient liquidity".to_string(),
            code: RejectReason::InsufficientLiquidity,
        },
    );

    let err = replay(&journal, &ReplayBookConfig::default())
        .err()
        .expect("a different verdict is a divergence");
    match &err {
        ReplayError::OutcomeMismatch {
            sequence_num,
            recorded,
            actual,
        } => {
            assert_eq!(*sequence_num, 1);
            assert_eq!(*recorded, RejectReason::InsufficientLiquidity);
            assert!(
                matches!(actual, Some(OrderBookError::DuplicateOrderId { .. })),
                "expected the duplicate-id error, got {actual:?}"
            );
        }
        other => panic!("expected OutcomeMismatch, got {other:?}"),
    }
    let text = err.to_string();
    assert!(
        text.contains("sequence 1")
            && text.contains("insufficient liquidity")
            && text.contains("duplicate order id"),
        "the message names the sequence and both verdicts: {text}"
    );
}

/// A string-only `Rejected` carries no code to decide by, so it keeps the
/// historical skip — including, for a submit that traded first, the
/// pre-existing gap. This pins that such journals still replay rather than
/// abort, and that closing the gap needs `RejectedWithCode`.
#[test]
fn legacy_string_rejection_keeps_the_skip() {
    let live = OrderBook::<()>::with_clock(SYMBOL, stub_clock());
    let journal: InMemoryJournal<()> = InMemoryJournal::new();

    sequence(
        &live,
        &journal,
        0,
        order(1, 100, 10, Side::Sell, TimeInForce::Gtc, Hash32::zero()),
    )
    .expect("seed ask");
    let ioc = order(2, 100, 15, Side::Buy, TimeInForce::Ioc, Hash32::zero());
    let err = live
        .add_order(ioc)
        .expect_err("the IOC remainder is unfillable");
    append(
        &journal,
        1,
        SequencerCommand::AddOrder(ioc),
        SequencerResult::Rejected {
            reason: err.to_string(),
        },
    );
    assert_eq!(live.best_ask(), None, "the live ask was consumed");

    let (replayed, last_applied) =
        replay(&journal, &ReplayBookConfig::default()).expect("a legacy rejection still replays");
    assert_eq!(last_applied, 0, "the string-only rejection was skipped");
    assert_eq!(
        replayed.best_ask(),
        Some(100),
        "without a code the traded IOC is skipped and the ask is rebuilt"
    );
}

/// A journal is expected to record the outcome the command API returned.
/// A success recorded for a submit that returned `Err` replays as a
/// success/failure disagreement and aborts, as it did before.
#[test]
fn success_journaled_for_a_failed_submit_aborts() {
    let live = OrderBook::<()>::with_clock(SYMBOL, stub_clock());
    let journal: InMemoryJournal<()> = InMemoryJournal::new();

    sequence(
        &live,
        &journal,
        0,
        order(1, 100, 10, Side::Sell, TimeInForce::Gtc, Hash32::zero()),
    )
    .expect("seed ask");
    let ioc = order(2, 100, 15, Side::Buy, TimeInForce::Ioc, Hash32::zero());
    live.add_order(ioc)
        .expect_err("the IOC remainder is unfillable");
    append(
        &journal,
        1,
        SequencerCommand::AddOrder(ioc),
        SequencerResult::OrderAdded {
            order_id: Id::from_u64(2),
        },
    );

    let err = replay(&journal, &ReplayBookConfig::default())
        .err()
        .expect("a success recorded for a failed submit must abort");
    assert!(
        matches!(
            err,
            ReplayError::OrderBookError {
                sequence_num: 1,
                source: OrderBookError::InsufficientLiquidity { .. },
            }
        ),
        "expected an aborting OrderBookError, got {err:?}"
    );
}

/// An error replay did not expect on a journaled success is still a hard
/// failure.
#[test]
fn unexpected_submit_error_aborts_replay() {
    let journal: InMemoryJournal<()> = InMemoryJournal::new();
    for seq in 0..2 {
        append(
            &journal,
            seq,
            SequencerCommand::AddOrder(order(
                1,
                100,
                10,
                Side::Sell,
                TimeInForce::Gtc,
                Hash32::zero(),
            )),
            SequencerResult::OrderAdded {
                order_id: Id::from_u64(1),
            },
        );
    }

    let err = ReplayEngine::<()>::replay_from(&journal, 0, SYMBOL)
        .err()
        .expect("the duplicate id must abort replay");
    assert!(
        matches!(
            err,
            ReplayError::OrderBookError {
                sequence_num: 1,
                ..
            }
        ),
        "expected an aborting OrderBookError, got {err:?}"
    );
}

/// A re-executed rejection is an applied event: it advances the applied
/// count, the last applied sequence and the progress callback, exactly like
/// the successful command before it.
#[test]
fn re_executed_rejection_advances_the_applied_sequence_and_progress() {
    let live = OrderBook::<()>::with_clock(SYMBOL, stub_clock());
    let journal: InMemoryJournal<()> = InMemoryJournal::new();

    sequence(
        &live,
        &journal,
        0,
        order(1, 100, 10, Side::Sell, TimeInForce::Gtc, Hash32::zero()),
    )
    .expect("seed ask");
    sequence(
        &live,
        &journal,
        1,
        order(2, 100, 15, Side::Buy, TimeInForce::Ioc, Hash32::zero()),
    )
    .expect_err("the IOC remainder is unfillable");

    let progress: RefCell<Vec<(u64, u64)>> = RefCell::new(Vec::new());
    let (replayed, last_applied) = ReplayEngine::<()>::replay_from_with_clock_and_progress(
        &journal,
        0,
        SYMBOL,
        stub_clock(),
        |count, seq| progress.borrow_mut().push((count, seq)),
    )
    .expect("replay succeeds");

    assert_books_match(&replayed, &live);
    assert_eq!(last_applied, 1);
    assert_eq!(progress.into_inner(), vec![(1, 0), (2, 1)]);
}

/// `From<&OrderBookError>` fills both fields, and the code travels as its
/// stable `u16` wire value.
#[test]
fn rejected_with_code_carries_the_wire_code() {
    let err = OrderBookError::InsufficientLiquidity {
        side: Side::Buy,
        requested: 15,
        available: 10,
    };
    let result = SequencerResult::from(&err);
    match &result {
        SequencerResult::RejectedWithCode { reason, code } => {
            assert_eq!(reason, &err.to_string());
            assert_eq!(*code, RejectReason::InsufficientLiquidity);
        }
        other => panic!("expected RejectedWithCode, got {other:?}"),
    }

    let json = serde_json::to_string(&result).expect("serialize");
    assert!(
        json.contains("\"code\":13"),
        "the code encodes as its u16 wire value: {json}"
    );
    let decoded: SequencerResult = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(code_of(&decoded), Some(RejectReason::InsufficientLiquidity));

    let event = SequencerEvent {
        sequence_num: 7,
        timestamp_ns: 7,
        command: SequencerCommand::<()>::CancelAll,
        result: SequencerResult::from(&OrderBookError::KillSwitchActive),
    };
    let json = serde_json::to_vec(&event).expect("serialize event");
    let decoded: SequencerEvent<()> = serde_json::from_slice(&json).expect("deserialize event");
    assert_eq!(decoded.sequence_num, 7);
    assert_eq!(
        code_of(&decoded.result),
        Some(RejectReason::KillSwitchActive)
    );
}
