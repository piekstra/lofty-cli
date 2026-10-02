//! `lofty orders competitiveness` — how far your resting orders are from
//! filling, and how far the competition is from you. Read-only.
//!
//! Pure report logic lives here; the command in `orders.rs` only fetches your
//! open orders and each property's order book.

use pk_cli_core::output;
use serde_json::{json, Value};

use super::book::{self, cents, BookSide};

/// Snap float dirt off a derived figure (`53.10 - 49.81` → `3.29`, not
/// `3.2900000000000027`) so it reads cleanly in `--json`.
fn clean(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

/// Gap from `from` to `to` in dollars, and as a percent of `base`.
fn gap(from: Option<f64>, to: Option<f64>, base: Option<f64>) -> (Option<f64>, Option<f64>) {
    match (from, to) {
        (Some(a), Some(b)) => {
            let d = clean(b - a);
            let pct = base.filter(|x| *x > 0.0).map(|x| clean(d / x * 100.0));
            (Some(d), pct)
        }
        _ => (None, None),
    }
}

/// Competitiveness of your open orders on ONE property. Pure (unit-tested).
///
/// `mine` is your open orders on this property (any direction); `book` is its
/// `/properties/{id}/orderbook` payload.
///
/// Buy side:
/// - `fillGap`: from your highest bid UP to the lowest ask that is not yours —
///   how far a seller must come down to fill you, as a percent of that ask.
///   Your own ask cannot fill your own bid, so it is not the counterparty.
/// - `competitionGap`: from the highest bid that is not yours UP to the highest
///   bid in the book, as a percent of the former. Zero when someone else holds
///   (or shares) the top; positive when you are alone above the competition.
/// - `leadUsd`: your highest bid minus the highest bid not yours (negative when
///   you are behind).
///
/// The sell side mirrors it: `fillGap` from the highest bid not yours UP to
/// your lowest ask (as a percent of that bid), `competitionGap` from the
/// lowest ask in the book UP to the lowest ask not yours, and `leadUsd` is how
/// far your lowest ask sits BELOW the lowest ask not yours.
pub fn property_report(property_id: &str, mine: &[Value], book_payload: &Value) -> Value {
    let dir = |d: &str| -> Vec<&Value> {
        mine.iter()
            .filter(|o| o.get("direction").and_then(Value::as_str) == Some(d))
            .collect()
    };
    let (my_bids, my_asks) = (dir("buy"), dir("sell"));
    // Your own size is attributed by `book::without_mine`, the same rule the
    // `quote` never-cross rail uses.
    let bids = book::levels(book_payload, BookSide::Bids);
    let asks = book::levels(book_payload, BookSide::Asks);
    let book_best_bid = book::best(&bids, BookSide::Bids);
    let book_best_ask = book::best(&asks, BookSide::Asks);
    let other_best_bid = book::best(
        &book::without_mine(&bids, mine, BookSide::Bids),
        BookSide::Bids,
    );
    let other_best_ask = book::best(
        &book::without_mine(&asks, mine, BookSide::Asks),
        BookSide::Asks,
    );

    let summary = |orders: &[&Value], best: fn(f64, f64) -> f64| -> Option<(f64, usize, f64)> {
        let prices: Vec<f64> = orders
            .iter()
            .filter_map(|o| o.get("price").and_then(Value::as_f64))
            .collect();
        let first = *prices.first()?;
        let qty: f64 = orders
            .iter()
            .filter_map(|o| o.get("quantity").and_then(Value::as_f64))
            .sum();
        Some((prices.iter().copied().fold(first, best), orders.len(), qty))
    };

    let buy = summary(&my_bids, f64::max).map(|(my_best, n, qty)| {
        let (fill, fill_pct) = gap(Some(my_best), other_best_ask, other_best_ask);
        let (comp, comp_pct) = gap(other_best_bid, book_best_bid, other_best_bid);
        json!({
            "orders": n,
            "quantity": qty,
            "myBestBid": my_best,
            "lowestOtherAsk": other_best_ask,
            "fillGapUsd": fill,
            "fillGapPct": fill_pct,
            "bookBestBid": book_best_bid,
            "highestOtherBid": other_best_bid,
            "competitionGapUsd": comp,
            "competitionGapPct": comp_pct,
            "leadUsd": other_best_bid.map(|o| clean(my_best - o)),
            "atTop": book_best_bid.is_none_or(|b| cents(my_best) >= cents(b)),
        })
    });
    let sell = summary(&my_asks, f64::min).map(|(my_best, n, qty)| {
        let (fill, fill_pct) = gap(other_best_bid, Some(my_best), other_best_bid);
        let (comp, comp_pct) = gap(book_best_ask, other_best_ask, book_best_ask);
        json!({
            "orders": n,
            "quantity": qty,
            "myBestAsk": my_best,
            "highestOtherBid": other_best_bid,
            "fillGapUsd": fill,
            "fillGapPct": fill_pct,
            "bookBestAsk": book_best_ask,
            "lowestOtherAsk": other_best_ask,
            "competitionGapUsd": comp,
            "competitionGapPct": comp_pct,
            "leadUsd": other_best_ask.map(|o| clean(o - my_best)),
            "atTop": book_best_ask.is_none_or(|a| cents(my_best) <= cents(a)),
        })
    });

    json!({
        "propertyId": property_id,
        "book": {
            "bestBid": book_best_bid,
            "bestAsk": book_best_ask,
            "highestOtherBid": other_best_bid,
            "lowestOtherAsk": other_best_ask,
        },
        "buy": buy,
        "sell": sell,
    })
}

/// Group open orders by property, preserving first-seen order.
pub fn by_property(open: &[Value]) -> Vec<(String, Vec<Value>)> {
    let mut out: Vec<(String, Vec<Value>)> = Vec::new();
    for o in open {
        let pid = o
            .get("propertyId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match out.iter_mut().find(|(p, _)| *p == pid) {
            Some((_, v)) => v.push(o.clone()),
            None => out.push((pid, vec![o.clone()])),
        }
    }
    out
}

fn money(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_f64)
        .map(|x| format!("${x:.2}"))
        .unwrap_or_else(|| "-".into())
}

fn signed(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_f64)
        .map(|x| {
            if x < 0.0 {
                format!("-${:.2}", -x)
            } else {
                format!("+${x:.2}")
            }
        })
        .unwrap_or_else(|| "-".into())
}

fn gap_text(v: &Value, usd_key: &str, pct_key: &str) -> String {
    match (
        v.get(usd_key).and_then(Value::as_f64),
        v.get(pct_key).and_then(Value::as_f64),
    ) {
        (Some(d), Some(p)) => format!("${d:.2} ({p:.2}%)"),
        (Some(d), None) => format!("${d:.2}"),
        _ => "-".into(),
    }
}

/// Human view: one row per property and side.
pub fn render(v: &Value) {
    let props = v
        .get("properties")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if props.is_empty() {
        println!("no open orders");
        return;
    }
    let mut rows = Vec::new();
    for p in &props {
        for (side, mine_key, other_key) in [
            ("buy", "myBestBid", "lowestOtherAsk"),
            ("sell", "myBestAsk", "highestOtherBid"),
        ] {
            let s = &p[side];
            if s.is_null() {
                continue;
            }
            rows.push(json!({
                "property": p["propertyId"],
                "side": side,
                "mine": money(s, mine_key),
                "qty": s["quantity"],
                "counterparty": money(s, other_key),
                "to fill": gap_text(s, "fillGapUsd", "fillGapPct"),
                "top vs next other": gap_text(s, "competitionGapUsd", "competitionGapPct"),
                "lead": signed(s, "leadUsd"),
            }));
        }
    }
    output::table(&rows);
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "01SAMPLEPROP00000000000001";

    fn load(rel: &str) -> Value {
        let path = format!("{}/tests/fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
    }

    /// Open orders on P from the shared orders fixture (active only, as the
    /// command selects them).
    fn mine() -> Vec<Value> {
        load("competitiveness/orders-open.json")["orders"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o["status"] == "active" && o["propertyId"] == P)
            .cloned()
            .collect()
    }

    fn report(book: &str) -> Value {
        property_report(P, &mine(), &load(&format!("competitiveness/{book}")))
    }

    #[test]
    fn buy_fill_gap_is_measured_to_the_lowest_ask_not_yours() {
        // Your bid 49.81; your own ask 52.75 is the book's best but cannot fill
        // you, so the counterparty is the next ask, 53.10.
        let r = report("orderbook-shared-top.json");
        assert_eq!(r["buy"]["myBestBid"], 49.81);
        assert_eq!(r["buy"]["lowestOtherAsk"], 53.1);
        assert_eq!(r["buy"]["fillGapUsd"], 3.29);
        assert_eq!(r["buy"]["fillGapPct"], clean(3.29 / 53.1 * 100.0));
    }

    #[test]
    fn a_shared_top_level_has_no_competition_gap() {
        // The 49.81 level holds 6: 4 are yours (the partially filled order's
        // REMAINING quantity), 2 are someone else's. They share the top.
        let r = report("orderbook-shared-top.json");
        assert_eq!(r["buy"]["bookBestBid"], 49.81);
        assert_eq!(r["buy"]["highestOtherBid"], 49.81);
        assert_eq!(r["buy"]["competitionGapUsd"], 0.0);
        assert_eq!(r["buy"]["leadUsd"], 0.0);
        assert_eq!(r["buy"]["atTop"], true);
    }

    #[test]
    fn sell_side_mirrors_the_buy_side() {
        // Your ask 52.75 alone at the top; the next ask not yours is 53.10, and
        // the highest bid not yours is 49.81.
        let r = report("orderbook-shared-top.json");
        let s = &r["sell"];
        assert_eq!(s["myBestAsk"], 52.75);
        assert_eq!(s["highestOtherBid"], 49.81);
        assert_eq!(s["fillGapUsd"], 2.94);
        assert_eq!(s["fillGapPct"], clean(2.94 / 49.81 * 100.0));
        assert_eq!(s["bookBestAsk"], 52.75);
        assert_eq!(s["lowestOtherAsk"], 53.1);
        assert_eq!(s["competitionGapUsd"], 0.35);
        assert_eq!(s["competitionGapPct"], clean(0.35 / 52.75 * 100.0));
        assert_eq!(s["leadUsd"], 0.35);
        assert_eq!(s["orders"], 1);
    }

    #[test]
    fn alone_on_top_shows_the_lead_and_behind_shows_a_negative_one() {
        // Bids: you alone at 49.81, the next other bid 49.00 → you lead by 0.81.
        // Asks: someone else at 52.00 undercuts your 52.75 → you trail by 0.75.
        let r = report("orderbook-alone-on-top.json");
        assert_eq!(r["buy"]["highestOtherBid"], 49.0);
        assert_eq!(r["buy"]["competitionGapUsd"], 0.81);
        assert_eq!(r["buy"]["competitionGapPct"], clean(0.81 / 49.0 * 100.0));
        assert_eq!(r["buy"]["leadUsd"], 0.81);
        assert_eq!(r["sell"]["bookBestAsk"], 52.0);
        assert_eq!(r["sell"]["lowestOtherAsk"], 52.0);
        assert_eq!(r["sell"]["competitionGapUsd"], 0.0);
        assert_eq!(r["sell"]["leadUsd"], -0.75);
        assert_eq!(r["sell"]["atTop"], false);
        // Buy fill gap now runs to the undercutting ask, not your own.
        assert_eq!(r["buy"]["fillGapUsd"], 2.19);
    }

    #[test]
    fn legacy_envelope_matches_your_orders_by_id() {
        // The older per-order envelope carries ids: an order with your id is
        // yours regardless of price coincidences.
        let r = report("orderbook-legacy-ids.json");
        assert_eq!(r["buy"]["bookBestBid"], 49.81);
        assert_eq!(r["buy"]["highestOtherBid"], 49.5);
        assert_eq!(r["buy"]["competitionGapUsd"], 0.31);
        assert_eq!(r["sell"]["lowestOtherAsk"], 53.1);
    }

    #[test]
    fn an_empty_opposite_side_leaves_the_fill_gap_unknown() {
        let r = property_report(
            P,
            &mine(),
            &load("competitiveness/orderbook-bids-only.json"),
        );
        assert!(r["buy"]["fillGapUsd"].is_null());
        assert!(r["buy"]["fillGapPct"].is_null());
        assert!(r["book"]["bestAsk"].is_null());
    }

    #[test]
    fn a_side_with_no_orders_of_yours_is_null() {
        let bids_only: Vec<Value> = mine()
            .into_iter()
            .filter(|o| o["direction"] == "buy")
            .collect();
        let r = property_report(
            P,
            &bids_only,
            &load("competitiveness/orderbook-shared-top.json"),
        );
        assert!(r["sell"].is_null());
        assert!(!r["buy"].is_null());
        // Without your ask in `mine`, the 52.75 ask counts as someone else's.
        assert_eq!(r["buy"]["lowestOtherAsk"], 52.75);
    }

    #[test]
    fn groups_open_orders_by_property() {
        let open: Vec<Value> = load("competitiveness/orders-open.json")["orders"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o["status"] == "active")
            .cloned()
            .collect();
        let g = by_property(&open);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].0, P);
        assert_eq!(g[0].1.len(), 3);
    }
}
