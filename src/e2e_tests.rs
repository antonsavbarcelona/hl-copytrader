//! End-to-end: the bot's order code (`live::Venue`, `trades::*`, the wick's cancel settlement) against the fake
//! Hyperliquid of `testkit`: real HTTP, real signing, the API's reply shapes, the matching faked. Each test has its own
//! coins (the family registry in `trades` is process-wide).

use crate::testkit::FakeHl;
use crate::trades;
use crate::wick::{AfterCancel, after_cancel};

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6 * b.abs().max(1.0)
}

// ---------------------------------------------------------------- opening and protecting

#[tokio::test]
async fn a_long_opens_at_market_with_its_stop_and_take_resting() {
    let f = FakeHl::start(&[("E2A", 2, 10, 100.0)]).await;
    let v = f.venue();
    let tr = trades::open_market(&v, "E2A", 1.0, 0.10, Some(0.05), 3600.0).await.unwrap();
    assert!(close(f.position("E2A"), 1.0));
    assert!(close(tr.entry_px, 100.0) && close(tr.stop_px, 90.0) && close(tr.take_px, 105.0));
    let o = f.open_orders("E2A");
    assert_eq!(o.len(), 2, "{o:?}");
    let stop = o.iter().find(|x| x.trigger.is_some()).unwrap();
    let take = o.iter().find(|x| x.trigger.is_none()).unwrap();
    assert!(!stop.buy && stop.reduce && close(stop.trigger.unwrap(), 90.0));
    assert!(!take.buy && take.reduce && close(take.px, 105.0));
    assert_eq!(Some(stop.oid), tr.stop_oid);
    assert_eq!(Some(take.oid), tr.take_oid);
    // the coin's leverage set first: cross at its max
    assert_eq!(f.with(|s| s.leverage.get("E2A").copied()), Some((10, true)));
}

#[tokio::test]
async fn a_short_mirrors_its_stop_above_and_its_take_below() {
    let f = FakeHl::start(&[("E2B", 2, 10, 100.0)]).await;
    let tr = trades::open_market(&f.venue(), "E2B", -2.0, 0.10, Some(0.05), 3600.0).await.unwrap();
    assert!(close(f.position("E2B"), -2.0));
    assert!(close(tr.stop_px, 110.0) && close(tr.take_px, 95.0));
    let o = f.open_orders("E2B");
    assert!(o.iter().any(|x| x.trigger.is_some() && x.buy && x.reduce));
    assert!(o.iter().any(|x| x.trigger.is_none() && x.buy && x.reduce && close(x.px, 95.0)));
}

#[tokio::test]
async fn a_market_order_tries_again_further_through_the_mid() {
    let f = FakeHl::start(&[("E2C", 2, 10, 100.0)]).await;
    f.with(|s| s.ioc_needs = 0.05); // 2% through does not match, 8% does
    let v = f.venue();
    let (got, px, _) = v.market("E2C", 1.0, false).await.unwrap();
    assert!(close(got, 1.0) && close(px, 100.0));
    let n = f.with(|s| s.actions.iter().filter(|a| a["type"] == "order").count());
    assert_eq!(n, 2);
}

#[tokio::test]
async fn a_market_order_with_nothing_within_reach_is_an_error_and_opens_nothing() {
    let f = FakeHl::start(&[("E2D", 2, 10, 100.0)]).await;
    f.with(|s| s.ioc_needs = 0.5);
    let e = trades::open_market(&f.venue(), "E2D", 1.0, 0.10, Some(0.05), 3600.0).await.unwrap_err();
    assert!(format!("{e:#}").contains("not filled"), "{e:#}");
    assert_eq!(f.position("E2D"), 0.0);
    assert!(f.open_orders("E2D").is_empty());
}

#[tokio::test]
async fn a_stop_refused_closes_the_position_again() {
    let f = FakeHl::start(&[("E2E", 2, 10, 100.0)]).await;
    f.with(|s| s.refuse_stops = true);
    let e = trades::open_market(&f.venue(), "E2E", 1.0, 0.10, Some(0.05), 3600.0).await.unwrap_err();
    assert!(format!("{e:#}").contains("closed again"), "{e:#}");
    assert_eq!(f.position("E2E"), 0.0, "never a position without its stop");
    assert!(f.open_orders("E2E").is_empty());
}

#[tokio::test]
async fn an_isolated_hip3_coin_gets_its_stop_inside_the_liquidation() {
    let f = FakeHl::start(&[("xyz:E2F", 2, 10, 100.0)]).await;
    f.with(|s| { s.refuse_cross.insert("xyz:E2F".into()); });
    let v = f.venue();
    let tr = trades::open_market(&v, "xyz:E2F", 1.0, 0.20, Some(0.05), 3600.0).await.unwrap();
    assert_eq!(f.with(|s| s.leverage.get("xyz:E2F").copied()), Some((5, false)));
    // 5x on a 10x coin liquidates ~15% away: the 20% stop comes in to 90% of it, 13.5%
    assert!(close(tr.stop_px, 86.5), "{}", tr.stop_px);
    assert_eq!(v.stop_cap("xyz:E2F").map(|x| (x * 1000.0).round()), Some(135.0));
}

#[tokio::test]
async fn the_take_comes_from_the_price_expected_when_the_fill_was_worse() {
    let f = FakeHl::start(&[("E2G", 2, 10, 100.0)]).await;
    let tr = trades::open_market_at(&f.venue(), "E2G", 1.0, 0.10, Some(0.05), 3600.0, 99.0).await.unwrap();
    assert!(close(tr.entry_px, 100.0));
    assert!(close(tr.take_px, 103.95), "{}", tr.take_px);
    assert!(close(tr.stop_px, 90.0), "the stop stays from the fill");
}

#[tokio::test]
async fn a_fill_better_than_expected_keeps_the_take_from_the_fill() {
    let f = FakeHl::start(&[("E2G2", 2, 10, 100.0)]).await;
    let tr = trades::open_market_at(&f.venue(), "E2G2", 1.0, 0.10, Some(0.05), 3600.0, 101.0).await.unwrap();
    assert!(close(tr.take_px, 105.0), "{}", tr.take_px);
    let s = trades::open_market_at(&f.venue(), "E2G2", -0.0, 0.10, Some(0.05), 3600.0, 101.0).await;
    assert!(s.is_err(), "a zero size is refused, not sent");
}

#[tokio::test]
async fn a_margin_refusal_takes_the_weakest_resting_orders_off_and_tries_once_more() {
    let f = FakeHl::start(&[("E2H", 2, 10, 100.0), ("E2H2", 2, 10, 50.0)]).await;
    let v = f.venue();
    let bid = match v.limit("E2H2", 1.0, 45.0, false, "Gtc").await.unwrap() {
        crate::live::Placed::Resting(o) => o,
        _ => panic!("the bid should rest"),
    };
    crate::live::set_release(vec![("E2H2".into(), bid, 1_000.0)]);
    f.with(|s| s.margin_refusals = 1);
    let (got, _, _) = v.market("E2H", 1.0, false).await.unwrap();
    assert!(close(got, 1.0));
    assert_eq!(f.order(bid).unwrap().status, "canceled");
    crate::live::set_release(vec![]);
}

#[tokio::test]
async fn a_post_only_order_that_would_cross_is_refused() {
    let f = FakeHl::start(&[("E2I", 2, 10, 100.0)]).await;
    let e = f.venue().limit("E2I", 1.0, 101.0, false, "Alo").await.unwrap_err();
    assert!(format!("{e:#}").contains("not placed"), "{e:#}");
    assert_eq!(f.position("E2I"), 0.0);
}

#[tokio::test]
async fn a_reduce_only_market_order_with_nothing_held_does_not_open_one() {
    let f = FakeHl::start(&[("E2J", 2, 10, 100.0)]).await;
    assert!(f.venue().market("E2J", -1.0, true).await.is_err());
    assert_eq!(f.position("E2J"), 0.0);
}

// ---------------------------------------------------------------- how a trade ends

#[tokio::test]
async fn the_take_fills_and_the_trade_ends_at_the_take_its_stop_cancelled() {
    let f = FakeHl::start(&[("E2K", 2, 10, 100.0)]).await;
    let v = f.venue();
    let tr = trades::open_market(&v, "E2K", 1.0, 0.10, Some(0.05), 3600.0).await.unwrap();
    f.set_mid("E2K", 106.0);
    assert_eq!(f.position("E2K"), 0.0);
    let (why, px) = trades::gone(&v, &tr, 0.0, "e2e").await.unwrap();
    assert_eq!(why, "take");
    assert!(close(px, 105.0));
    assert!(f.open_orders("E2K").is_empty(), "the stop must not stay behind");
}

#[tokio::test]
async fn the_stop_fires_and_the_trade_ends_at_its_fill_the_take_cancelled() {
    let f = FakeHl::start(&[("E2L", 2, 10, 100.0)]).await;
    let v = f.venue();
    let tr = trades::open_market(&v, "E2L", 1.0, 0.10, Some(0.05), 3600.0).await.unwrap();
    f.set_mid("E2L", 88.0);
    assert_eq!(f.position("E2L"), 0.0);
    let (why, px) = trades::gone(&v, &tr, 0.0, "e2e").await.unwrap();
    assert_eq!(why, "stop");
    assert!(close(px, 88.0), "{px}");
    assert!(f.open_orders("E2L").is_empty(), "the take must not stay behind");
}

#[tokio::test]
async fn a_position_closed_by_hand_is_told_manual_and_both_orders_come_off() {
    let f = FakeHl::start(&[("E2M", 2, 10, 100.0)]).await;
    let v = f.venue();
    let tr = trades::open_market(&v, "E2M", 1.0, 0.10, Some(0.05), 3600.0).await.unwrap();
    f.set_mid("E2M", 101.0);
    f.hand_trade("E2M", -1.0);
    let (why, px) = trades::gone(&v, &tr, 0.0, "e2e").await.unwrap();
    assert_eq!(why, "manual");
    assert!(close(px, 101.0), "{px}");
    assert!(f.open_orders("E2M").is_empty());
}

#[tokio::test]
async fn a_take_filled_in_part_closes_the_rest_at_market() {
    let f = FakeHl::start(&[("E2N", 2, 10, 100.0)]).await;
    let v = f.venue();
    let tr = trades::open_market(&v, "E2N", 1.0, 0.10, Some(0.05), 3600.0).await.unwrap();
    f.set_mid("E2N", 104.0);
    f.partial_fill(tr.take_oid.unwrap(), 0.4);
    let held = f.position("E2N");
    assert!(close(held, 0.6));
    let (why, _) = trades::gone(&v, &tr, held, "e2e").await.unwrap();
    assert_eq!(why, "take");
    assert_eq!(f.position("E2N"), 0.0, "the rest closed at market");
    assert!(f.open_orders("E2N").is_empty(), "the take's rest and the stop cancelled");
}

#[tokio::test]
async fn a_take_whose_status_lags_is_still_read_as_the_take() {
    let f = FakeHl::start(&[("E2O", 2, 10, 100.0)]).await;
    let v = f.venue();
    let tr = trades::open_market(&v, "E2O", 1.0, 0.10, Some(0.05), 3600.0).await.unwrap();
    f.with(|s| { s.status_lag.insert(tr.take_oid.unwrap(), 2); });
    f.set_mid("E2O", 106.0);
    let (why, px) = trades::gone(&v, &tr, 0.0, "e2e").await.unwrap();
    assert_eq!(why, "take");
    assert!(close(px, 105.0));
}

#[tokio::test]
async fn closing_on_time_cancels_both_orders_then_sells_at_market() {
    let f = FakeHl::start(&[("E2P", 2, 10, 100.0)]).await;
    let v = f.venue();
    let tr = trades::open_market(&v, "E2P", 1.0, 0.10, Some(0.05), 3600.0).await.unwrap();
    f.set_mid("E2P", 102.0);
    let px = trades::close(&v, &tr).await.unwrap();
    assert!(close(px, 102.0));
    assert_eq!(f.position("E2P"), 0.0);
    assert!(f.open_orders("E2P").is_empty());
}

#[tokio::test]
async fn a_trade_still_held_is_not_ended() {
    let f = FakeHl::start(&[("E2Q", 2, 10, 100.0)]).await;
    let v = f.venue();
    let tr = trades::open_market(&v, "E2Q", 1.0, 0.10, Some(0.05), 3600.0).await.unwrap();
    assert!(trades::gone(&v, &tr, 1.0, "e2e").await.is_none());
    assert_eq!(f.open_orders("E2Q").len(), 2);
}

// ---------------------------------------------------------------- the wick's bids (2026-11-09, xyz:HIMS)

#[tokio::test]
async fn a_bid_filled_just_before_its_cancel_is_kept_while_its_status_lags_then_settled_as_filled() {
    let f = FakeHl::start(&[("xyz:E2R", 2, 10, 100.0)]).await;
    let v = f.venue();
    f.with(|s| s.lag_next_order = 2); // its status says "open, nothing filled" for two reads
    let oid = match v.limit("xyz:E2R", 1.0, 96.0, false, "Gtc").await.unwrap() {
        crate::live::Placed::Resting(o) => o,
        _ => panic!("the bid should rest"),
    };
    f.set_mid("xyz:E2R", 95.0); // filled
    assert!(close(f.position("xyz:E2R"), 1.0));
    v.cancel("xyz:E2R", oid).await; // too late: refused, logged
    // the read right after the cancel lags: the bid must not be dropped
    assert_eq!(after_cancel(&v.filled(oid).await), AfterCancel::Keep);
    assert_eq!(after_cancel(&v.filled(oid).await), AfterCancel::Keep);
    // the next check sees the fill: the trade with its stop and take
    assert_eq!(after_cancel(&v.filled(oid).await), AfterCancel::Filled(1.0));
}

#[tokio::test]
async fn a_bid_cancelled_with_nothing_filled_goes_off_the_book() {
    let f = FakeHl::start(&[("xyz:E2S", 2, 10, 100.0)]).await;
    let v = f.venue();
    let oid = match v.limit("xyz:E2S", 1.0, 96.0, false, "Gtc").await.unwrap() {
        crate::live::Placed::Resting(o) => o,
        _ => panic!("the bid should rest"),
    };
    v.cancel("xyz:E2S", oid).await;
    assert_eq!(after_cancel(&v.filled(oid).await), AfterCancel::Gone);
    assert_eq!(f.position("xyz:E2S"), 0.0);
}

#[tokio::test]
async fn a_bid_filled_in_part_before_its_cancel_is_the_part_traded() {
    let f = FakeHl::start(&[("xyz:E2T", 2, 10, 100.0)]).await;
    let v = f.venue();
    let oid = match v.limit("xyz:E2T", 1.0, 96.0, false, "Gtc").await.unwrap() {
        crate::live::Placed::Resting(o) => o,
        _ => panic!("the bid should rest"),
    };
    f.partial_fill(oid, 0.3);
    v.cancel("xyz:E2T", oid).await;
    match after_cancel(&v.filled(oid).await) {
        AfterCancel::Filled(x) => assert!(close(x, 0.3), "{x}"),
        o => panic!("{o:?}"),
    }
}

#[tokio::test]
async fn an_unknown_order_status_is_kept_not_dropped() {
    let f = FakeHl::start(&[("E2U", 2, 10, 100.0)]).await;
    let r = f.venue().filled(999_999).await;
    assert!(r.is_err());
    assert_eq!(after_cancel(&r), AfterCancel::Keep);
}
