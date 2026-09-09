/******************************************************************************
   A submit that traded and then returned `Err` must replay.

   `add_order` emits real fills before failing on an unfilled IOC remainder
   or an STP-cancelled taker, so a sequencer records those commands as
   `Rejected` over a book they already mutated. Replay used to skip them —
   resurrecting the consumed liquidity — and to abort on any `Err` from a
   submit it did apply. Each test builds the journal the way a sequencer
   does: run the command against a live book, record the outcome it saw.
******************************************************************************/

use orderbook_rs::orderbook::sequencer::{
    InMemoryJournal, Journal, ReplayBookConfig, ReplayEngine, ReplayError, SequencerCommand,
    SequencerEvent, SequencerResult, snapshots_match,
};
use orderbook_rs::{Clock, OrderBook, OrderBookError, STPMode, StubClock};
use pricelevel::{Hash32, Id, OrderType, Price, Quantity, Side, TimeInForce, TimestampMs};
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

/// A minimal sequencer: execute against the live book, journal the command
/// with the result the caller observed. `Err` becomes `Rejected` — the
/// classification the crate's own `Rejected` skip was reading.
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
        Err(e) => SequencerResult::Rejected {
            reason: e.to_string(),
        },
    };
    journal
        .append(&SequencerEvent {
            sequence_num: seq,
            timestamp_ns: seq,
            command: SequencerCommand::AddOrder(order),
            result,
        })
        .expect("journal append");
    outcome.map(|_| ())
}

fn assert_replays_to(
    live: &OrderBook<()>,
    journal: &InMemoryJournal<()>,
    config: &ReplayBookConfig,
) {
    let (replayed, _) = ReplayEngine::<()>::replay_from_with_clock_and_config(
        journal,
        0,
        SYMBOL,
        stub_clock(),
        config,
    )
    .expect("replay succeeds");
    assert!(
        snapshots_match(
            &replayed.create_snapshot(usize::MAX),
            &live.create_snapshot(usize::MAX)
        ),
        "replayed book diverged from the live book"
    );
}

/// An IOC that consumes the whole book and then reports its unfillable
/// remainder: the fills happened, so replay must consume them too.
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
    assert_replays_to(&live, &journal, &ReplayBookConfig::default());
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
    assert_replays_to(&live, &journal, &config);
}

/// A market order that only cancels same-user makers under `CancelMaker`
/// mutates the book and still reports no liquidity.
#[test]
fn cancel_maker_market_rejection_replays_its_cancels() {
    let mut live = OrderBook::<()>::with_clock(SYMBOL, stub_clock());
    live.set_stp_mode(STPMode::CancelMaker);
    let journal: InMemoryJournal<()> = InMemoryJournal::new();

    sequence(
        &live,
        &journal,
        0,
        order(1, 100, 5, Side::Sell, TimeInForce::Gtc, user(1)),
    )
    .expect("seed same-user maker");

    let taker = Id::from_u64(2);
    let outcome = live.submit_market_order_with_user(taker, 5, Side::Buy, user(1));
    let err = outcome.expect_err("every maker at the level is the taker's own");
    journal
        .append(&SequencerEvent {
            sequence_num: 1,
            timestamp_ns: 1,
            command: SequencerCommand::MarketOrder {
                id: taker,
                quantity: 5,
                side: Side::Buy,
            },
            result: SequencerResult::Rejected {
                reason: err.to_string(),
            },
        })
        .expect("journal append");

    assert_eq!(live.best_ask(), None, "the same-user maker was cancelled");
    let config = ReplayBookConfig::new(None, STPMode::CancelMaker, None, None, None, None);
    assert_replays_to(&live, &journal, &config);
}

/// A sequencer that records the fills rather than the rejection is
/// journaling the truth: replay tolerates the same two errors regardless of
/// how the command was classified.
#[test]
fn traded_then_failed_submit_replays_under_a_success_classification() {
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
    let _ = live.add_order(ioc);
    journal
        .append(&SequencerEvent {
            sequence_num: 1,
            timestamp_ns: 1,
            command: SequencerCommand::AddOrder(ioc),
            result: SequencerResult::OrderAdded {
                order_id: Id::from_u64(2),
            },
        })
        .expect("journal append");

    assert_replays_to(&live, &journal, &ReplayBookConfig::default());
}

/// A rejection that never touched the book replays as the same no-op.
#[test]
fn pure_rejection_replays_clean() {
    let mut live = OrderBook::<()>::with_clock(SYMBOL, stub_clock());
    live.set_tick_size(10);
    let journal: InMemoryJournal<()> = InMemoryJournal::new();

    let err = sequence(
        &live,
        &journal,
        0,
        order(1, 105, 10, Side::Sell, TimeInForce::Gtc, Hash32::zero()),
    )
    .expect_err("105 is not a multiple of the tick");
    assert!(
        matches!(err, OrderBookError::InvalidTickSize { .. }),
        "expected InvalidTickSize, got {err:?}"
    );
    sequence(
        &live,
        &journal,
        1,
        order(2, 100, 10, Side::Sell, TimeInForce::Gtc, Hash32::zero()),
    )
    .expect("a tick-aligned order is admitted");

    let config = ReplayBookConfig::new(None, STPMode::None, Some(10), None, None, None);
    assert_replays_to(&live, &journal, &config);
}

/// An error replay did not expect is still a hard failure.
#[test]
fn unexpected_submit_error_aborts_replay() {
    let journal: InMemoryJournal<()> = InMemoryJournal::new();
    for seq in 0..2 {
        journal
            .append(&SequencerEvent {
                sequence_num: seq,
                timestamp_ns: seq,
                command: SequencerCommand::AddOrder(order(
                    1,
                    100,
                    10,
                    Side::Sell,
                    TimeInForce::Gtc,
                    Hash32::zero(),
                )),
                result: SequencerResult::OrderAdded {
                    order_id: Id::from_u64(1),
                },
            })
            .expect("journal append");
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
