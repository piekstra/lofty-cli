//! `lofty account` — balances, positions, and executed trade history.

use clap::Subcommand;
use pk_cli_core::{output, CliError};
use serde_json::Value;

use super::{emit, table_view, Ctx};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// USDC / ALGO / rent / gift balances.
    Balance,
    /// Token holdings by property, with cost basis and P&L inputs.
    Positions,
    /// Executed trades (completed buys and sells).
    Trades {
        /// Filter to one property.
        #[arg(long)]
        property_id: Option<String>,
        /// buy or sell.
        #[arg(long)]
        direction: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// AMM LP positions and unclaimed rewards (upstream 500 at capture time).
    LpPositions,
    /// Order coverage: USDC reserved backing your resting bids vs free to spend,
    /// and whether every open order is currently covered.
    ///
    /// Lofty funds open orders from your live balances — buys need the USDC and
    /// sells need the shares, across all your open orders on a property
    /// COMBINED, and orders your wallet can't cover are canceled automatically.
    /// Coverage is checked per property, not in aggregate, so the wallet floor
    /// every bid must clear is your LARGEST single-property bid total: that much
    /// is reserved, and only the surplus above it is free to spend.
    Coverage {
        /// Simulate spending this much USDC (e.g. buying tokens) and report
        /// which properties' bids it would leave uncovered.
        #[arg(long, value_name = "USD")]
        spend: Option<f64>,
    },
}

pub fn run(ctx: &Ctx, cmd: &Cmd) -> Result<(), CliError> {
    let client = ctx.client()?;
    match cmd {
        Cmd::Balance => {
            let payload = client.get("/public/v1/account/balance", &[])?;
            emit(ctx, "account-balance", payload, output::render);
            Ok(())
        }
        Cmd::Positions => {
            let payload = client.get("/public/v1/account/positions", &[])?;
            emit(ctx, "account-positions", payload, |v| {
                let positions = v
                    .get("positions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                output::table(&table_view(
                    &positions,
                    &[
                        "propertyId",
                        "currentTokens",
                        "costBasis",
                        "currentValue",
                        "totalRentEarned",
                    ],
                ));
                if let Some(totals) = v.get("totals") {
                    output::render(totals);
                }
            });
            Ok(())
        }
        Cmd::Trades {
            property_id,
            direction,
            limit,
        } => {
            let mut q: Vec<(&str, String)> = Vec::new();
            if let Some(id) = property_id {
                q.push(("propertyId", id.clone()));
            }
            if let Some(d) = direction {
                q.push(("direction", d.clone()));
            }
            if let Some(n) = limit {
                q.push(("limit", n.to_string()));
            }
            let payload = client.get("/public/v1/account/trades", &q)?;
            emit(ctx, "account-trades", payload, |v| {
                let trades = v
                    .get("trades")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                output::table(&table_view(
                    &trades,
                    &["propertyId", "direction", "price", "quantity", "createdAt"],
                ));
            });
            Ok(())
        }
        Cmd::LpPositions => {
            let payload = client.get("/public/v1/account/lp-positions", &[])?;
            emit(ctx, "account-lp-positions", payload, output::render);
            Ok(())
        }
        Cmd::Coverage { spend } => {
            if spend.is_some_and(|s| !s.is_finite() || s < 0.0) {
                return Err(CliError::Usage(
                    "--spend must be a non-negative USD amount".into(),
                ));
            }
            let usdc = client
                .get("/public/v1/account/balance", &[])?
                .get("usdc")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let orders = client
                .get("/public/v1/orders", &[("all", "true".into())])?
                .get("orders")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let positions = client
                .get("/public/v1/account/positions", &[])?
                .get("positions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let report = coverage(usdc, &orders, &positions, *spend);
            emit(ctx, "account-coverage", report, render_coverage);
            Ok(())
        }
    }
}

/// Compute order coverage from a wallet balance, open orders, and holdings.
/// Pure (unit-tested).
///
/// Lofty covers open orders from live balances, per property and across that
/// property's open orders combined: its buys must be covered by wallet USDC and
/// its sells by held tokens, or the order is canceled automatically. Because the
/// check is per property rather than aggregate, several properties' bids can
/// lean on the same USDC at once — so the binding reserve is the LARGEST
/// single-property bid total, and only the surplus above it is spendable without
/// dropping some property's bid below its cover.
///
/// `bidExposure` (the sum across properties) is therefore NOT the reserve; it is
/// what you would owe if every bid filled, and it legitimately exceeds the
/// wallet when quoting several properties. That is worth surfacing, because one
/// large fill spends cash the other properties' bids were also counting on.
fn coverage(usdc: f64, orders: &[Value], positions: &[Value], spend: Option<f64>) -> Value {
    use std::collections::BTreeMap;
    let f = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let s = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    let held: BTreeMap<String, f64> = positions
        .iter()
        .map(|p| (s(p, "propertyId"), f(p, "currentTokens")))
        .collect();

    // Per property: USD needed to cover its bids, tokens needed to cover its asks.
    let mut by: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    for o in orders.iter().filter(|o| s(o, "status") == "active") {
        let e = by.entry(s(o, "propertyId")).or_insert((0.0, 0.0));
        match s(o, "direction").as_str() {
            "buy" => e.0 += f(o, "price") * f(o, "quantity"),
            "sell" => e.1 += f(o, "quantity"),
            _ => {}
        }
    }

    let reserved = by.values().map(|(bid, _)| *bid).fold(0.0, f64::max);
    let exposure: f64 = by.values().map(|(bid, _)| *bid).sum();
    let free = (usdc - reserved).max(0.0);

    let mut rows = Vec::new();
    let (mut uncovered_bids, mut uncovered_asks) = (0usize, 0usize);
    for (pid, (bid_usd, ask_qty)) in &by {
        let tokens = held.get(pid).copied().unwrap_or(0.0);
        let bid_covered = *bid_usd <= usdc;
        let ask_covered = *ask_qty <= tokens;
        if !bid_covered {
            uncovered_bids += 1;
        }
        if !ask_covered {
            uncovered_asks += 1;
        }
        rows.push(serde_json::json!({
            "propertyId": pid, "bidUsd": bid_usd, "bidCovered": bid_covered,
            "askQty": ask_qty, "heldTokens": tokens, "askCovered": ask_covered,
        }));
    }
    rows.sort_by(|a, b| f(b, "bidUsd").total_cmp(&f(a, "bidUsd")));

    let mut out = serde_json::json!({
        "walletUsdc": usdc,
        "reservedUsdc": reserved,
        "freeUsdc": free,
        "bidExposureUsdc": exposure,
        "overCommitted": exposure > usdc,
        "uncoveredBids": uncovered_bids,
        "uncoveredAsks": uncovered_asks,
        "properties": rows,
    });

    // Spending cash lowers the wallet every property's bids are checked against,
    // so a spend can cascade — uncovering bids on properties you weren't touching.
    if let Some(spend) = spend {
        let after = usdc - spend;
        let uncovers: Vec<Value> = by
            .iter()
            .filter(|(_, (bid, _))| *bid > 0.0 && *bid > after)
            .map(|(pid, (bid, _))| serde_json::json!({"propertyId": pid, "bidUsd": bid}))
            .collect();
        out["simulation"] = serde_json::json!({
            "spend": spend,
            "walletAfter": after,
            "fits": spend <= free,
            "uncovers": uncovers,
        });
    }
    out
}

/// Human-readable coverage: the headline capital line, then a per-property table
/// and explicit warnings for anything uncovered or at risk from a simulated spend.
fn render_coverage(v: &Value) {
    let g = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let props = v
        .get("properties")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let rows: Vec<Value> = props
        .iter()
        .map(|p| {
            let gf = |k: &str| p.get(k).and_then(Value::as_f64).unwrap_or(0.0);
            serde_json::json!({
                "property": p.get("propertyId"),
                "bid $": format!("{:.2}", gf("bidUsd")),
                "bid covered": p.get("bidCovered"),
                "ask qty": gf("askQty"),
                "held": gf("heldTokens"),
                "ask covered": p.get("askCovered"),
            })
        })
        .collect();
    output::table(&rows);
    eprintln!(
        "wallet ${:.2} — ${:.2} reserved (largest single-property bid), ${:.2} free to spend",
        g("walletUsdc"),
        g("reservedUsdc"),
        g("freeUsdc"),
    );
    if v.get("overCommitted").and_then(Value::as_bool) == Some(true) {
        eprintln!(
            "  note: ${:.2} total bid exposure exceeds the wallet — fine while they rest (coverage is per property), but one fill spends cash the others also rely on",
            g("bidExposureUsdc"),
        );
    }
    for p in &props {
        let pid = p.get("propertyId").and_then(Value::as_str).unwrap_or("?");
        if p.get("bidCovered").and_then(Value::as_bool) == Some(false) {
            eprintln!(
                "  \u{26a0} {pid}: bids exceed your wallet — at risk of automatic cancellation"
            );
        }
        if p.get("askCovered").and_then(Value::as_bool) == Some(false) {
            eprintln!("  \u{26a0} {pid}: asks exceed your held tokens — at risk of automatic cancellation");
        }
    }
    if let Some(sim) = v.get("simulation") {
        let sf = |k: &str| sim.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        eprintln!(
            "spending ${:.2} leaves ${:.2} — {}",
            sf("spend"),
            sf("walletAfter"),
            if sim.get("fits").and_then(Value::as_bool) == Some(true) {
                "fits within free capital"
            } else {
                "EXCEEDS free capital"
            },
        );
        for u in sim
            .get("uncovers")
            .and_then(Value::as_array)
            .unwrap_or(&vec![])
        {
            eprintln!(
                "  \u{26a0} would uncover {} (bids ${:.2})",
                u.get("propertyId").and_then(Value::as_str).unwrap_or("?"),
                u.get("bidUsd").and_then(Value::as_f64).unwrap_or(0.0),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const A: &str = "01SAMPLEPROPERTY000000000A";
    const B: &str = "01SAMPLEPROPERTY000000000B";

    fn bid(pid: &str, price: f64, qty: f64) -> Value {
        json!({"propertyId": pid, "direction": "buy", "price": price,
               "quantity": qty, "status": "active"})
    }
    fn ask(pid: &str, price: f64, qty: f64) -> Value {
        json!({"propertyId": pid, "direction": "sell", "price": price,
               "quantity": qty, "status": "active"})
    }
    fn position(pid: &str, tokens: f64) -> Value {
        json!({"propertyId": pid, "currentTokens": tokens})
    }

    #[test]
    fn reserve_is_the_largest_single_property_bid_not_the_sum() {
        // Coverage is checked per property, so two $100 bids both clear a $150
        // wallet: the reserve is $100 (the max), leaving $50 free — NOT $200/$0.
        let orders = [bid(A, 50.0, 2.0), bid(B, 100.0, 1.0)];
        let r = coverage(150.0, &orders, &[], None);
        assert_eq!(r["reservedUsdc"], 100.0);
        assert_eq!(r["freeUsdc"], 50.0);
        assert_eq!(r["bidExposureUsdc"], 200.0);
        assert_eq!(r["overCommitted"], true);
        assert_eq!(r["uncoveredBids"], 0);
    }

    #[test]
    fn combines_orders_on_the_same_property() {
        // Same property: its bids are covered COMBINED, so they sum into one reserve.
        let orders = [bid(A, 50.0, 1.0), bid(A, 30.0, 1.0)];
        let r = coverage(150.0, &orders, &[], None);
        assert_eq!(r["reservedUsdc"], 80.0);
        assert_eq!(r["freeUsdc"], 70.0);
        assert_eq!(r["overCommitted"], false);
    }

    #[test]
    fn flags_a_bid_the_wallet_cannot_cover() {
        let r = coverage(40.0, &[bid(A, 50.0, 1.0)], &[], None);
        assert_eq!(r["uncoveredBids"], 1);
        assert_eq!(r["properties"][0]["bidCovered"], false);
        assert_eq!(r["freeUsdc"], 0.0); // clamped, never negative
    }

    #[test]
    fn asks_are_covered_by_held_tokens_not_usdc() {
        let orders = [ask(A, 60.0, 4.0), ask(B, 60.0, 4.0)];
        let positions = [position(A, 4.0), position(B, 1.0)];
        let r = coverage(0.0, &orders, &positions, None);
        assert_eq!(r["uncoveredAsks"], 1); // B holds 1 token against a 4-token ask
        assert_eq!(r["reservedUsdc"], 0.0); // asks reserve no USDC
        let b = r["properties"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["propertyId"] == B)
            .unwrap()
            .clone();
        assert_eq!(b["askCovered"], false);
        assert_eq!(b["heldTokens"], 1.0);
    }

    #[test]
    fn ignores_orders_that_are_not_active() {
        let orders = [
            bid(A, 50.0, 1.0),
            json!({"propertyId": B, "direction": "buy", "price": 999.0,
                   "quantity": 1.0, "status": "cancelled"}),
        ];
        let r = coverage(150.0, &orders, &[], None);
        assert_eq!(r["reservedUsdc"], 50.0);
        assert_eq!(r["properties"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn simulated_spend_reports_the_cascade_onto_other_properties() {
        // $150 wallet, bids of $100 (A) and $90 (B): $50 free. Spending $80 drops
        // the wallet to $70 — uncovering BOTH, including the one not being touched.
        let orders = [bid(A, 100.0, 1.0), bid(B, 90.0, 1.0)];
        let r = coverage(150.0, &orders, &[], Some(80.0));
        let sim = &r["simulation"];
        assert_eq!(sim["fits"], false); // $80 > $50 free
        assert_eq!(sim["walletAfter"], 70.0);
        assert_eq!(sim["uncovers"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_spend_within_free_capital_uncovers_nothing() {
        let orders = [bid(A, 100.0, 1.0)];
        let r = coverage(150.0, &orders, &[], Some(50.0));
        assert_eq!(r["simulation"]["fits"], true);
        assert!(r["simulation"]["uncovers"].as_array().unwrap().is_empty());
    }

    #[test]
    fn empty_account_is_all_free_and_never_over_committed() {
        let r = coverage(25.0, &[], &[], None);
        assert_eq!(r["reservedUsdc"], 0.0);
        assert_eq!(r["freeUsdc"], 25.0);
        assert_eq!(r["overCommitted"], false);
        assert!(r["properties"].as_array().unwrap().is_empty());
    }
}
