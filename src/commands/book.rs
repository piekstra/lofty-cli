//! Order-book parsing shared by every command that reads a property's book.
//!
//! Owns two rules that must not drift between commands:
//! - which envelope a side lives in: the current `orderbook.{bids,asks}`
//!   (aggregated by price, no order ids) or the older
//!   `orderbook.orderBook.{buyOrders,sellOrders}` (one entry per order, with
//!   an `id`);
//! - what "the book without my own orders" means. `quote`'s never-cross rail
//!   and `orders competitiveness` both read it, so they must agree.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BookSide {
    Bids,
    Asks,
}

impl BookSide {
    fn keys(self) -> (&'static str, &'static str) {
        match self {
            Self::Bids => ("bids", "buyOrders"),
            Self::Asks => ("asks", "sellOrders"),
        }
    }

    /// The order `direction` that rests on this side.
    pub fn direction(self) -> &'static str {
        match self {
            Self::Bids => "buy",
            Self::Asks => "sell",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Level {
    pub price: f64,
    pub qty: f64,
    /// Present only on the legacy per-order envelope.
    pub id: Option<String>,
}

/// Prices are compared in whole cents: the book and the order list both carry
/// USD floats, and an exact float match would miss `49.81` vs `49.809999`.
pub fn cents(p: f64) -> i64 {
    (p * 100.0).round() as i64
}

/// Quantity below this is float residue from subtraction, not resting size.
const DUST: f64 = 1e-9;

/// The resting levels on one side, from either envelope.
///
/// An entry without a numeric price is skipped, as is one that reports a
/// quantity of zero: that level is empty. An entry with a price but NO
/// quantity field is kept with an unknown (infinite) size. That fails closed:
/// the never-cross checks still see the price, and subtracting your own size
/// can never make it disappear. Readers that need a real size (reward scoring)
/// must skip non-finite quantities.
pub fn levels(book: &Value, side: BookSide) -> Vec<Level> {
    let (new_key, old_key) = side.keys();
    book.pointer(&format!("/orderbook/{new_key}"))
        .or_else(|| book.pointer(&format!("/orderbook/orderBook/{old_key}")))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|l| {
                    let price = l.get("price").and_then(Value::as_f64)?;
                    let qty = match l.get("quantity") {
                        None | Some(Value::Null) => f64::INFINITY,
                        Some(q) => q.as_f64()?,
                    };
                    let id = l
                        .get("id")
                        .or_else(|| l.get("orderId"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    (qty > DUST).then_some(Level { price, qty, id })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The levels with your own resting size removed.
///
/// `mine` is your open orders on this property; only those resting on `side`
/// are considered. A level carrying an order id (legacy envelope) is yours
/// exactly when the id is one of your order ids. Levels without ids are
/// aggregated by price, and your open quantity at that price is subtracted:
/// whatever remains belongs to someone else.
pub fn without_mine(levels: &[Level], mine: &[Value], side: BookSide) -> Vec<Level> {
    let mine: Vec<&Value> = mine
        .iter()
        .filter(|o| o.get("direction").and_then(Value::as_str) == Some(side.direction()))
        .collect();
    let my_ids: BTreeSet<&str> = mine
        .iter()
        .filter_map(|o| o.get("orderId").and_then(Value::as_str))
        .collect();
    let mut my_qty: BTreeMap<i64, f64> = BTreeMap::new();
    for o in &mine {
        if let Some(p) = o.get("price").and_then(Value::as_f64) {
            *my_qty.entry(cents(p)).or_default() +=
                o.get("quantity").and_then(Value::as_f64).unwrap_or(0.0);
        }
    }

    let mut out = Vec::new();
    let mut anonymous: BTreeMap<i64, f64> = BTreeMap::new();
    for l in levels {
        match &l.id {
            Some(id) if my_ids.contains(id.as_str()) => {}
            Some(_) => out.push(l.clone()),
            None => *anonymous.entry(cents(l.price)).or_default() += l.qty,
        }
    }
    for (c, q) in anonymous {
        let rest = q - my_qty.get(&c).copied().unwrap_or(0.0);
        if rest > DUST {
            out.push(Level {
                price: c as f64 / 100.0,
                qty: rest,
                id: None,
            });
        }
    }
    out
}

/// Best price on a side: the highest bid or the lowest ask.
pub fn best(levels: &[Level], side: BookSide) -> Option<f64> {
    let prices = levels.iter().map(|l| l.price);
    match side {
        BookSide::Bids => prices.reduce(f64::max),
        BookSide::Asks => prices.reduce(f64::min),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "01SAMPLEPROP00000000000001";

    fn load(rel: &str) -> Value {
        let path = format!("{}/tests/fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
    }

    fn mine() -> Vec<Value> {
        load("competitiveness/orders-open.json")["orders"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o["status"] == "active" && o["propertyId"] == P)
            .cloned()
            .collect()
    }

    #[test]
    fn reads_both_envelopes() {
        let current = load("competitiveness/orderbook-shared-top.json");
        let legacy = load("competitiveness/orderbook-legacy-ids.json");
        assert_eq!(
            best(&levels(&current, BookSide::Bids), BookSide::Bids),
            Some(49.81)
        );
        assert_eq!(
            best(&levels(&current, BookSide::Asks), BookSide::Asks),
            Some(52.75)
        );
        assert_eq!(
            best(&levels(&legacy, BookSide::Bids), BookSide::Bids),
            Some(49.81)
        );
        assert!(levels(&legacy, BookSide::Bids)
            .iter()
            .all(|l| l.id.is_some()));
    }

    #[test]
    fn subtracts_your_quantity_from_aggregated_levels() {
        // 49.81 holds 6, of which 4 are yours; 49.50 holds exactly your 2.
        let book = load("competitiveness/orderbook-shared-top.json");
        let others = without_mine(&levels(&book, BookSide::Bids), &mine(), BookSide::Bids);
        let at = |p: f64| {
            others
                .iter()
                .find(|l| cents(l.price) == cents(p))
                .map(|l| l.qty)
        };
        assert_eq!(at(49.81), Some(2.0));
        assert_eq!(at(49.5), None);
        assert_eq!(at(49.0), Some(10.0));
    }

    #[test]
    fn legacy_levels_are_matched_by_order_id() {
        // At 49.50 the legacy book lists your order AND someone else's of the
        // same size; only yours is removed.
        let book = load("competitiveness/orderbook-legacy-ids.json");
        let others = without_mine(&levels(&book, BookSide::Bids), &mine(), BookSide::Bids);
        assert_eq!(best(&others, BookSide::Bids), Some(49.5));
        assert_eq!(others.len(), 1);
    }

    #[test]
    fn only_orders_on_the_same_side_are_subtracted() {
        // Your ask at 52.75 must not eat into a bid level, or vice versa.
        let book = load("competitiveness/orderbook-shared-top.json");
        let asks = without_mine(&levels(&book, BookSide::Asks), &mine(), BookSide::Asks);
        assert_eq!(best(&asks, BookSide::Asks), Some(53.1));
        let bids = without_mine(&levels(&book, BookSide::Bids), &[], BookSide::Bids);
        assert_eq!(bids.len(), 3);
    }

    #[test]
    fn zero_quantity_is_empty_but_missing_quantity_fails_closed() {
        // Asks: 51 reports quantity 0 (empty), 52 reports no quantity at all,
        // 53 x2. An unknown-size level must stay visible, even after your own
        // same-price size is subtracted, or a crossing check fails open.
        let book = load("book/orderbook-zero-and-missing-quantity.json");
        let asks = levels(&book, BookSide::Asks);
        assert_eq!(asks.len(), 2);
        assert_eq!(best(&asks, BookSide::Asks), Some(52.0));
        let my_ask = load("competitiveness/orders-open.json")["orders"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["direction"] == "sell" && o["status"] == "active")
            .cloned()
            .map(|mut o| {
                o["price"] = serde_json::json!(52.0);
                o
            })
            .unwrap();
        let others = without_mine(&asks, &[my_ask], BookSide::Asks);
        assert_eq!(best(&others, BookSide::Asks), Some(52.0));
    }

    #[test]
    fn an_empty_side_has_no_best() {
        let book = load("competitiveness/orderbook-bids-only.json");
        assert!(levels(&book, BookSide::Asks).is_empty());
        assert_eq!(best(&levels(&book, BookSide::Asks), BookSide::Asks), None);
    }
}
