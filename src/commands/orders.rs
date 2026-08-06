//! `lofty orders` — list, inspect, place, and cancel limit orders.
//!
//! Placing and cancelling are mutations: they prompt for confirmation, or
//! require `--force` when non-interactive (exit 6 otherwise). Both send an
//! `Idempotency-Key` automatically.

use clap::Subcommand;
use pk_cli_core::{output, CliError};
use serde_json::{json, Value};

use super::{confirm, emit, table_view, Ctx};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List your orders (all properties by default).
    #[command(alias = "ls")]
    List {
        /// Filter to one property.
        #[arg(long)]
        property_id: Option<String>,
        /// active | pending | executing | executed | cancelled | expired | intent.
        #[arg(long)]
        status: Option<String>,
    },
    /// Get one order by ID.
    Get { order_id: String },
    /// Place a limit order (requires a trading-enabled key).
    Create {
        #[arg(long)]
        property_id: String,
        /// buy or sell.
        #[arg(long)]
        direction: String,
        /// Price per token in USD (min $0.01).
        #[arg(long)]
        price: f64,
        /// Number of tokens (min 1).
        #[arg(long)]
        quantity: u32,
        /// Expiry as Unix ms (min 29 days out; default 30 days).
        #[arg(long)]
        expire_at: Option<u64>,
        /// Accept a price materially worse than the market, or an unpriceable
        /// book. Says out loud: I may lose money against the listed price.
        #[arg(long)]
        accept_below_market: bool,
        /// Skip the confirmation prompt.
        #[arg(long)]
        force: bool,
    },
    /// Cancel an active order.
    Cancel {
        order_id: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        force: bool,
    },
}

pub fn run(ctx: &Ctx, cmd: &Cmd) -> Result<(), CliError> {
    match cmd {
        Cmd::List {
            property_id,
            status,
        } => {
            let client = ctx.client()?;
            let mut q: Vec<(&str, String)> = Vec::new();
            match property_id {
                Some(id) => q.push(("propertyId", id.clone())),
                None => q.push(("all", "true".into())),
            }
            if let Some(s) = status {
                q.push(("status", s.clone()));
            }
            let payload = client.get("/public/v1/orders", &q)?;
            emit(ctx, "orders-list", payload, |v| {
                let orders = v
                    .get("orders")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                output::table(&table_view(
                    &orders,
                    &[
                        "orderId",
                        "propertyId",
                        "direction",
                        "price",
                        "quantity",
                        "status",
                        "expireAt",
                    ],
                ));
            });
            Ok(())
        }
        Cmd::Get { order_id } => {
            let client = ctx.client()?;
            let payload = client.get(&format!("/public/v1/orders/{order_id}"), &[])?;
            emit(ctx, "order", payload, output::render);
            Ok(())
        }
        Cmd::Create {
            property_id,
            direction,
            price,
            quantity,
            expire_at,
            accept_below_market,
            force,
        } => {
            if !matches!(direction.as_str(), "buy" | "sell") {
                return Err(CliError::Usage(
                    "--direction must be `buy` or `sell`".into(),
                ));
            }
            if *price < 0.01 {
                return Err(CliError::Usage("--price must be at least $0.01".into()));
            }
            if *quantity < 1 {
                return Err(CliError::Usage("--quantity must be at least 1".into()));
            }
            // Preserve the invariant the surface tests pin: an unconfirmed mutation
            // must never touch the keychain or the network. Bail out here, before any
            // request, when we already know we cannot proceed — otherwise a
            // non-interactive run without --force would hang on a keychain prompt.
            if !force && !ctx.common.interactive() {
                return Err(CliError::ConfirmationRequired(format!(
                    "place limit {direction} of {quantity} token(s) @ ${price:.2} on {property_id} — pass --force to proceed non-interactively"
                )));
            }

            let client = ctx.client()?;

            // Price the order against the market BEFORE sending it. These books are
            // thin and trade rarely, so a mispriced limit order does not rest and
            // wait to be noticed — it crosses and fills instantly at whatever is
            // resting. That is not recoverable.
            let book = client
                .get(
                    &format!("/public/v1/properties/{property_id}/orderbook"),
                    &[],
                )
                .ok();
            let trades = client
                .get(&format!("/public/v1/properties/{property_id}/trades"), &[])
                .ok();
            let market = market_view(book.as_ref(), trades.as_ref());
            if let Some(problem) = market.objection(direction, *price) {
                if !accept_below_market {
                    return Err(CliError::Usage(format!(
                        "{problem}\n{}\n\nIf you understand and accept that this may lose money against the listed price, re-run with --accept-below-market.",
                        market.describe()
                    )));
                }
                eprintln!("\u{26a0} {problem}");
                eprintln!("{}", market.describe());
                eprintln!("proceeding: --accept-below-market was given");
            }

            // Reward-scoring advisory. Deliberately NOT a rail: a sub-minimum
            // order is legitimate as often as it is a mistake (see the doc on
            // min_contracts_note), so state the consequence and let the operator
            // judge. Printed before the prompt so --force runs still see it.
            let min_note = min_contracts_note(&client, property_id, *quantity);
            if let Some(note) = &min_note {
                eprintln!("\u{26a0} {note}");
            }

            confirm(
                ctx,
                *force,
                &format!(
                    "place limit {direction} of {quantity} token(s) @ ${price:.2} on {property_id} (total ${:.2}){}{}",
                    *price * f64::from(*quantity),
                    market.summary_suffix(),
                    min_note.map(|n| format!("\n\u{26a0} {n}")).unwrap_or_default(),
                ),
            )?;
            let mut body = json!({
                "propertyId": property_id,
                "direction": direction,
                "price": price,
                "quantity": quantity,
            });
            if let Some(exp) = expire_at {
                body["expireAt"] = json!(exp);
            }
            let payload = client.post("/public/v1/orders", &body)?;
            emit(ctx, "order-created", payload, output::render);
            Ok(())
        }
        Cmd::Cancel { order_id, force } => {
            confirm(ctx, *force, &format!("cancel order {order_id}"))?;
            let client = ctx.client()?;
            let payload = client.delete(&format!("/public/v1/orders/{order_id}"))?;
            emit(ctx, "order-cancelled", payload, output::render);
            Ok(())
        }
    }
}

/// Everything known about what a token is currently worth, gathered from BOTH
/// price sources because they disagree — an orderbook read of $50 against a real
/// bid of $71.09 is what priced a sell 20% under the market.
#[derive(Debug, Default, Clone, PartialEq)]
struct MarketView {
    book_bid: Option<f64>,
    book_ask: Option<f64>,
    feed_bid: Option<f64>,
    feed_ask: Option<f64>,
    last_trade: Option<f64>,
    /// Trades seen at all. An empty tape means the price is not discoverable
    /// here, which is a reason to stop rather than to shrug.
    trade_count: usize,
}

/// Tolerance before a price counts as "materially worse than the market". Wide
/// enough that ordinary spread does not trip it, tight enough that a fat-finger
/// or a stale-endpoint read does.
/// Advisory when an order is smaller than its LP-reward program's `minContracts`.
///
/// Qualifying a side and SCORING use different bars. A covered order of at least
/// `minTwoSidedLiquidity` (1 on every observed program) establishes that the side
/// exists; only orders of at least `minContracts` accrue score. So a sub-minimum
/// order is legal and sometimes exactly right — a 1-token ask is the cheapest way
/// to hold a side open — but a sub-minimum BID ties up USDC and earns nothing,
/// which is easy to do by accident and invisible afterwards.
///
/// Intent is unknowable from here, so this returns a note rather than an error.
/// Returns None when the property runs no reward program, when the order clears
/// the bar, or when the lookup fails — never blocks an order on a failed read.
fn min_contracts_note(
    client: &crate::client::LoftyClient,
    property_id: &str,
    quantity: u32,
) -> Option<String> {
    let programs = client.get("/public/v1/account/lp-programs", &[]).ok()?;
    min_contracts_note_from(&programs, property_id, quantity)
}

/// Pure half of [`min_contracts_note`], split out so the rule is testable without
/// a client. Takes the `/account/lp-programs` payload verbatim.
fn min_contracts_note_from(programs: &Value, property_id: &str, quantity: u32) -> Option<String> {
    let min = programs
        .get("programs")?
        .as_array()?
        .iter()
        .find(|p| p.get("propertyId").and_then(Value::as_str) == Some(property_id))?
        .get("minContracts")
        .and_then(Value::as_f64)?;
    (f64::from(quantity) < min).then(|| {
        format!(
            "{quantity} token(s) is below this property's minContracts ({min:.0}). \
             The order will QUALIFY its side for two-sided eligibility but score \
             NOTHING toward LP rewards. Use at least {min:.0} to earn on it."
        )
    })
}

const MARKET_TOLERANCE_PCT: f64 = 5.0;

fn market_view(book: Option<&Value>, trades: Option<&Value>) -> MarketView {
    let side = |v: Option<&Value>, new_key: &str, old_key: &str, want_max: bool| -> Option<f64> {
        let levels = v?
            .pointer(&format!("/orderbook/{new_key}"))
            .or_else(|| v?.pointer(&format!("/orderbook/orderBook/{old_key}")))?
            .as_array()?;
        levels
            .iter()
            .filter_map(|l| l.get("price").and_then(Value::as_f64))
            .fold(None, |acc: Option<f64>, p| {
                Some(match acc {
                    Some(a) if want_max => a.max(p),
                    Some(a) => a.min(p),
                    None => p,
                })
            })
    };
    let recent: Vec<(f64, u64)> = trades
        .and_then(|t| t.get("recentTrades"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some((
                        x.get("price").and_then(Value::as_f64)?,
                        x.get("createdAt").and_then(Value::as_u64).unwrap_or(0),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let last_trade = recent.iter().max_by_key(|(_, t)| *t).map(|(p, _)| *p);
    MarketView {
        book_bid: side(book, "bids", "buyOrders", true),
        book_ask: side(book, "asks", "sellOrders", false),
        feed_bid: trades
            .and_then(|t| t.get("bestBid"))
            .and_then(Value::as_f64),
        feed_ask: trades
            .and_then(|t| t.get("bestAsk"))
            .and_then(Value::as_f64),
        last_trade,
        trade_count: recent.len(),
    }
}

impl MarketView {
    /// The best evidence of what a SELL could fetch: the highest bid either source
    /// reports, and the last print. Deliberately the most FAVOURABLE reference —
    /// the question is "could you have done better", so being generous to the
    /// market makes the check strict on us.
    fn sell_reference(&self) -> Option<f64> {
        [self.book_bid, self.feed_bid, self.last_trade]
            .into_iter()
            .flatten()
            .fold(None, |a: Option<f64>, p| Some(a.map_or(p, |x| x.max(p))))
    }
    /// Mirror for buys: the lowest ask/print, i.e. the cheapest you could have paid.
    fn buy_reference(&self) -> Option<f64> {
        [self.book_ask, self.feed_ask, self.last_trade]
            .into_iter()
            .flatten()
            .fold(None, |a: Option<f64>, p| Some(a.map_or(p, |x| x.min(p))))
    }

    /// Why this order should not be sent as priced, if there is a reason.
    fn objection(&self, direction: &str, price: f64) -> Option<String> {
        // No tape at all: price is undiscoverable, so no check can vouch for it.
        // Silence is not evidence the price is fine.
        if self.trade_count == 0 {
            return Some(format!(
                "this property has NO trade history to price against — a {direction} at ${price:.2} cannot be checked, and thin books fill instantly at whatever is resting"
            ));
        }
        if direction == "sell" {
            let r = self.sell_reference()?;
            // At or under the bid, a sell is a taker order: it fills NOW, at the
            // bid, not at your price.
            if let Some(bid) = [self.book_bid, self.feed_bid]
                .into_iter()
                .flatten()
                .fold(None, |a: Option<f64>, p| {
                    Some(a.map_or(p, |x: f64| x.max(p)))
                })
            {
                if price <= bid {
                    return Some(format!(
                        "sell at ${price:.2} is AT OR BELOW the best bid ${bid:.2} — it will fill immediately as a taker, not rest on the book"
                    ));
                }
            }
            if price < r * (1.0 - MARKET_TOLERANCE_PCT / 100.0) {
                return Some(format!(
                    "sell at ${price:.2} is {:.1}% below the market reference ${r:.2}",
                    (1.0 - price / r) * 100.0
                ));
            }
        } else {
            let r = self.buy_reference()?;
            if let Some(ask) = [self.book_ask, self.feed_ask]
                .into_iter()
                .flatten()
                .fold(None, |a: Option<f64>, p| {
                    Some(a.map_or(p, |x: f64| x.min(p)))
                })
            {
                if price >= ask {
                    return Some(format!(
                        "buy at ${price:.2} is AT OR ABOVE the best ask ${ask:.2} — it will fill immediately as a taker"
                    ));
                }
            }
            if price > r * (1.0 + MARKET_TOLERANCE_PCT / 100.0) {
                return Some(format!(
                    "buy at ${price:.2} is {:.1}% above the market reference ${r:.2}",
                    (price / r - 1.0) * 100.0
                ));
            }
        }
        None
    }

    /// Show every source, including where they disagree — that disagreement is
    /// the failure mode, so hiding it behind one number would repeat it.
    fn describe(&self) -> String {
        let f = |o: Option<f64>| o.map_or("—".into(), |v| format!("${v:.2}"));
        format!(
            "  orderbook   bid {}  ask {}\n  trades feed bid {}  ask {}\n  last trade  {}   ({} recent print(s))",
            f(self.book_bid),
            f(self.book_ask),
            f(self.feed_bid),
            f(self.feed_ask),
            f(self.last_trade),
            self.trade_count,
        )
    }

    fn summary_suffix(&self) -> String {
        match (self.sell_reference(), self.last_trade) {
            (Some(r), Some(l)) => format!(" — market ref ${r:.2}, last trade ${l:.2}"),
            (Some(r), None) => format!(" — market ref ${r:.2}"),
            _ => " — NO market reference available".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn book(bids: Vec<f64>, asks: Vec<f64>) -> Value {
        json!({"orderbook": {
            "bids": bids.iter().map(|p| json!({"price": p, "quantity": 1})).collect::<Vec<_>>(),
            "asks": asks.iter().map(|p| json!({"price": p, "quantity": 1})).collect::<Vec<_>>()}})
    }
    fn feed(best_bid: f64, best_ask: f64, prints: Vec<(f64, u64)>) -> Value {
        json!({"bestBid": best_bid, "bestAsk": best_ask,
               "recentTrades": prints.iter().map(|(p,t)| json!({"price": p, "createdAt": t})).collect::<Vec<_>>()})
    }

    #[test]
    fn refuses_the_sell_that_actually_lost_money() {
        // The real incident: the ORDERBOOK reported a $50 best bid while the trades
        // feed reported $71.09 and the last print was $67.50. Trusting the orderbook
        // priced a sell at $57.12, which crossed and filled instantly.
        let b = book(vec![50.0, 45.0], vec![]);
        let t = feed(71.09, 71.43, vec![(67.50, 1_785_500_000_000)]);
        let m = market_view(Some(&b), Some(&t));
        let why = m.objection("sell", 57.12).expect("must object");
        assert!(why.contains("BELOW the best bid"), "{why}");
        // It must quote the REAL bid, not the stale orderbook one.
        assert!(why.contains("71.09"), "{why}");
    }

    #[test]
    fn the_higher_of_the_two_disagreeing_sources_wins_for_a_sell() {
        // Being generous about what the market would pay makes the check strict on
        // us — the whole failure was believing the pessimistic source.
        let b = book(vec![50.0], vec![]);
        let t = feed(71.09, 71.43, vec![(67.50, 1)]);
        let m = market_view(Some(&b), Some(&t));
        assert_eq!(m.sell_reference(), Some(71.09));
        assert_eq!(m.book_bid, Some(50.0));
    }

    #[test]
    fn no_trade_history_is_itself_a_reason_to_stop() {
        // An empty tape means the price cannot be checked. Silence is not evidence
        // the price is fine — this property traded roughly twice a year.
        let b = book(vec![50.0], vec![60.0]);
        let m = market_view(Some(&b), Some(&feed(50.0, 60.0, vec![])));
        let why = m.objection("sell", 55.0).expect("must object");
        assert!(why.contains("NO trade history"), "{why}");
    }

    #[test]
    fn a_sell_well_under_the_market_is_refused_even_without_crossing() {
        // Below the reference but above the bid: rests rather than crossing, and is
        // still a bad price.
        let m = market_view(
            Some(&book(vec![40.0], vec![])),
            Some(&feed(40.0, 80.0, vec![(70.0, 1)])),
        );
        let why = m.objection("sell", 55.0).expect("must object");
        assert!(why.contains("below the market reference"), "{why}");
    }

    #[test]
    fn buys_are_guarded_the_same_way() {
        let m = market_view(
            Some(&book(vec![50.0], vec![60.0])),
            Some(&feed(50.0, 60.0, vec![(58.0, 1)])),
        );
        assert!(m
            .objection("buy", 60.0)
            .unwrap()
            .contains("AT OR ABOVE the best ask"));
        // Overpaying WITHOUT crossing: the ask is far away so nothing would fill,
        // but the last print says $58, so $75 is still well over the market. The
        // crossing check fires first when both apply, hence a separate book here.
        let over = market_view(
            Some(&book(vec![50.0], vec![100.0])),
            Some(&feed(50.0, 100.0, vec![(58.0, 1)])),
        );
        let why = over.objection("buy", 75.0).expect("must object");
        assert!(why.contains("above the market reference"), "{why}");
    }

    #[test]
    fn a_fairly_priced_resting_order_passes() {
        // Sell above the bid and within tolerance of the reference: exactly the
        // recovery-ask shape we place every day. It must not be blocked.
        let m = market_view(
            Some(&book(vec![62.25], vec![63.70])),
            Some(&feed(62.25, 63.70, vec![(62.0, 1)])),
        );
        assert_eq!(m.objection("sell", 72.80), None);
        assert_eq!(m.objection("buy", 60.00), None);
    }

    #[test]
    fn describe_shows_the_disagreement_rather_than_hiding_it() {
        let b = book(vec![50.0], vec![]);
        let t = feed(71.09, 71.43, vec![(67.50, 1)]);
        let d = market_view(Some(&b), Some(&t)).describe();
        assert!(d.contains("50.00") && d.contains("71.09"), "{d}");
    }

    fn lp_programs() -> Value {
        json!({"programs": [
            {"propertyId": "01PROPA", "minContracts": 4.0, "minTwoSidedLiquidity": 1.0},
            {"propertyId": "01PROPB", "minContracts": 2.0, "minTwoSidedLiquidity": 1.0}
        ]})
    }

    #[test]
    fn sub_min_contracts_order_is_flagged_as_qualifying_but_not_scoring() {
        // The mistake this exists to catch: 3 tokens on a minContracts-4 program.
        // The order rests, qualifies the side, and earns nothing — silently.
        let n = min_contracts_note_from(&lp_programs(), "01PROPA", 3).expect("should warn");
        assert!(n.contains("below"), "{n}");
        assert!(n.contains("QUALIFY"), "{n}");
        assert!(n.contains("NOTHING"), "{n}");
    }

    #[test]
    fn an_order_at_or_above_min_contracts_is_silent() {
        assert!(min_contracts_note_from(&lp_programs(), "01PROPA", 4).is_none());
        assert!(min_contracts_note_from(&lp_programs(), "01PROPA", 9).is_none());
        // Thresholds differ per property: 3 is fine on a minContracts-2 program.
        assert!(min_contracts_note_from(&lp_programs(), "01PROPB", 3).is_none());
    }

    #[test]
    fn a_property_with_no_reward_program_never_warns() {
        // Most properties run no program at all; they must not be nagged.
        assert!(min_contracts_note_from(&lp_programs(), "01NOTINPROGRAM", 1).is_none());
        // Nor should a malformed or empty payload block an order.
        assert!(min_contracts_note_from(&json!({}), "01PROPA", 1).is_none());
        assert!(min_contracts_note_from(&json!({"programs": []}), "01PROPA", 1).is_none());
    }
}
