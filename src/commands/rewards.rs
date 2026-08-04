//! `lofty rewards` — LP-reward (market-making) programs and payout history.
//!
//! This is the heart of the market-making workflow: `programs` shows every
//! property paying for order-book liquidity and its qualification rules;
//! `history` shows what you actually earned.

use clap::Subcommand;
use pk_cli_core::{output, CliError};
use serde_json::Value;

use super::{emit, table_view, Ctx};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// All properties currently paying LP rewards, with program terms.
    #[command(alias = "ls")]
    Programs,
    /// Program terms for a single property (exit 4 if none).
    Program { property_id: String },
    /// Your reward payout history, newest first.
    History {
        /// Only rewards since this Unix-ms timestamp.
        #[arg(long)]
        since: Option<u64>,
        /// Max results per page (max 200).
        #[arg(long)]
        limit: Option<u32>,
        /// Continuation cursor from a previous page.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Reconcile payouts: per-property totals, an internal-consistency check
    /// (amount == percentOfPool x pool-per-block), and gap detection (payout
    /// periods with no reward mid-streak — candidate misses to investigate).
    Reconcile {
        /// Only reconcile rewards since this Unix-ms timestamp.
        #[arg(long)]
        since: Option<u64>,
    },
    /// Are your resting orders actually earning LP rewards right now?
    ///
    /// Checks every open order against the published qualification rules — the
    /// program must be TWO-SIDED (one-sided liquidity earns nothing), each order
    /// at least `minContracts`, priced within `allowedSpread` of the book mid,
    /// and covered by your balances — then projects your share of the pool from
    /// the published score formula, and says why anything earns nothing.
    Eligibility {
        /// Only this property.
        #[arg(long)]
        property_id: Option<String>,
    },
}

pub fn run(ctx: &Ctx, cmd: &Cmd) -> Result<(), CliError> {
    let client = ctx.client()?;
    match cmd {
        Cmd::Programs => {
            let payload = client.get("/public/v1/account/lp-programs", &[])?;
            emit(ctx, "lp-programs", payload, |v| {
                let programs = v
                    .get("programs")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                output::table(&table_view(
                    &programs,
                    &[
                        "propertyId",
                        "dailyRewards",
                        "allowedSpread",
                        "minContracts",
                        "minTwoSidedLiquidity",
                        "slug",
                    ],
                ));
            });
            Ok(())
        }
        Cmd::Program { property_id } => {
            let payload = client.get("/public/v1/account/lp-programs", &[])?;
            let program = payload
                .get("programs")
                .and_then(Value::as_array)
                .and_then(|arr| {
                    arr.iter()
                        .find(|p| p.get("propertyId").and_then(Value::as_str) == Some(property_id))
                })
                .cloned()
                .ok_or_else(|| {
                    CliError::NotFound(format!("no active LP program for {property_id}"))
                })?;
            emit(ctx, "lp-program", program, output::render);
            Ok(())
        }
        Cmd::History {
            since,
            limit,
            cursor,
        } => {
            let mut q: Vec<(&str, String)> = Vec::new();
            if let Some(s) = since {
                q.push(("since", s.to_string()));
            }
            if let Some(n) = limit {
                q.push(("limit", n.to_string()));
            }
            if let Some(c) = cursor {
                q.push(("cursor", c.clone()));
            }
            let payload = client.get("/public/v1/account/lp-rewards", &q)?;
            emit(ctx, "lp-rewards-history", payload, |v| {
                let rewards = v
                    .get("rewards")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                output::table(&rewards);
                if let Some(cursor) = v.get("nextCursor").filter(|c| !c.is_null()) {
                    eprintln!("next cursor: {cursor}");
                }
            });
            Ok(())
        }
        Cmd::Reconcile { since } => {
            // Program terms give the pool-per-block and period length per property.
            let programs = client
                .get("/public/v1/account/lp-programs", &[])?
                .get("programs")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            // Pull the FULL payout history (paginate until the cursor runs out) so
            // totals and gap detection see everything, not just the first page.
            let mut rewards: Vec<Value> = Vec::new();
            let mut cursor: Option<String> = None;
            // Bound the paging: a page cap plus a repeated-cursor break so a
            // misbehaving `nextCursor` can never spin forever (200 * 50 = 10k rows).
            for _ in 0..50 {
                let mut q: Vec<(&str, String)> = vec![("limit", "200".to_string())];
                if let Some(s) = since {
                    q.push(("since", s.to_string()));
                }
                if let Some(c) = &cursor {
                    q.push(("cursor", c.clone()));
                }
                let page = client.get("/public/v1/account/lp-rewards", &q)?;
                if let Some(arr) = page.get("rewards").and_then(Value::as_array) {
                    rewards.extend(arr.iter().cloned());
                }
                match page.get("nextCursor").and_then(Value::as_str) {
                    // Advance only on a fresh cursor; null/absent/repeat → done.
                    Some(c) if Some(c) != cursor.as_deref() => cursor = Some(c.to_string()),
                    _ => break,
                }
            }
            let report = reconcile(&rewards, &programs);
            emit(ctx, "lp-reconcile", report, render_reconcile);
            Ok(())
        }
        Cmd::Eligibility { property_id } => {
            let programs = client
                .get("/public/v1/account/lp-programs", &[])?
                .get("programs")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let orders: Vec<Value> = client
                .get("/public/v1/orders", &[("all", "true".into())])?
                .get("orders")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|o| o.get("status").and_then(Value::as_str) == Some("active"))
                .collect();
            let usdc = client
                .get("/public/v1/account/balance", &[])?
                .get("usdc")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let positions = client
                .get("/public/v1/account/positions", &[])?
                .get("positions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            // Only fetch books for programs we actually have orders on — the mid
            // is meaningless elsewhere and each book is a separate request.
            let mut rows = Vec::new();
            for program in &programs {
                let pid = program
                    .get("propertyId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if property_id.as_deref().is_some_and(|f| f != pid) {
                    continue;
                }
                let mine: Vec<Value> = orders
                    .iter()
                    .filter(|o| o.get("propertyId").and_then(Value::as_str) == Some(pid.as_str()))
                    .cloned()
                    .collect();
                if mine.is_empty() {
                    continue; // not engaged here
                }
                let book = client.get(&format!("/public/v1/properties/{pid}/orderbook"), &[])?;
                let held = positions
                    .iter()
                    .find(|p| p.get("propertyId").and_then(Value::as_str) == Some(pid.as_str()))
                    .and_then(|p| p.get("currentTokens").and_then(Value::as_f64))
                    .unwrap_or(0.0);
                rows.push(eligibility(program, &mine, &book, usdc, held));
            }
            if rows.is_empty() {
                return Err(CliError::NotFound(match property_id {
                    Some(f) => format!("no open orders on an LP-reward property for {f}"),
                    None => "no open orders on any LP-reward property".into(),
                }));
            }
            let daily: f64 = rows
                .iter()
                .filter_map(|r| r.get("expectedDaily").and_then(Value::as_f64))
                .sum();
            let report = serde_json::json!({"properties": rows, "expectedDailyTotal": daily});
            emit(ctx, "lp-eligibility", report, render_eligibility);
            Ok(())
        }
    }
}

/// Best bid/ask from either order-book envelope (SDK 0.2.3+ `bids`/`asks`, or the
/// older `orderBook.buyOrders`/`sellOrders`). Levels aggregate by price.
fn best_of(book: &Value, new_key: &str, old_key: &str, want_max: bool) -> Option<f64> {
    let levels = book
        .pointer(&format!("/orderbook/{new_key}"))
        .or_else(|| book.pointer(&format!("/orderbook/orderBook/{old_key}")))
        .and_then(Value::as_array)?;
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
}

/// Published per-order score (lofty.ai/lp-rewards):
/// `((allowedSpread + 1 - |price - mid|) / (allowedSpread + 1))^2 x quantity`.
/// Rises quadratically the closer the order rests to mid.
fn score_order(dist_from_mid: f64, allowed_spread: f64, quantity: f64) -> f64 {
    let ratio = (allowed_spread + 1.0 - dist_from_mid.abs()) / (allowed_spread + 1.0);
    ratio.max(0.0).powi(2) * quantity
}

/// Evaluate one property's LP-reward eligibility. Pure (unit-tested).
///
/// The published rules all have to hold at once: the position is TWO-SIDED (a
/// bid AND an ask — one-sided liquidity earns nothing), and to SCORE, an order
/// is at least `minContracts`, rests within `allowedSpread` of the book mid, and
/// is covered by live balances (bids by USDC, asks by held tokens).
///
/// Qualifying a SIDE and SCORING are separate steps with DIFFERENT thresholds:
///   * qualifying — a covered order of at least `minTwoSidedLiquidity` shares
///     (1 on every live program) establishes that the side exists. Neither
///     `minContracts` nor the band applies here.
///   * scoring — only in-band orders of at least `minContracts` accrue score.
///
/// So a 1-share ask parked far out of band contributes ZERO score yet still
/// makes the position two-sided, letting the bid's score earn. Collapsing the
/// two thresholds understates eligibility and hides live income.
fn eligibility(program: &Value, mine: &[Value], book: &Value, usdc: f64, held: f64) -> Value {
    let f = |v: &Value, k: &str, d: f64| v.get(k).and_then(Value::as_f64).unwrap_or(d);
    let spread = f(program, "allowedSpread", 0.0);
    let min_contracts = f(program, "minContracts", 0.0);
    let min_two_sided = f(program, "minTwoSidedLiquidity", 0.0);
    let daily = f(program, "dailyRewards", 0.0);
    let pid = program
        .get("propertyId")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let (best_bid, best_ask) = (
        best_of(book, "bids", "buyOrders", true),
        best_of(book, "asks", "sellOrders", false),
    );
    let Some(mid) = best_bid.zip(best_ask).map(|(b, a)| (b + a) / 2.0) else {
        return serde_json::json!({
            "propertyId": pid, "earning": false,
            "reason": "order book is one-sided or empty — no mid to measure against",
            "orders": [], "expectedDaily": 0.0,
        });
    };

    // Per-property coverage: all our bids against USDC, all our asks against tokens.
    let bid_usd: f64 = mine
        .iter()
        .filter(|o| o.get("direction").and_then(Value::as_str) == Some("buy"))
        .map(|o| f(o, "price", 0.0) * f(o, "quantity", 0.0))
        .sum();
    let ask_qty: f64 = mine
        .iter()
        .filter(|o| o.get("direction").and_then(Value::as_str) == Some("sell"))
        .map(|o| f(o, "quantity", 0.0))
        .sum();
    let (bids_covered, asks_covered) = (bid_usd <= usdc, ask_qty <= held);

    let mut rows = Vec::new();
    let (mut bid_shares, mut ask_shares, mut our_score) = (0.0, 0.0, 0.0);
    for o in mine {
        let is_buy = o.get("direction").and_then(Value::as_str) == Some("buy");
        let (price, qty) = (f(o, "price", 0.0), f(o, "quantity", 0.0));
        let dist = (price - mid).abs();
        let in_band = dist <= spread;
        let big_enough = qty >= min_contracts;
        let covered = if is_buy { bids_covered } else { asks_covered };
        // Being COVERED qualifies the side; the bar is `minTwoSidedLiquidity`
        // (1 on every live program), NOT `minContracts`. Gating this on
        // `big_enough` conflated the two thresholds and reported properties as
        // one-sided that Lofty was actually paying — see the regression test.
        if covered {
            if is_buy {
                bid_shares += qty;
            } else {
                ask_shares += qty;
            }
        }
        let scoring = in_band && big_enough && covered;
        if scoring {
            our_score += score_order(dist, spread, qty);
        }
        let mut why = Vec::new();
        if !in_band {
            why.push("out of band");
        }
        if !big_enough {
            why.push("below minContracts");
        }
        if !covered {
            why.push("not covered");
        }
        rows.push(serde_json::json!({
            "direction": if is_buy { "buy" } else { "sell" },
            "price": price, "quantity": qty,
            "distFromMid": dist, "inBand": in_band,
            "meetsMinContracts": big_enough, "covered": covered,
            "scoring": scoring,
            "issues": why,
        }));
    }

    let two_sided = bid_shares >= min_two_sided.max(1.0) && ask_shares >= min_two_sided.max(1.0);
    let earning = two_sided && our_score > 0.0;
    let reason = if earning {
        None
    } else if !two_sided {
        Some(if bid_shares < min_two_sided.max(1.0) {
            "not two-sided: no qualifying BID (one-sided liquidity earns nothing)"
        } else {
            "not two-sided: no qualifying ASK (one-sided liquidity earns nothing)"
        })
    } else {
        Some("two-sided, but no order is in-band and sized, so nothing scores")
    };

    // Competition: every other in-band, sized level on the book. Our own resting
    // size is part of those levels, so subtract it to avoid counting it twice.
    let side_score = |new_key: &str, old_key: &str| -> f64 {
        book.pointer(&format!("/orderbook/{new_key}"))
            .or_else(|| book.pointer(&format!("/orderbook/orderBook/{old_key}")))
            .and_then(Value::as_array)
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(|l| Some((l.get("price")?.as_f64()?, l.get("quantity")?.as_f64()?)))
                    .filter(|(p, q)| *q >= min_contracts && (p - mid).abs() <= spread)
                    .map(|(p, q)| score_order(p - mid, spread, q))
                    .sum()
            })
            .unwrap_or(0.0)
    };
    let book_score = side_score("bids", "buyOrders") + side_score("asks", "sellOrders");
    let competing = (book_score - our_score).max(0.0);
    let total = our_score + competing;
    let share = if earning && total > 0.0 {
        our_score / total
    } else {
        0.0
    };

    serde_json::json!({
        "propertyId": pid,
        "name": program.pointer("/address/line2").and_then(Value::as_str).unwrap_or(pid),
        "mid": mid, "band": [mid - spread, mid + spread],
        "twoSided": two_sided, "earning": earning, "reason": reason,
        "ourScore": our_score, "competingScore": competing,
        "expectedShare": share, "expectedDaily": share * daily,
        "orders": rows,
    })
}

/// Human-readable eligibility: one line per property with the verdict, then the
/// specific reason and the offending orders whenever it is earning nothing.
fn render_eligibility(v: &Value) {
    let props = v
        .get("properties")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for p in &props {
        let g = |k: &str| p.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        let name = p.get("name").and_then(Value::as_str).unwrap_or("?");
        if p.get("earning").and_then(Value::as_bool) == Some(true) {
            println!(
                "{name}: EARNING — {:.1}% of pool \u{2248} ${:.2}/day (mid ${:.2}, competing score {:.1})",
                g("expectedShare") * 100.0,
                g("expectedDaily"),
                g("mid"),
                g("competingScore"),
            );
        } else {
            println!(
                "{name}: $0 — {}",
                p.get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("not eligible"),
            );
        }
        for o in p.get("orders").and_then(Value::as_array).unwrap_or(&vec![]) {
            let issues = o
                .get("issues")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            if issues.is_empty() {
                continue;
            }
            println!(
                "    {} ${:.2} x{} — {issues}",
                o.get("direction").and_then(Value::as_str).unwrap_or("?"),
                o.get("price").and_then(Value::as_f64).unwrap_or(0.0),
                o.get("quantity").and_then(Value::as_f64).unwrap_or(0.0),
            );
        }
    }
    eprintln!(
        "projected total: ${:.2}/day across {} propert{}",
        v.get("expectedDailyTotal")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        props.len(),
        if props.len() == 1 { "y" } else { "ies" },
    );
}

/// Format a Unix-ms timestamp as `YYYY-MM-DD HH:MM UTC` without a date
/// dependency (civil-from-days, per Howard Hinnant's algorithm).
fn fmt_period(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (h, mi) = (secs % 86400 / 3600, secs % 3600 / 60);
    let z = secs.div_euclid(86400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02} UTC")
}

/// Reconcile LP-reward payouts against program terms. Pure (unit-tested): groups
/// payouts by property, sums earnings, verifies each payout equals
/// `percentOfPool x (dailyRewards / blocksPerDay)`, and flags hourly periods with
/// no payout between a property's first and last payout (candidate misses).
fn reconcile(rewards: &[Value], programs: &[Value]) -> Value {
    use std::collections::BTreeMap;
    let f = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64);
    let u = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64);
    let s = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
    };

    // propertyId -> (pool per block, period length ms, display name)
    let mut prog: BTreeMap<String, (f64, u64, String)> = BTreeMap::new();
    for p in programs {
        let pid = s(p, "propertyId");
        let daily = f(p, "dailyRewards").unwrap_or(10.0);
        let blocks = f(p, "blocksPerDay").unwrap_or(24.0);
        let pool = if blocks > 0.0 { daily / blocks } else { daily };
        let period = u(p, "blockDurationMs").unwrap_or(3_600_000);
        let name = p
            .pointer("/address/line2")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| pid.clone());
        prog.insert(pid, (pool, period, name));
    }

    let mut by: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for r in rewards {
        by.entry(s(r, "propertyId")).or_default().push(r);
    }

    let (mut props, mut tot_earn, mut tot_pay, mut tot_gap) = (Vec::new(), 0.0, 0usize, 0usize);
    for (pid, mut rows) in by {
        rows.sort_by_key(|r| u(r, "periodStart").unwrap_or(0));
        let (pool, period, name) =
            prog.get(&pid)
                .cloned()
                .unwrap_or((10.0 / 24.0, 3_600_000, pid.clone()));
        let earned: f64 = rows.iter().filter_map(|r| f(r, "amount")).sum();
        let n = rows.len();
        let avg_pct = if n > 0 {
            rows.iter()
                .filter_map(|r| f(r, "percentOfPool"))
                .sum::<f64>()
                / n as f64
        } else {
            0.0
        };
        // Internal consistency: amount / (pct/100) should equal the per-block pool.
        let inconsistent = rows
            .iter()
            .filter(|r| {
                let (a, p) = (
                    f(r, "amount").unwrap_or(0.0),
                    f(r, "percentOfPool").unwrap_or(0.0),
                );
                p > 0.0 && (a / (p / 100.0) - pool).abs() > 0.01
            })
            .count();
        // Gaps: missing period boundaries between the first and last payout.
        let mut periods: Vec<u64> = rows.iter().filter_map(|r| u(r, "periodStart")).collect();
        periods.sort_unstable();
        periods.dedup();
        let mut gaps: Vec<u64> = Vec::new();
        for w in periods.windows(2) {
            let Some(steps) = (w[1] - w[0]).checked_div(period) else {
                continue;
            };
            for k in 1..steps {
                gaps.push(w[0] + k * period);
            }
        }
        tot_earn += earned;
        tot_pay += n;
        tot_gap += gaps.len();
        props.push(serde_json::json!({
            "propertyId": pid, "name": name, "payouts": n, "earned": earned,
            "avgPercentOfPool": avg_pct,
            "firstPeriod": periods.first().copied().unwrap_or(0),
            "lastPeriod": periods.last().copied().unwrap_or(0),
            "inconsistentPayouts": inconsistent, "gaps": gaps,
        }));
    }
    props.sort_by(|a, b| {
        f(b, "earned")
            .unwrap_or(0.0)
            .total_cmp(&f(a, "earned").unwrap_or(0.0))
    });
    serde_json::json!({
        "properties": props, "totalEarned": tot_earn,
        "totalPayouts": tot_pay, "totalGaps": tot_gap,
    })
}

/// Human-readable reconciliation: a per-property summary table, then any
/// consistency mismatches and gap periods called out explicitly.
fn render_reconcile(v: &Value) {
    let props = v
        .get("properties")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let rows: Vec<Value> = props
        .iter()
        .map(|p| {
            serde_json::json!({
                "property": p.get("name"),
                "payouts": p.get("payouts"),
                "earned": format!("${:.4}", p.get("earned").and_then(Value::as_f64).unwrap_or(0.0)),
                "avg%pool": format!("{:.1}", p.get("avgPercentOfPool").and_then(Value::as_f64).unwrap_or(0.0)),
                "gaps": p.get("gaps").and_then(Value::as_array).map_or(0, Vec::len),
                "mismatch": p.get("inconsistentPayouts"),
            })
        })
        .collect();
    output::table(&rows);
    eprintln!(
        "total: ${:.4} across {} payout(s); {} gap period(s)",
        v.get("totalEarned").and_then(Value::as_f64).unwrap_or(0.0),
        v.get("totalPayouts").and_then(Value::as_u64).unwrap_or(0),
        v.get("totalGaps").and_then(Value::as_u64).unwrap_or(0),
    );
    for p in &props {
        let name = p.get("name").and_then(Value::as_str).unwrap_or("?");
        if let Some(gaps) = p
            .get("gaps")
            .and_then(Value::as_array)
            .filter(|g| !g.is_empty())
        {
            let when: Vec<String> = gaps
                .iter()
                .filter_map(Value::as_u64)
                .map(fmt_period)
                .collect();
            eprintln!(
                "  \u{26a0} {name}: {} period(s) with no payout — verify eligibility: {}",
                gaps.len(),
                when.join(", ")
            );
        }
        if p.get("inconsistentPayouts")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0
        {
            eprintln!("  \u{26a0} {name}: some payouts don't match percentOfPool x pool — report to Lofty");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HR: u64 = 3_600_000;
    const T0: u64 = 1_784_764_800_000; // 2026-07-23 00:00 UTC

    fn programs() -> Vec<Value> {
        vec![json!({
            "propertyId": "01SAMPLEPROP0000000000001",
            "dailyRewards": 10, "blocksPerDay": 24, "blockDurationMs": HR,
            "address": {"line2": "Sample City, ST 00000"}
        })]
    }
    // Pool per block = 10/24 = 0.416666..., so amount = pct/100 * that.
    fn payout(period: u64, pct: f64) -> Value {
        json!({
            "propertyId": "01SAMPLEPROP0000000000001",
            "periodStart": period, "percentOfPool": pct,
            "amount": pct / 100.0 * (10.0 / 24.0),
        })
    }

    #[test]
    fn sums_earnings_and_averages_pool_share() {
        let rewards = vec![payout(T0, 20.0), payout(T0 + HR, 10.0)];
        let r = reconcile(&rewards, &programs());
        let p = &r["properties"][0];
        assert_eq!(p["payouts"], 2);
        assert!((p["earned"].as_f64().unwrap() - (30.0 / 100.0 * 10.0 / 24.0)).abs() < 1e-9);
        assert!((p["avgPercentOfPool"].as_f64().unwrap() - 15.0).abs() < 1e-9);
        assert_eq!(r["totalPayouts"], 2);
    }

    #[test]
    fn detects_a_missing_period_mid_streak() {
        // periods at T0, T0+1h, then T0+3h → T0+2h is the gap.
        let rewards = vec![
            payout(T0, 10.0),
            payout(T0 + HR, 10.0),
            payout(T0 + 3 * HR, 10.0),
        ];
        let r = reconcile(&rewards, &programs());
        let gaps = r["properties"][0]["gaps"].as_array().unwrap();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].as_u64().unwrap(), T0 + 2 * HR);
        assert_eq!(r["totalGaps"], 1);
    }

    #[test]
    fn flags_a_payout_inconsistent_with_the_pool() {
        // amount doesn't match 15% of the 0.41666 pool → one mismatch.
        let bad = json!({
            "propertyId": "01SAMPLEPROP0000000000001",
            "periodStart": T0, "percentOfPool": 15.0, "amount": 999.0,
        });
        let r = reconcile(&[bad], &programs());
        assert_eq!(r["properties"][0]["inconsistentPayouts"], 1);
    }

    #[test]
    fn fmt_period_is_utc_civil() {
        assert_eq!(fmt_period(T0), "2026-07-23 00:00 UTC");
        assert_eq!(fmt_period(T0 + 13 * HR), "2026-07-23 13:00 UTC");
    }
}

#[cfg(test)]
mod eligibility_tests {
    use super::*;
    use serde_json::json;

    const P: &str = "01SAMPLEPROPERTY000000000A";

    fn program() -> Value {
        json!({"propertyId": P, "dailyRewards": 10.0, "allowedSpread": 2.0,
               "minContracts": 4.0, "minTwoSidedLiquidity": 1.0,
               "address": {"line2": "Sample City, ST 00000"}})
    }
    /// Book with a $50/$52 inside market → mid $51, band [$49, $53].
    fn book(extra_bids: Vec<Value>, extra_asks: Vec<Value>) -> Value {
        let mut bids = vec![json!({"price": 50.0, "quantity": 4.0})];
        let mut asks = vec![json!({"price": 52.0, "quantity": 4.0})];
        bids.extend(extra_bids);
        asks.extend(extra_asks);
        json!({"orderbook": {"bids": bids, "asks": asks}})
    }
    fn order(dir: &str, price: f64, qty: f64) -> Value {
        json!({"propertyId": P, "direction": dir, "price": price,
               "quantity": qty, "status": "active"})
    }

    #[test]
    fn two_sided_in_band_and_covered_earns() {
        let mine = [order("buy", 50.0, 4.0), order("sell", 52.0, 4.0)];
        let r = eligibility(&program(), &mine, &book(vec![], vec![]), 1000.0, 10.0);
        assert_eq!(r["earning"], true);
        assert_eq!(r["twoSided"], true);
        assert!(r["reason"].is_null());
        assert!(r["mid"].as_f64().unwrap() - 51.0 < 1e-9);
        assert!(r["expectedDaily"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn a_tiny_out_of_band_ask_still_qualifies_the_side() {
        // Regression, confirmed against observed payouts: a 4-share in-band bid
        // plus a 1-share ask parked far out of band — the ask failing BOTH
        // minContracts and the band. This shape reports as "not two-sided, $0"
        // if the two thresholds are conflated, yet the program pays it: the
        // bid's score earns because minTwoSidedLiquidity (1), not minContracts,
        // decides whether a side exists.
        let mine = [order("buy", 50.0, 4.0), order("sell", 60.0, 1.0)];
        let r = eligibility(&program(), &mine, &book(vec![], vec![]), 1000.0, 10.0);
        assert_eq!(r["twoSided"], true);
        assert_eq!(r["earning"], true);
        assert!(r["expectedDaily"].as_f64().unwrap() > 0.0);
        // ...while contributing no score of its own.
        let ask = r["orders"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["direction"] == "sell")
            .cloned()
            .unwrap();
        assert_eq!(ask["scoring"], false);
        assert_eq!(ask["meetsMinContracts"], false);
        assert_eq!(ask["inBand"], false);
    }

    #[test]
    fn a_bid_alone_earns_nothing_however_good_it_is() {
        // The single most expensive mistake: one-sided liquidity earns NOTHING,
        // even resting exactly at mid with plenty of size and cover.
        let mine = [order("buy", 51.0, 10.0)];
        let r = eligibility(&program(), &mine, &book(vec![], vec![]), 1000.0, 10.0);
        assert_eq!(r["earning"], false);
        assert_eq!(r["twoSided"], false);
        assert!(r["reason"].as_str().unwrap().contains("no qualifying ASK"));
        assert_eq!(r["expectedDaily"], 0.0);
    }

    #[test]
    fn an_ask_alone_earns_nothing_either() {
        let mine = [order("sell", 51.0, 10.0)];
        let r = eligibility(&program(), &mine, &book(vec![], vec![]), 1000.0, 10.0);
        assert_eq!(r["twoSided"], false);
        assert!(r["reason"].as_str().unwrap().contains("no qualifying BID"));
    }

    #[test]
    fn an_undersized_order_qualifies_its_side_but_never_scores() {
        // 2 tokens against minContracts 4. This previously asserted the side did
        // NOT qualify. That was wrong and it was expensive: properties showed as
        // earning $0 while Lofty was paying them hourly. minContracts gates
        // SCORE; minTwoSidedLiquidity (1) gates whether the side exists.
        let mine = [order("buy", 50.0, 4.0), order("sell", 52.0, 2.0)];
        let r = eligibility(&program(), &mine, &book(vec![], vec![]), 1000.0, 10.0);
        assert_eq!(r["twoSided"], true);
        assert_eq!(r["earning"], true); // the in-band bid scores
        let ask = &r["orders"][1];
        assert_eq!(ask["meetsMinContracts"], false);
        assert_eq!(ask["scoring"], false); // the undersized ask contributes none
        assert!(ask["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i == "below minContracts"));
    }

    #[test]
    fn an_uncovered_bid_is_reported_and_stops_the_reward() {
        // Bid needs $200 but the wallet holds $50 → uncovered, so no qualifying bid.
        let mine = [order("buy", 50.0, 4.0), order("sell", 52.0, 4.0)];
        let r = eligibility(&program(), &mine, &book(vec![], vec![]), 50.0, 10.0);
        assert_eq!(r["orders"][0]["covered"], false);
        assert_eq!(r["earning"], false);
        assert!(r["reason"].as_str().unwrap().contains("no qualifying BID"));
    }

    #[test]
    fn an_ask_beyond_held_tokens_is_uncovered() {
        let mine = [order("buy", 50.0, 4.0), order("sell", 52.0, 4.0)];
        let r = eligibility(&program(), &mine, &book(vec![], vec![]), 1000.0, 1.0);
        assert_eq!(r["orders"][1]["covered"], false);
        assert!(r["reason"].as_str().unwrap().contains("no qualifying ASK"));
    }

    #[test]
    fn out_of_band_orders_qualify_the_side_but_never_score() {
        // Ask at $70 is far outside [$49, $53]: it still establishes the ask side
        // (sized + covered), but only the in-band bid accrues score.
        let mine = [order("buy", 50.0, 4.0), order("sell", 70.0, 4.0)];
        let r = eligibility(&program(), &mine, &book(vec![], vec![]), 1000.0, 10.0);
        assert_eq!(r["twoSided"], true);
        assert_eq!(r["earning"], true);
        assert_eq!(r["orders"][1]["inBand"], false);
        assert_eq!(r["orders"][1]["scoring"], false);
        assert_eq!(r["orders"][0]["scoring"], true);
    }

    #[test]
    fn score_rises_quadratically_toward_mid() {
        // Published formula: ((spread + 1 - dist) / (spread + 1))^2 * qty.
        assert!((score_order(0.0, 2.0, 1.0) - 1.0).abs() < 1e-9); // at mid → full weight
        assert!((score_order(1.0, 2.0, 1.0) - (2.0f64 / 3.0).powi(2)).abs() < 1e-9);
        assert!((score_order(3.0, 2.0, 1.0) - 0.0).abs() < 1e-9); // past the band → 0
                                                                  // Doubling size doubles score; halving distance more than doubles it.
        assert!((score_order(1.0, 2.0, 2.0) - 2.0 * score_order(1.0, 2.0, 1.0)).abs() < 1e-9);
        assert!(score_order(0.5, 2.0, 1.0) / score_order(1.0, 2.0, 1.0) > 1.0);
    }

    #[test]
    fn competition_excludes_our_own_resting_size() {
        // We are the entire book, so nothing competes and the pool is all ours.
        let mine = [order("buy", 50.0, 4.0), order("sell", 52.0, 4.0)];
        let r = eligibility(&program(), &mine, &book(vec![], vec![]), 1000.0, 10.0);
        assert_eq!(r["competingScore"], 0.0);
        assert!((r["expectedShare"].as_f64().unwrap() - 1.0).abs() < 1e-9);
        assert!((r["expectedDaily"].as_f64().unwrap() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_crowded_book_shrinks_our_share() {
        let mine = [order("buy", 50.0, 4.0), order("sell", 52.0, 4.0)];
        let crowded = book(
            vec![json!({"price": 50.5, "quantity": 40.0})],
            vec![json!({"price": 51.5, "quantity": 40.0})],
        );
        let r = eligibility(&program(), &mine, &crowded, 1000.0, 10.0);
        assert!(r["competingScore"].as_f64().unwrap() > 0.0);
        assert!(
            r["expectedShare"].as_f64().unwrap() < 0.2,
            "share {:?}",
            r["expectedShare"]
        );
    }

    #[test]
    fn a_one_sided_book_has_no_mid_to_measure_against() {
        let only_bids =
            json!({"orderbook": {"bids": [{"price": 50.0, "quantity": 4.0}], "asks": []}});
        let mine = [order("buy", 50.0, 4.0)];
        let r = eligibility(&program(), &mine, &only_bids, 1000.0, 10.0);
        assert_eq!(r["earning"], false);
        assert!(r["reason"].as_str().unwrap().contains("one-sided or empty"));
    }

    #[test]
    fn reads_the_legacy_orderbook_envelope() {
        let legacy = json!({"orderbook": {"orderBook": {
            "buyOrders": [{"price": 50.0, "quantity": 4.0}],
            "sellOrders": [{"price": 52.0, "quantity": 4.0}]}}});
        let mine = [order("buy", 50.0, 4.0), order("sell", 52.0, 4.0)];
        let r = eligibility(&program(), &mine, &legacy, 1000.0, 10.0);
        assert!((r["mid"].as_f64().unwrap() - 51.0).abs() < 1e-9);
        assert_eq!(r["earning"], true);
    }
}
