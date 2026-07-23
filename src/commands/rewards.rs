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
    }
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
