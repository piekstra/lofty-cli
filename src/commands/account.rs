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
    /// Maker rebates earned on your resting-order fills.
    ///
    /// Lofty advertises "maker rebates — paid at fill" but documents no rate.
    /// Verified empirically against paid rebates: it is 50% of the platform fee
    /// on YOUR side of the trade, paid only when your order was the RESTING one.
    /// Cross the book (take liquidity) and you earn nothing.
    ///
    /// Derived entirely from your own trade history — the rebate figures are not
    /// otherwise exposed by the API.
    Rebates {
        /// Only this property.
        #[arg(long)]
        property_id: Option<String>,
        /// Only trades at or after this Unix-ms timestamp.
        #[arg(long)]
        since: Option<u64>,
    },
    /// Fee-inclusive break-even sell price for your holdings.
    ///
    /// A position's `costBasis` is what you PAID FOR THE TOKENS — it excludes the
    /// platform buy fee that was charged on top, so it is NOT your true cost, and
    /// selling at it books a loss. Break-even also has to clear the sell fee on
    /// the way out. This computes both: true cost = costBasis x (1 + buy fee),
    /// then the ask that nets it after the sell fee, plus any target margin.
    ///
    /// Fee rates are read live from the property (marketplace) and its AMM pool,
    /// so the numbers reflect the venue you actually trade on.
    Breakeven {
        /// Only this property (default: every position you hold).
        #[arg(long)]
        property_id: Option<String>,
        /// Price a hypothetical cost per token instead of your real position.
        #[arg(long, value_name = "USD")]
        cost: Option<f64>,
        /// Net profit to target above true cost, in percent (default 0 = break even).
        #[arg(long, value_name = "PCT", default_value_t = 0.0)]
        margin: f64,
        /// Where you would sell: the order book, or into the AMM pool.
        #[arg(long, value_name = "VENUE", default_value = "book")]
        sell_venue: SellVenue,
        /// Buy fee already paid, in percent. Overrides the both-venues breakdown.
        #[arg(long, value_name = "PCT")]
        buy_fee: Option<f64>,
        /// Sell fee to clear, in percent. Overrides the venue's published rate.
        #[arg(long, value_name = "PCT")]
        sell_fee: Option<f64>,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SellVenue {
    /// Marketplace limit order (`mtSellFeePct`).
    Book,
    /// Swap into the AMM pool (`fees.platformSell` PLUS the pool's `fees.lp`).
    Amm,
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
        Cmd::Rebates { property_id, since } => {
            let mut q: Vec<(&str, String)> = vec![("limit", "200".to_string())];
            if let Some(id) = property_id {
                q.push(("propertyId", id.clone()));
            }
            let trades = client
                .get("/public/v1/account/trades", &q)?
                .get("trades")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            // Full order history (including filled ones) is what distinguishes a
            // maker fill from a taker fill — the trade record itself says nothing.
            let orders = client
                .get("/public/v1/orders", &[("all", "true".to_string())])?
                .get("orders")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let report = rebates(&trades, &orders, *since);
            emit(ctx, "account-rebates", report, render_rebates);
            Ok(())
        }
        Cmd::Breakeven {
            property_id,
            cost,
            margin,
            sell_venue,
            buy_fee,
            sell_fee,
        } => {
            for (name, v) in [
                ("--cost", cost),
                ("--buy-fee", buy_fee),
                ("--sell-fee", sell_fee),
            ] {
                if v.is_some_and(|x| !x.is_finite() || x < 0.0) {
                    return Err(CliError::Usage(format!(
                        "{name} must be a non-negative number"
                    )));
                }
            }
            if !margin.is_finite() || *margin < 0.0 {
                return Err(CliError::Usage(
                    "--margin must be a non-negative percent".into(),
                ));
            }
            if sell_fee.is_some_and(|f| f >= 100.0) {
                return Err(CliError::Usage(
                    "--sell-fee must be below 100% (a 100% fee has no break-even price)".into(),
                ));
            }

            // Pools carry AMM fee rates for every property in one call.
            let pools = client
                .get("/public/v1/amm/pools", &[])?
                .get("pools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            // Which positions to price: an explicit --cost needs a property only
            // for its fee rates; otherwise every position holding tokens.
            let mut targets: Vec<(String, f64)> = Vec::new();
            match (cost, property_id) {
                (Some(c), Some(pid)) => targets.push((pid.clone(), *c)),
                (Some(_), None) => {
                    return Err(CliError::Usage(
                        "--cost also needs --property-id (fee rates are per property)".into(),
                    ))
                }
                (None, filter) => {
                    let positions = client
                        .get("/public/v1/account/positions", &[])?
                        .get("positions")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    for p in &positions {
                        let pid = p
                            .get("propertyId")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if filter.as_deref().is_some_and(|f| f != pid) {
                            continue;
                        }
                        if p.get("currentTokens")
                            .and_then(Value::as_f64)
                            .unwrap_or(0.0)
                            <= 0.0
                        {
                            continue;
                        }
                        // `costBasis` is per token, in cents.
                        let basis =
                            p.get("costBasis").and_then(Value::as_f64).unwrap_or(0.0) / 100.0;
                        targets.push((pid.to_string(), basis));
                    }
                    if targets.is_empty() {
                        return Err(CliError::NotFound(match filter {
                            Some(f) => format!("no position holding tokens for {f}"),
                            None => "no positions holding tokens".into(),
                        }));
                    }
                }
            }

            let mut rows = Vec::new();
            for (pid, basis) in targets {
                let listing = client.get(&format!("/public/v1/properties/{pid}"), &[])?;
                let pool = pools
                    .iter()
                    .find(|p| p.get("propertyId").and_then(Value::as_str) == Some(pid.as_str()));
                let fees = venue_fees(&listing, pool);
                rows.push(breakeven(
                    &pid,
                    basis,
                    &fees,
                    *margin,
                    *sell_venue,
                    *buy_fee,
                    *sell_fee,
                ));
            }
            let report = serde_json::json!({"marginPct": margin, "positions": rows});
            emit(ctx, "account-breakeven", report, render_breakeven);
            Ok(())
        }
    }
}

/// Published fee rates for a property, normalized to PERCENT.
///
/// The two sources disagree on units, which is easy to get wrong: the property
/// listing reports fractions (`mtSellFeePct: 0.035` = 3.5%) while the AMM pool
/// reports percentages (`fees.platformSell: 2.5` = 2.5%).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct VenueFees {
    pub book_buy: Option<f64>,
    pub book_sell: Option<f64>,
    pub amm_buy: Option<f64>,
    pub amm_sell: Option<f64>,
}

pub fn venue_fees(listing: &Value, pool: Option<&Value>) -> VenueFees {
    // `properties/{id}` wraps its payload in `property`; accept a bare listing too,
    // the same tolerance the orderbook parser carries for its envelope change.
    let frac = |field: &str| {
        [
            format!("/property/liquidity/{field}"),
            format!("/liquidity/{field}"),
        ]
        .iter()
        .find_map(|p| listing.pointer(p).and_then(Value::as_f64))
        // Scaling a binary float by 100 leaves visible dirt (0.035 →
        // 3.4999999999999996) that would surface verbatim in --json. Rates are
        // published to a few decimals, so snap off the representation error.
        .map(|v| (v * 100.0 * 1e6).round() / 1e6) // listing fractions → percent
    };
    let pct = |k: &str| {
        pool.and_then(|p| p.pointer(&format!("/fees/{k}")))
            .and_then(Value::as_f64) // pool values are already percent
    };
    // A swap pays the platform fee AND the pool's LP fee — the pool reports them
    // as separate lines, so the all-in rate is their sum. Confirmed against live
    // quotes: fees.total/usdcAmount = 5.00% on a buy (platform 3 + lp 2) and
    // ~5.5% on a sell (platform 3.5 + lp 2). Taking `platform` alone understates
    // every AMM figure by the LP fee. The order book has no LP fee — that fee
    // compensates AMM liquidity providers and has no order-book analogue — so the
    // marketplace rates stand alone.
    let amm = |k: &str| match (pct(k), pct("lp").unwrap_or(0.0)) {
        (Some(platform), lp) => Some(platform + lp),
        (None, _) => None,
    };
    VenueFees {
        book_buy: frac("mtBuyFeePct"),
        book_sell: frac("mtSellFeePct"),
        amm_buy: amm("platformBuy"),
        amm_sell: amm("platformSell"),
    }
}

/// The ask that nets `margin` percent above true cost after the sell fee.
///
/// Two fees bracket a round trip and both must be paid out of the sale:
///   trueCost = costBasis x (1 + buyFee/100)     <- the buy fee already charged
///   ask      = trueCost x (1 + margin/100) / (1 - sellFee/100)
///
/// `costBasis` is what you paid for the tokens and EXCLUDES the platform buy
/// fee, so pricing off it (or off any figure that omits the fee) books a loss.
pub fn ask_for(
    cost_basis: f64,
    buy_fee_pct: f64,
    sell_fee_pct: f64,
    margin_pct: f64,
) -> (f64, f64) {
    let true_cost = cost_basis * (1.0 + buy_fee_pct / 100.0);
    let net = 1.0 - sell_fee_pct / 100.0;
    // A fee at/above 100% leaves nothing to net; report an unreachable price
    // rather than a negative one that looks like a bargain.
    let ask = if net > 0.0 {
        true_cost * (1.0 + margin_pct / 100.0) / net
    } else {
        f64::INFINITY
    };
    (true_cost, ask)
}

/// Price one position. Pure (unit-tested).
///
/// The buy fee you actually paid depends on where you bought, which the API does
/// not record — so unless `--buy-fee` says otherwise, both venues are priced and
/// labelled rather than silently guessing one.
#[allow(clippy::too_many_arguments)]
fn breakeven(
    property_id: &str,
    cost_basis: f64,
    fees: &VenueFees,
    margin_pct: f64,
    sell_venue: SellVenue,
    buy_fee: Option<f64>,
    sell_fee: Option<f64>,
) -> Value {
    let (venue_name, published_sell) = match sell_venue {
        SellVenue::Book => ("book", fees.book_sell),
        SellVenue::Amm => ("amm", fees.amm_sell),
    };
    let sell_fee_pct = sell_fee.or(published_sell);

    // Candidate acquisition costs: an explicit override, else each venue we know.
    let candidates: Vec<(&str, Option<f64>)> = match buy_fee {
        Some(f) => vec![("specified", Some(f))],
        None => vec![("amm", fees.amm_buy), ("book", fees.book_buy)],
    };

    let scenarios: Vec<Value> = candidates
        .into_iter()
        .filter_map(|(via, bf)| Some((via, bf?, sell_fee_pct?)))
        .map(|(via, bf, sf)| {
            let (true_cost, ask) = ask_for(cost_basis, bf, sf, margin_pct);
            let (_, breakeven_ask) = ask_for(cost_basis, bf, sf, 0.0);
            serde_json::json!({
                "acquiredVia": via, "buyFeePct": bf, "trueCost": true_cost,
                "breakEvenAsk": breakeven_ask, "targetAsk": ask,
            })
        })
        .collect();

    serde_json::json!({
        "propertyId": property_id,
        "costBasis": cost_basis,
        "sellVenue": venue_name,
        "sellFeePct": sell_fee_pct,
        "scenarios": scenarios,
    })
}

/// Human-readable break-even: one block per position, a row per acquisition
/// venue, with the cost-basis caveat stated up front.
fn render_breakeven(v: &Value) {
    let margin = v.get("marginPct").and_then(Value::as_f64).unwrap_or(0.0);
    let positions = v
        .get("positions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for p in &positions {
        let pid = p.get("propertyId").and_then(Value::as_str).unwrap_or("?");
        let basis = p.get("costBasis").and_then(Value::as_f64).unwrap_or(0.0);
        let venue = p.get("sellVenue").and_then(Value::as_str).unwrap_or("?");
        match p.get("sellFeePct").and_then(Value::as_f64) {
            Some(sf) => eprintln!(
                "{pid}: cost basis ${basis:.2}/token (excludes the buy fee) — selling on {venue} at {sf:.2}% fee",
            ),
            None => {
                eprintln!("{pid}: no published {venue} sell fee — pass --sell-fee to price it");
                continue;
            }
        }
        let rows: Vec<Value> = p
            .get("scenarios")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|s| {
                let g = |k: &str| s.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                serde_json::json!({
                    "acquired via": s.get("acquiredVia"),
                    "buy fee %": format!("{:.2}", g("buyFeePct")),
                    "true cost": format!("${:.2}", g("trueCost")),
                    "break-even ask": format!("${:.2}", g("breakEvenAsk")),
                    format!("ask +{margin:.1}%"): format!("${:.2}", g("targetAsk")),
                })
            })
            .collect();
        if rows.is_empty() {
            eprintln!("  (no published buy-fee rate — pass --buy-fee to price it)");
        } else {
            output::table(&rows);
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

#[cfg(test)]
mod rebate_tests {
    use super::*;
    use serde_json::json;

    const P: &str = "01SAMPLEPROPERTY000000000A";

    fn trade(dir: &str, price: f64, buyer_fee: f64, seller_fee: f64, at: u64) -> Value {
        json!({"tradeId": "T1", "propertyId": P, "direction": dir, "price": price,
               "quantity": 1, "createdAt": at,
               "buyerFeeAmount": buyer_fee * MICRO, "sellerFeeAmount": seller_fee * MICRO})
    }
    fn order(dir: &str, price: f64) -> Value {
        json!({"propertyId": P, "direction": dir, "price": price})
    }

    #[test]
    fn reproduces_the_rebates_actually_paid() {
        // Verified against real payouts: a $62.25 buy with a $1.5562 buyer fee
        // rebated $0.78, and a $52.75 sell with a $1.5825 seller fee rebated $0.79.
        let trades = [
            trade("buy", 62.25, 1.55625, 1.8675, 3),
            trade("sell", 52.75, 1.31875, 1.5825, 2),
        ];
        let orders = [order("buy", 62.25), order("sell", 52.75)];
        let r = rebates(&trades, &orders, None);
        let got: Vec<String> = r["trades"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| format!("{:.2}", t["rebate"].as_f64().unwrap()))
            .collect();
        assert_eq!(got, vec!["0.78", "0.79"]);
        assert!((r["totalRebates"].as_f64().unwrap() - 1.57).abs() < 0.005);
        assert_eq!(r["makerFills"], 2);
    }

    #[test]
    fn a_taker_fill_earns_nothing() {
        // Crossing the book pays no rebate — the real corroboration was a sell that
        // crossed and received nothing while resting fills the same week were paid.
        let trades = [trade("sell", 71.09, 1.0, 2.0, 1)];
        let r = rebates(&trades, &[], None); // no resting order at that price
        assert_eq!(r["totalRebates"], 0.0);
        assert_eq!(r["takerFills"], 1);
        assert_eq!(r["trades"][0]["wasMaker"], false);
    }

    #[test]
    fn the_fee_taken_is_the_one_on_our_side() {
        // A buy rebates off the BUYER fee, a sell off the SELLER fee. Mixing them up
        // silently misprices every rebate.
        let buy = rebates(
            &[trade("buy", 10.0, 4.0, 8.0, 1)],
            &[order("buy", 10.0)],
            None,
        );
        assert_eq!(buy["trades"][0]["rebate"], 2.0);
        let sell = rebates(
            &[trade("sell", 10.0, 4.0, 8.0, 1)],
            &[order("sell", 10.0)],
            None,
        );
        assert_eq!(sell["trades"][0]["rebate"], 4.0);
    }

    #[test]
    fn an_opposite_side_order_does_not_count_as_maker() {
        // A resting BID does not make a SELL a maker fill, even at the same price.
        let r = rebates(
            &[trade("sell", 62.25, 1.0, 2.0, 1)],
            &[order("buy", 62.25)],
            None,
        );
        assert_eq!(r["takerFills"], 1);
    }

    #[test]
    fn since_filters_older_trades() {
        let trades = [
            trade("buy", 10.0, 4.0, 8.0, 100),
            trade("buy", 10.0, 4.0, 8.0, 900),
        ];
        let r = rebates(&trades, &[order("buy", 10.0)], Some(500));
        assert_eq!(r["trades"].as_array().unwrap().len(), 1);
        assert_eq!(r["makerFills"], 1);
    }
}

#[cfg(test)]
mod breakeven_tests {
    use super::*;
    use serde_json::json;

    const P: &str = "01SAMPLEPROPERTY000000000A";

    // Rates mirror the live shapes: the listing reports FRACTIONS, the pool PERCENT.
    fn listing() -> Value {
        json!({"liquidity": {"mtBuyFeePct": 0.03, "mtSellFeePct": 0.035, "lpFeePct": 0.02}})
    }
    fn pool() -> Value {
        // Real observed shape: the platform and LP fees are separate lines.
        json!({"propertyId": P, "fees": {"lp": 2.0, "platformBuy": 3.0, "platformSell": 3.5}})
    }
    fn fees() -> VenueFees {
        venue_fees(&listing(), Some(&pool()))
    }
    fn scenario<'a>(v: &'a Value, via: &str) -> &'a Value {
        v["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["acquiredVia"] == via)
            .unwrap_or_else(|| panic!("no `{via}` scenario"))
    }

    #[test]
    fn normalizes_the_two_fee_unit_conventions_to_percent() {
        // The listing's 0.035 and the pool's 2.5 both mean "percent" after this.
        let f = fees();
        assert_eq!(f.book_buy, Some(3.0));
        assert_eq!(f.book_sell, Some(3.5));
        // AMM rates are all-in: platform + the pool's LP fee.
        assert_eq!(f.amm_buy, Some(5.0)); // 3 + 2
        assert_eq!(f.amm_sell, Some(5.5)); // 3.5 + 2
    }

    #[test]
    fn amm_rates_include_the_pools_lp_fee() {
        // A swap pays platform AND lp; the pool lists them separately. Taking
        // `platform` alone understated every AMM figure by the lp fee. Verified
        // against live quotes: fees.total/usdcAmount = 5.00% buy, ~5.5% sell.
        let f = fees();
        assert_eq!(f.amm_buy, Some(5.0), "buy = platform 3 + lp 2");
        assert_eq!(f.amm_sell, Some(5.5), "sell = platform 3.5 + lp 2");
        // A pool with no lp line is just the platform rate, not a missing rate.
        let no_lp = json!({"propertyId": P, "fees": {"platformBuy": 3.0, "platformSell": 3.5}});
        assert_eq!(venue_fees(&listing(), Some(&no_lp)).amm_buy, Some(3.0));
        // No platform rate at all is genuinely unknown.
        let empty = json!({"propertyId": P, "fees": {"lp": 2.0}});
        assert_eq!(venue_fees(&listing(), Some(&empty)).amm_buy, None);
    }

    #[test]
    fn reads_fees_through_the_property_envelope() {
        // `properties/{id}` returns {"property": {...}} — reading a bare
        // `/liquidity` finds nothing and silently drops the marketplace rates.
        let wrapped = json!({"property": listing()});
        let f = venue_fees(&wrapped, Some(&pool()));
        assert_eq!(f.book_sell, Some(3.5), "book fees lost inside the envelope");
        assert_eq!(f.book_buy, Some(3.0));
        // A bare listing still works.
        assert_eq!(venue_fees(&listing(), None).book_sell, Some(3.5));
    }

    #[test]
    fn true_cost_adds_the_buy_fee_the_basis_leaves_out() {
        // $100 basis + 5% AMM buy fee = $105 actually spent per token.
        let (true_cost, _) = ask_for(100.0, 5.0, 0.0, 0.0);
        assert!((true_cost - 105.0).abs() < 1e-9);
    }

    #[test]
    fn break_even_ask_clears_the_sell_fee() {
        // Selling at true cost loses the sell fee; the ask must gross it up.
        let (_, ask) = ask_for(100.0, 5.0, 3.5, 0.0);
        assert!((ask - 105.0 / 0.965).abs() < 1e-9);
        // Net proceeds land exactly back on true cost.
        assert!((ask * 0.965 - 105.0).abs() < 1e-9);
    }

    #[test]
    fn margin_is_net_of_every_fee() {
        let (true_cost, ask) = ask_for(100.0, 5.0, 3.5, 5.0);
        let net = ask * 0.965; // proceeds after the sell fee
        assert!(
            (net - true_cost * 1.05).abs() < 1e-9,
            "net {net} vs {true_cost}"
        );
    }

    #[test]
    fn prices_both_acquisition_venues_when_the_buy_fee_is_unknown() {
        let v = breakeven(P, 100.0, &fees(), 0.0, SellVenue::Book, None, None);
        assert_eq!(v["scenarios"].as_array().unwrap().len(), 2);
        assert_eq!(scenario(&v, "amm")["buyFeePct"], 5.0);
        assert_eq!(scenario(&v, "book")["buyFeePct"], 3.0);
        // A pricier acquisition needs a higher exit.
        assert!(
            scenario(&v, "amm")["breakEvenAsk"].as_f64().unwrap()
                > scenario(&v, "book")["breakEvenAsk"].as_f64().unwrap()
        );
    }

    #[test]
    fn an_explicit_buy_fee_collapses_to_one_scenario() {
        let v = breakeven(P, 100.0, &fees(), 0.0, SellVenue::Book, Some(0.0), None);
        let s = v["scenarios"].as_array().unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0]["acquiredVia"], "specified");
        // buy fee 0 → true cost IS the basis.
        assert!((s[0]["trueCost"].as_f64().unwrap() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn selling_into_the_amm_uses_the_pool_fee() {
        let book = breakeven(P, 100.0, &fees(), 0.0, SellVenue::Book, Some(5.0), None);
        let amm = breakeven(P, 100.0, &fees(), 0.0, SellVenue::Amm, Some(5.0), None);
        assert_eq!(book["sellFeePct"], 3.5); // mtSellFeePct
        assert_eq!(amm["sellFeePct"], 5.5); // platformSell 3.5 + lp 2
                                            // Swapping out costs more than resting an ask, so it needs a higher price.
        assert!(
            amm["scenarios"][0]["breakEvenAsk"].as_f64().unwrap()
                > book["scenarios"][0]["breakEvenAsk"].as_f64().unwrap()
        );
    }

    #[test]
    fn an_explicit_sell_fee_overrides_the_published_rate() {
        let v = breakeven(
            P,
            100.0,
            &fees(),
            0.0,
            SellVenue::Book,
            Some(0.0),
            Some(10.0),
        );
        assert_eq!(v["sellFeePct"], 10.0);
        assert!((v["scenarios"][0]["breakEvenAsk"].as_f64().unwrap() - 100.0 / 0.9).abs() < 1e-9);
    }

    #[test]
    fn missing_published_rates_yield_no_scenarios_rather_than_wrong_ones() {
        // No pool and no listing fees: nothing to price off, so say nothing.
        let bare = venue_fees(&json!({}), None);
        let v = breakeven(P, 100.0, &bare, 0.0, SellVenue::Book, None, None);
        assert!(v["sellFeePct"].is_null());
        assert!(v["scenarios"].as_array().unwrap().is_empty());
    }

    #[test]
    fn reproduces_a_worked_position() {
        // $63.68 basis, bought via AMM (5%), sold on the book (3.5%):
        // true cost $66.86; break-even $69.29; a 5% net target $72.75.
        let v = breakeven(P, 63.68, &fees(), 5.0, SellVenue::Book, Some(5.0), None);
        let s = &v["scenarios"][0];
        assert_eq!(format!("{:.2}", s["trueCost"].as_f64().unwrap()), "66.86");
        assert_eq!(
            format!("{:.2}", s["breakEvenAsk"].as_f64().unwrap()),
            "69.29"
        );
        assert_eq!(format!("{:.2}", s["targetAsk"].as_f64().unwrap()), "72.75");
    }

    #[test]
    fn a_total_sell_fee_has_no_reachable_break_even() {
        let (_, ask) = ask_for(100.0, 0.0, 100.0, 0.0);
        assert!(ask.is_infinite(), "expected no finite price, got {ask}");
    }
}

/// Maker rebate as a fraction of the fee paid on your side of a trade.
///
/// Lofty advertises the rebate but publishes no rate (see the project's LOF-11).
/// Verified against three paid rebates, exact to the cent: a $62.25 buy carrying a
/// $1.5562 buyer fee rebated $0.78, and a $52.75 sell carrying a $1.5825 seller
/// fee rebated $0.79. A taker fill — crossing the book — rebated nothing, which
/// matches "paid at fill" meaning paid to the RESTING side.
const MAKER_REBATE_SHARE: f64 = 0.5;

/// Micro-USDC → USD. Fee amounts arrive in micro-units while prices are plain USD.
const MICRO: f64 = 1e6;

/// Reconstruct maker rebates from trade history. Pure (unit-tested).
///
/// A trade record carries no maker/taker flag, so it is inferred: a fill counts as
/// MAKER when one of our orders on that property rested at that exact price. That
/// is the same evidence a human would use, and it is conservative — an
/// unmatched trade is reported as a taker (no rebate) rather than credited
/// optimistically.
fn rebates(trades: &[Value], orders: &[Value], since: Option<u64>) -> Value {
    let f = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let s = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let u = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);

    let mut rows = Vec::new();
    let (mut total, mut maker_count, mut taker_count) = (0.0, 0usize, 0usize);
    for t in trades {
        if since.is_some_and(|c| u(t, "createdAt") < c) {
            continue;
        }
        let pid = s(t, "propertyId");
        let price = f(t, "price");
        let buying = s(t, "direction") == "buy";
        // Our fee is the one on our side of the book.
        let our_fee = if buying {
            f(t, "buyerFeeAmount") / MICRO
        } else {
            f(t, "sellerFeeAmount") / MICRO
        };
        let was_maker = orders.iter().any(|o| {
            s(o, "propertyId") == pid
                && s(o, "direction") == if buying { "buy" } else { "sell" }
                && (f(o, "price") - price).abs() < 0.005
        });
        let rebate = if was_maker {
            our_fee * MAKER_REBATE_SHARE
        } else {
            0.0
        };
        if was_maker {
            maker_count += 1;
        } else {
            taker_count += 1;
        }
        total += rebate;
        rows.push(serde_json::json!({
            "tradeId": t.get("tradeId"), "propertyId": pid,
            "direction": if buying { "buy" } else { "sell" },
            "price": price, "quantity": f(t, "quantity"),
            "createdAt": u(t, "createdAt"),
            "ourFee": our_fee, "wasMaker": was_maker, "rebate": rebate,
        }));
    }
    rows.sort_by_key(|r| {
        std::cmp::Reverse(r.get("createdAt").and_then(Value::as_u64).unwrap_or(0))
    });
    serde_json::json!({
        "rebateShareOfFee": MAKER_REBATE_SHARE,
        "totalRebates": total,
        "makerFills": maker_count,
        "takerFills": taker_count,
        "trades": rows,
    })
}

/// Human view: the rebate-earning fills, then the ones that earned nothing and why.
fn render_rebates(v: &Value) {
    let rows = v
        .get("trades")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let earners: Vec<Value> = rows
        .iter()
        .filter(|r| r.get("wasMaker").and_then(Value::as_bool) == Some(true))
        .map(|r| {
            let g = |k: &str| r.get(k).and_then(Value::as_f64).unwrap_or(0.0);
            serde_json::json!({
                "property": r.get("propertyId"),
                "side": r.get("direction"),
                "price": format!("${:.2}", g("price")),
                "qty": g("quantity"),
                "your fee": format!("${:.4}", g("ourFee")),
                "rebate": format!("${:.4}", g("rebate")),
            })
        })
        .collect();
    if earners.is_empty() {
        println!("no maker fills — rebates are paid only when YOUR order was the resting one");
    } else {
        output::table(&earners);
    }
    eprintln!(
        "total ${:.4} across {} maker fill(s) at {:.0}% of your side's fee; {} taker fill(s) earned nothing (crossing the book pays no rebate)",
        v.get("totalRebates").and_then(Value::as_f64).unwrap_or(0.0),
        v.get("makerFills").and_then(Value::as_u64).unwrap_or(0),
        v.get("rebateShareOfFee").and_then(Value::as_f64).unwrap_or(0.0) * 100.0,
        v.get("takerFills").and_then(Value::as_u64).unwrap_or(0),
    );
}
