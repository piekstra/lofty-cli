//! `lofty quote` — safe primitives for moving a two-sided market-making quote.
//!
//! These are MECHANISM, not strategy: you supply the target prices, and the
//! command performs the move safely. It decides nothing about *where* to quote.
//!
//! Every subcommand is a DRY RUN by default — it prints the plan and changes
//! nothing until `--execute`. The dangerous thing requires a flag; the safe
//! thing happens when you forget one.

use clap::Subcommand;
use pk_cli_core::CliError;
use serde_json::{json, Value};

use super::{confirm, emit, Ctx};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Move a resting quote to new prices, safely.
    ///
    /// Cancels and re-posts ONLY the sides you give a price for, after checking
    /// that each target is in the reward band, covered by your balances, at
    /// least `minContracts`, and does not cross the market. Sides you omit are
    /// left untouched — so re-centering a bid can never orphan an earning ask.
    ///
    /// DRY RUN unless `--execute`.
    Recenter {
        #[arg(long)]
        property_id: String,
        /// New bid price. Omit to leave the bid alone.
        #[arg(long, value_name = "USD")]
        bid: Option<f64>,
        /// New ask price. Omit to leave the ask alone.
        #[arg(long, value_name = "USD")]
        ask: Option<f64>,
        /// Bid size (default: keep the resting bid's quantity).
        #[arg(long, value_name = "N")]
        bid_qty: Option<u32>,
        /// Ask size (default: keep the resting ask's quantity).
        #[arg(long, value_name = "N")]
        ask_qty: Option<u32>,
        /// Refuse to place an ask below this price (e.g. your break-even).
        #[arg(long, value_name = "USD")]
        min_ask: Option<f64>,
        /// Allow a target outside the reward band (it will earn nothing).
        #[arg(long)]
        allow_out_of_band: bool,
        /// Actually place the orders. Without this, nothing is sent.
        #[arg(long)]
        execute: bool,
        /// Skip the confirmation prompt (only meaningful with --execute).
        #[arg(long)]
        force: bool,
    },
    /// Post a fresh two-sided quote on a property.
    ///
    /// Refuses to leave you one-sided — one-sided liquidity earns nothing, so if
    /// either side can't be placed (typically too few tokens to back the ask)
    /// nothing is sent and the shortfall is reported. It never buys inventory
    /// for you; acquiring tokens is a separate, explicit `amm swap`.
    ///
    /// DRY RUN unless `--execute`.
    Provision {
        #[arg(long)]
        property_id: String,
        #[arg(long, value_name = "USD")]
        bid: f64,
        #[arg(long, value_name = "USD")]
        ask: f64,
        /// Tokens per side (default: the program's minContracts).
        #[arg(long, value_name = "N")]
        qty: Option<u32>,
        /// Refuse to place the ask below this price (e.g. your break-even).
        #[arg(long, value_name = "USD")]
        min_ask: Option<f64>,
        /// Actually place the orders. Without this, nothing is sent.
        #[arg(long)]
        execute: bool,
        /// Skip the confirmation prompt (only meaningful with --execute).
        #[arg(long)]
        force: bool,
    },
    /// Cancel resting reward quotes on a property, keeping deliberate holds.
    ///
    /// Use when standing down from a property. `--keep-above` protects orders you
    /// mean to leave resting — typically a cost-basis recovery ask parked above
    /// the band — so pulling a quote never cancels the sell that is waiting to
    /// recover your position.
    ///
    /// DRY RUN unless `--execute`.
    Pull {
        #[arg(long)]
        property_id: String,
        /// Never cancel a SELL at or above this price (protects recovery asks).
        #[arg(long, value_name = "USD")]
        keep_above: Option<f64>,
        /// Limit to one side.
        #[arg(long, value_name = "SIDE")]
        side: Option<Side>,
        /// Actually cancel. Without this, nothing is sent.
        #[arg(long)]
        execute: bool,
        /// Skip the confirmation prompt (only meaningful with --execute).
        #[arg(long)]
        force: bool,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

pub fn run(ctx: &Ctx, cmd: &Cmd) -> Result<(), CliError> {
    match cmd {
        Cmd::Recenter {
            property_id,
            bid,
            ask,
            bid_qty,
            ask_qty,
            min_ask,
            allow_out_of_band,
            execute,
            force,
        } => {
            if bid.is_none() && ask.is_none() {
                return Err(CliError::Usage(
                    "give --bid and/or --ask: a recenter with neither side would do nothing".into(),
                ));
            }
            for (name, v) in [("--bid", bid), ("--ask", ask), ("--min-ask", min_ask)] {
                if v.is_some_and(|p| !p.is_finite() || p < 0.01) {
                    return Err(CliError::Usage(format!("{name} must be at least $0.01")));
                }
            }

            // Only now touch the keychain/network: bad args must fail fast with a
            // usage error (exit 2) instead of an auth error — or a keychain prompt —
            // on a machine with no key stored.
            let client = ctx.client()?;
            let program = client
                .get("/public/v1/account/lp-programs", &[])?
                .get("programs")
                .and_then(Value::as_array)
                .and_then(|a| {
                    a.iter()
                        .find(|p| p.get("propertyId").and_then(Value::as_str) == Some(property_id))
                        .cloned()
                })
                .ok_or_else(|| {
                    CliError::NotFound(format!("no active LP program for {property_id}"))
                })?;
            let mine: Vec<Value> = client
                .get("/public/v1/orders", &[("propertyId", property_id.clone())])?
                .get("orders")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|o| o.get("status").and_then(Value::as_str) == Some("active"))
                .collect();
            let book = client.get(
                &format!("/public/v1/properties/{property_id}/orderbook"),
                &[],
            )?;
            let usdc = client
                .get("/public/v1/account/balance", &[])?
                .get("usdc")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let held = client
                .get("/public/v1/account/positions", &[])?
                .get("positions")
                .and_then(Value::as_array)
                .and_then(|a| {
                    a.iter()
                        .find(|p| p.get("propertyId").and_then(Value::as_str) == Some(property_id))
                        .and_then(|p| p.get("currentTokens").and_then(Value::as_f64))
                })
                .unwrap_or(0.0);

            let plan = plan_recenter(
                &program,
                &mine,
                &book,
                usdc,
                held,
                Targets {
                    bid: *bid,
                    ask: *ask,
                    bid_qty: *bid_qty,
                    ask_qty: *ask_qty,
                    min_ask: *min_ask,
                    allow_out_of_band: *allow_out_of_band,
                },
            )?;

            if !execute {
                emit(ctx, "quote-plan", plan, |v| render_plan(v, Render::DryRun));
                return Ok(());
            }
            let steps = plan
                .get("steps")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            confirm(
                ctx,
                *force,
                &format!(
                    "recenter {property_id}: {} order operation(s) on real money",
                    steps.len()
                ),
            )?;

            // Per side: cancel, then re-post. Cancel-first is deliberate — posting
            // first would double-commit capital and can breach per-property
            // coverage, which gets BOTH orders auto-cancelled. The cost is a brief
            // one-sided window (no atomic modify exists); if the re-post fails we
            // say so loudly, because that window is when nothing is earning.
            let mut done = Vec::new();
            for step in &steps {
                let result = match step.get("op").and_then(Value::as_str) {
                    Some("cancel") => {
                        let id = step
                            .get("orderId")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        client.delete(&format!("/public/v1/orders/{id}"))
                    }
                    Some("create") => client.post(
                        "/public/v1/orders",
                        &json!({
                            "propertyId": property_id,
                            "direction": step.get("direction"),
                            "price": step.get("price"),
                            "quantity": step.get("quantity"),
                        }),
                    ),
                    _ => continue,
                };
                match result {
                    Ok(v) => done.push(json!({"step": step, "ok": true, "result": v})),
                    Err(e) => {
                        // A partial failure is the one moment the operator most needs
                        // to know exactly what landed — a side may now be unquoted and
                        // earning $0. Keep stdout to the single error DTO `fail`
                        // emits (AGENTS.md: one tagged DTO, diagnostics → stderr), so
                        // the detail rides along in the message and the human-readable
                        // breakdown goes to stderr in BOTH modes.
                        for line in partial_failure_lines(&done, step) {
                            eprintln!("{line}");
                        }
                        return Err(CliError::Other(partial_failure_message(
                            &done,
                            step,
                            &e.to_string(),
                        )));
                    }
                }
            }
            emit(ctx, "quote-recentered", json!({"applied": done}), |v| {
                render_plan(v, Render::Applied)
            });
            Ok(())
        }

        Cmd::Provision {
            property_id,
            bid,
            ask,
            qty,
            min_ask,
            execute,
            force,
        } => {
            for (name, v) in [
                ("--bid", Some(*bid)),
                ("--ask", Some(*ask)),
                ("--min-ask", *min_ask),
            ] {
                if v.is_some_and(|p| !p.is_finite() || p < 0.01) {
                    return Err(CliError::Usage(format!("{name} must be at least $0.01")));
                }
            }
            if bid >= ask {
                return Err(CliError::Usage(format!(
                    "--bid ${bid:.2} must be below --ask ${ask:.2}"
                )));
            }
            let client = ctx.client()?;
            let st = fetch_state(&client, property_id)?;
            let plan = plan_provision(&st, *bid, *ask, *qty, *min_ask)?;
            apply_or_show(
                ctx,
                &client,
                property_id,
                plan,
                *execute,
                *force,
                "provision",
            )
        }

        Cmd::Pull {
            property_id,
            keep_above,
            side,
            execute,
            force,
        } => {
            if keep_above.is_some_and(|p| !p.is_finite() || p < 0.0) {
                return Err(CliError::Usage(
                    "--keep-above must be a non-negative price".into(),
                ));
            }
            let client = ctx.client()?;
            let st = fetch_state(&client, property_id)?;
            let plan = plan_pull(&st, *keep_above, *side)?;
            apply_or_show(ctx, &client, property_id, plan, *execute, *force, "pull")
        }
    }
}

/// Everything a quote primitive needs about one property, in one place.
struct State {
    program: Value,
    mine: Vec<Value>,
    book: Value,
    usdc: f64,
    held: f64,
}

fn fetch_state(client: &crate::client::LoftyClient, property_id: &str) -> Result<State, CliError> {
    let program = client
        .get("/public/v1/account/lp-programs", &[])?
        .get("programs")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter()
                .find(|p| p.get("propertyId").and_then(Value::as_str) == Some(property_id))
                .cloned()
        })
        .ok_or_else(|| CliError::NotFound(format!("no active LP program for {property_id}")))?;
    let mine: Vec<Value> = client
        .get(
            "/public/v1/orders",
            &[("propertyId", property_id.to_string())],
        )?
        .get("orders")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|o| o.get("status").and_then(Value::as_str) == Some("active"))
        .collect();
    let book = client.get(
        &format!("/public/v1/properties/{property_id}/orderbook"),
        &[],
    )?;
    let usdc = client
        .get("/public/v1/account/balance", &[])?
        .get("usdc")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let held = client
        .get("/public/v1/account/positions", &[])?
        .get("positions")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter()
                .find(|p| p.get("propertyId").and_then(Value::as_str) == Some(property_id))
                .and_then(|p| p.get("currentTokens").and_then(Value::as_f64))
        })
        .unwrap_or(0.0);
    Ok(State {
        program,
        mine,
        book,
        usdc,
        held,
    })
}

/// Show the plan (dry run) or confirm and apply it. Shared by every primitive so
/// the dry-run default and the partial-failure reporting can't diverge.
fn apply_or_show(
    ctx: &Ctx,
    client: &crate::client::LoftyClient,
    property_id: &str,
    plan: Value,
    execute: bool,
    force: bool,
    what: &str,
) -> Result<(), CliError> {
    if !execute {
        emit(ctx, "quote-plan", plan, |v| render_plan(v, Render::DryRun));
        return Ok(());
    }
    let steps = plan
        .get("steps")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if steps.is_empty() {
        // --execute was honoured; the plan was simply empty.
        emit(ctx, "quote-noop", plan, |v| render_plan(v, Render::NoOp));
        return Ok(());
    }
    confirm(
        ctx,
        force,
        &format!(
            "{what} {property_id}: {} order operation(s) on real money",
            steps.len()
        ),
    )?;
    let mut done = Vec::new();
    for step in &steps {
        let result = match step.get("op").and_then(Value::as_str) {
            Some("cancel") => {
                let id = step
                    .get("orderId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                client.delete(&format!("/public/v1/orders/{id}"))
            }
            Some("create") => client.post(
                "/public/v1/orders",
                &json!({
                    "propertyId": property_id,
                    "direction": step.get("direction"),
                    "price": step.get("price"),
                    "quantity": step.get("quantity"),
                }),
            ),
            _ => continue,
        };
        match result {
            Ok(v) => done.push(json!({"step": step, "ok": true, "result": v})),
            Err(e) => {
                for line in partial_failure_lines(&done, step) {
                    eprintln!("{line}");
                }
                return Err(CliError::Other(partial_failure_message(
                    &done,
                    step,
                    &e.to_string(),
                )));
            }
        }
    }
    emit(ctx, "quote-applied", json!({"applied": done}), |v| {
        render_plan(v, Render::Applied)
    });
    Ok(())
}

/// Shared book/market derivation: mid from the FULL book (what Lofty measures
/// the reward band against) and best bid/ask with our own size removed (what a
/// crossing check must compare to).
fn market_from(program: &Value, mine: &[Value], book: &Value) -> Result<Market, CliError> {
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let levels = |new_key: &str, old_key: &str| -> Vec<(f64, f64)> {
        book.pointer(&format!("/orderbook/{new_key}"))
            .or_else(|| book.pointer(&format!("/orderbook/orderBook/{old_key}")))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|l| Some((l.get("price")?.as_f64()?, l.get("quantity")?.as_f64()?)))
                    .collect()
            })
            .unwrap_or_default()
    };
    let full_bid = levels("bids", "buyOrders")
        .iter()
        .map(|l| l.0)
        .fold(f64::NEG_INFINITY, f64::max);
    let full_ask = levels("asks", "sellOrders")
        .iter()
        .map(|l| l.0)
        .fold(f64::INFINITY, f64::min);
    if !full_bid.is_finite() || !full_ask.is_finite() {
        return Err(CliError::Other(
            "order book is one-sided or empty — no mid to measure the band against".into(),
        ));
    }
    let mut bid_levels = levels("bids", "buyOrders");
    let mut ask_levels = levels("asks", "sellOrders");
    let subtract = |lv: &mut Vec<(f64, f64)>, dir: &str| {
        for o in mine
            .iter()
            .filter(|o| o.get("direction").and_then(Value::as_str) == Some(dir))
        {
            let (p, q) = (num(o, "price"), num(o, "quantity"));
            if let Some(l) = lv.iter_mut().find(|l| (l.0 - p).abs() < 0.005) {
                l.1 -= q;
            }
        }
        lv.retain(|l| l.1 > 0.0);
    };
    subtract(&mut bid_levels, "buy");
    subtract(&mut ask_levels, "sell");
    Ok(Market {
        mid: (full_bid + full_ask) / 2.0,
        spread: num(program, "allowedSpread"),
        min_contracts: num(program, "minContracts"),
        mkt_best_bid: bid_levels
            .iter()
            .map(|l| l.0)
            .fold(f64::NEG_INFINITY, f64::max),
        mkt_best_ask: ask_levels.iter().map(|l| l.0).fold(f64::INFINITY, f64::min),
    })
}

/// Plan a fresh two-sided quote. Pure (unit-tested).
///
/// Refuses to produce a ONE-SIDED result: one-sided liquidity earns nothing, so
/// if either leg fails a rail — most often too few tokens to back the ask —
/// nothing is planned at all and the caller is told what is short. Placing the
/// half that works would spend capital for $0.
///
/// It never buys inventory. Acquiring tokens moves real money through a
/// different venue and stays an explicit, separate `amm swap`.
fn plan_provision(
    st: &State,
    bid: f64,
    ask: f64,
    qty: Option<u32>,
    min_ask: Option<f64>,
) -> Result<Value, CliError> {
    let m = market_from(&st.program, &st.mine, &st.book)?;
    let qty = qty.map_or(m.min_contracts, f64::from);
    let existing: Vec<&str> = ["buy", "sell"]
        .iter()
        .filter(|d| {
            st.mine
                .iter()
                .any(|o| o.get("direction").and_then(Value::as_str) == Some(**d))
        })
        .copied()
        .collect();
    if !existing.is_empty() {
        return Err(CliError::Usage(format!(
            "already quoting {} on this property — use `quote recenter` to move it, or `quote pull` to stand down first",
            existing.join(" and ")
        )));
    }
    let rails = Rails {
        min_ask,
        allow_out_of_band: false,
    };
    let mut notes = Vec::new();
    // Check BOTH legs before planning either: a rail failure on one side must
    // abort the whole quote rather than leave a paid-for, non-earning half.
    for (dir, price, cover) in [
        (
            "buy",
            bid,
            Cover {
                need: bid * qty,
                have: st.usdc,
            },
        ),
        (
            "sell",
            ask,
            Cover {
                need: qty,
                have: st.held,
            },
        ),
    ] {
        if let Some(n) = check_placement(&m, dir, price, qty, cover, &rails)? {
            notes.push(n);
        }
    }
    let steps: Vec<Value> = [("buy", bid), ("sell", ask)]
        .iter()
        .map(|(dir, price)| {
            json!({"op": "create", "direction": dir, "price": price, "quantity": qty,
                   "distFromMid": (price - m.mid).abs()})
        })
        .collect();
    Ok(json!({
        "propertyId": st.program.get("propertyId"),
        "mid": m.mid, "band": [m.mid - m.spread, m.mid + m.spread],
        "steps": steps, "untouched": [], "notes": notes,
    }))
}

/// Plan a stand-down. Pure (unit-tested).
///
/// Cancels resting orders, except any SELL at or above `keep_above` — a
/// deliberate hold (typically a cost-basis recovery ask parked above the band)
/// that standing down must never sweep away.
fn plan_pull(st: &State, keep_above: Option<f64>, side: Option<Side>) -> Result<Value, CliError> {
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let (mut steps, mut kept) = (Vec::new(), Vec::new());
    for o in &st.mine {
        let dir = o
            .get("direction")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (price, qty) = (num(o, "price"), num(o, "quantity"));
        let wrong_side = side.is_some_and(|s| s.as_str() != dir);
        let held_back = dir == "sell" && keep_above.is_some_and(|k| price >= k);
        if wrong_side || held_back {
            kept.push(json!({"direction": dir, "price": price, "quantity": qty,
                             "reason": if held_back { "at or above --keep-above" }
                                       else { "other side" }}));
            continue;
        }
        steps.push(
            json!({"op": "cancel", "direction": dir, "orderId": o.get("orderId"),
                          "price": price, "quantity": qty}),
        );
    }
    // A pull that would leave exactly one earning side is worth flagging: the
    // remainder rests, costs cover, and earns nothing.
    let mut notes = Vec::new();
    let remaining: Vec<&str> = ["buy", "sell"]
        .iter()
        .filter(|d| {
            kept.iter()
                .any(|k| k.get("direction").and_then(Value::as_str) == Some(**d))
        })
        .copied()
        .collect();
    if remaining.len() == 1 && !steps.is_empty() {
        notes.push(format!(
            "leaves a {}-only position — one-sided liquidity earns nothing, so the remainder rests for recovery, not rewards",
            remaining[0]
        ));
    }
    Ok(json!({
        "propertyId": st.program.get("propertyId"),
        "steps": steps, "untouched": kept, "notes": notes,
    }))
}

/// Live market facts every placement rail measures against. `mkt_best_bid`/
/// `mkt_best_ask` EXCLUDE our own resting size, so re-pricing past our own order
/// is never mistaken for crossing the market.
struct Market {
    mid: f64,
    spread: f64,
    min_contracts: f64,
    mkt_best_bid: f64,
    mkt_best_ask: f64,
}

/// What an order must be backed by: bids by wallet USDC, asks by held tokens.
struct Cover {
    need: f64,
    have: f64,
}

/// Caller-supplied guards.
struct Rails {
    min_ask: Option<f64>,
    allow_out_of_band: bool,
}

/// The shared safety rails for any order a `quote` primitive is about to place.
/// Pure (unit-tested), and the SINGLE place every primitive enforces them — so
/// they cannot drift apart as the family grows.
///
/// Returns an advisory note when an order is allowed but won't earn; returns an
/// error when placing it would be a mistake we refuse to make on the caller's
/// behalf. Ordered by what a violation costs:
///  1. crossing the market — fills instantly as a taker and pays the taker fee,
///     the exact opposite of resting liquidity;
///  2. exceeding cover — Lofty auto-cancels uncovered orders;
///  3. below `minContracts` / outside the reward band — rests but earns nothing;
///  4. under an explicit `--min-ask` floor — an accidental below-cost sell.
fn check_placement(
    m: &Market,
    dir: &str,
    price: f64,
    qty: f64,
    cover: Cover,
    r: &Rails,
) -> Result<Option<String>, CliError> {
    if dir == "buy" && m.mkt_best_ask.is_finite() && price >= m.mkt_best_ask {
        return Err(CliError::Usage(format!(
            "bid ${price:.2} would cross the best ask ${:.2} and fill immediately as a taker; this command only rests liquidity",
            m.mkt_best_ask
        )));
    }
    if dir == "sell" && m.mkt_best_bid.is_finite() && price <= m.mkt_best_bid {
        return Err(CliError::Usage(format!(
            "ask ${price:.2} would cross the best bid ${:.2} and fill immediately as a taker; this command only rests liquidity",
            m.mkt_best_bid
        )));
    }
    if cover.need > cover.have {
        return Err(CliError::Usage(if dir == "buy" {
            format!(
                "bid needs ${:.2} of cover but the wallet holds ${:.2} — Lofty auto-cancels uncovered orders",
                cover.need, cover.have
            )
        } else {
            format!(
                "ask needs {} token(s) but only {} are held — Lofty auto-cancels uncovered orders",
                cover.need, cover.have
            )
        }));
    }
    if qty < m.min_contracts {
        return Err(CliError::Usage(format!(
            "{dir} size {qty} is below minContracts {} — it would rest but earn nothing",
            m.min_contracts
        )));
    }
    if dir == "sell" {
        if let Some(floor) = r.min_ask {
            if price < floor {
                return Err(CliError::Usage(format!(
                    "ask ${price:.2} is below --min-ask ${floor:.2}"
                )));
            }
        }
    }
    let (lo, hi) = (m.mid - m.spread, m.mid + m.spread);
    if (price - m.mid).abs() > m.spread {
        if !r.allow_out_of_band {
            return Err(CliError::Usage(format!(
                "{dir} ${price:.2} is outside the reward band [${lo:.2}, ${hi:.2}] (mid ${:.2}) and would earn nothing — pass --allow-out-of-band to place it anyway",
                m.mid
            )));
        }
        return Ok(Some(format!(
            "{dir} ${price:.2} is outside the band [${lo:.2}, ${hi:.2}] — it will rest but earn nothing"
        )));
    }
    Ok(None)
}

/// One-line description of a planned step, for failure reporting.
fn describe_step(step: &Value) -> String {
    format!(
        "{} {} ${:.2} x{}",
        step.get("op").and_then(Value::as_str).unwrap_or("?"),
        step.get("direction").and_then(Value::as_str).unwrap_or("?"),
        step.get("price").and_then(Value::as_f64).unwrap_or(0.0),
        step.get("quantity").and_then(Value::as_f64).unwrap_or(0.0),
    )
}

/// Error text for a partway failure. Names what already landed and what didn't,
/// so the message alone is enough to see whether a side is now unquoted — it has
/// to stand on its own, because it is all the `--json` error DTO carries.
/// Pure (unit-tested).
fn partial_failure_message(applied: &[Value], failed: &Value, err: &str) -> String {
    let done: Vec<String> = applied
        .iter()
        .filter_map(|a| a.get("step"))
        .map(describe_step)
        .collect();
    format!(
        "recenter failed partway on `{}`: {err}. Already applied: {}. A side may now be unquoted and earning $0 — check `lofty rewards eligibility` and re-post it",
        describe_step(failed),
        if done.is_empty() {
            "nothing".to_string()
        } else {
            done.join("; ")
        },
    )
}

/// Human-readable diagnostic for the same failure (stderr, both output modes).
fn partial_failure_lines(applied: &[Value], failed: &Value) -> Vec<String> {
    let mut out =
        vec!["\u{26a0} recenter FAILED partway — a side may now be unquoted and earning $0".into()];
    for a in applied.iter().filter_map(|a| a.get("step")) {
        out.push(format!("    applied: {}", describe_step(a)));
    }
    out.push(format!("    FAILED:  {}", describe_step(failed)));
    out.push("    verify with `lofty rewards eligibility`, then re-post the missing side".into());
    out
}

struct Targets {
    bid: Option<f64>,
    ask: Option<f64>,
    bid_qty: Option<u32>,
    ask_qty: Option<u32>,
    min_ask: Option<f64>,
    allow_out_of_band: bool,
}

/// Build the cancel/create plan and enforce every safety rail. Pure (unit-tested).
///
/// Rails, in the order a mistake would cost you:
///  - never CROSS the market — a bid at/above the best ask (or an ask at/below the
///    best bid) is a taker order that fills instantly and pays the taker fee. Hard
///    refusal: this command exists to REST liquidity.
///  - never exceed COVER — bids against wallet USDC, asks against held tokens,
///    counting the sides being left in place. Uncovered orders are auto-cancelled.
///  - never go below `minContracts`, and stay inside the reward band, or the order
///    rests but earns nothing (band overridable with --allow-out-of-band).
///  - never place an ask below `--min-ask` when given.
///  - touch ONLY the sides given a price, so re-centering one side can never
///    orphan the other into a non-earning one-sided position.
fn plan_recenter(
    program: &Value,
    mine: &[Value],
    book: &Value,
    usdc: f64,
    held: f64,
    t: Targets,
) -> Result<Value, CliError> {
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let side = |dir: &str| -> Vec<&Value> {
        mine.iter()
            .filter(|o| o.get("direction").and_then(Value::as_str) == Some(dir))
            .collect()
    };
    let (my_bids, my_asks) = (side("buy"), side("sell"));

    // Single shared derivation — the same mid/best-bid/best-ask every other
    // primitive measures against, so the rails can't drift between commands.
    let market = market_from(program, mine, book)?;
    let mid = market.mid;

    let mut steps = Vec::new();
    let mut notes = Vec::new();

    // Capital/inventory still committed by the sides we are NOT touching.
    let untouched_bid_usd: f64 = if t.bid.is_some() {
        0.0
    } else {
        my_bids
            .iter()
            .map(|o| num(o, "price") * num(o, "quantity"))
            .sum()
    };
    let untouched_ask_qty: f64 = if t.ask.is_some() {
        0.0
    } else {
        my_asks.iter().map(|o| num(o, "quantity")).sum()
    };

    for (dir, target, qty_override, existing) in [
        ("buy", t.bid, t.bid_qty, &my_bids),
        ("sell", t.ask, t.ask_qty, &my_asks),
    ] {
        let Some(price) = target else { continue };
        // Default to the size already resting — a recenter moves price, not size.
        let existing_qty: f64 = existing.iter().map(|o| num(o, "quantity")).sum();
        let qty = qty_override.map_or(existing_qty, f64::from);
        if qty <= 0.0 {
            return Err(CliError::Usage(format!(
                "no resting {dir} to take a size from — pass --{}-qty",
                if dir == "buy" { "bid" } else { "ask" }
            )));
        }
        let cover = if dir == "buy" {
            Cover {
                need: price * qty + untouched_bid_usd,
                have: usdc,
            }
        } else {
            Cover {
                need: qty + untouched_ask_qty,
                have: held,
            }
        };
        if let Some(note) = check_placement(
            &market,
            dir,
            price,
            qty,
            cover,
            &Rails {
                min_ask: t.min_ask,
                allow_out_of_band: t.allow_out_of_band,
            },
        )? {
            notes.push(note);
        }
        let dist = (price - mid).abs();

        // Cancel only this side, then re-post it.
        for o in existing.iter() {
            steps.push(json!({
                "op": "cancel", "direction": dir,
                "orderId": o.get("orderId"),
                "price": num(o, "price"), "quantity": num(o, "quantity"),
            }));
        }
        steps.push(json!({
            "op": "create", "direction": dir, "price": price, "quantity": qty,
            "distFromMid": dist,
        }));
    }

    let untouched: Vec<Value> = [("buy", t.bid, &my_bids), ("sell", t.ask, &my_asks)]
        .iter()
        .filter(|(_, target, _)| target.is_none())
        .flat_map(|(dir, _, orders)| {
            orders.iter().map(move |o| {
                json!({"direction": dir, "price": num(o, "price"), "quantity": num(o, "quantity")})
            })
        })
        .collect();

    Ok(json!({
        "propertyId": program.get("propertyId"),
        "mid": mid, "band": [mid - market.spread, mid + market.spread],
        "steps": steps, "untouched": untouched, "notes": notes,
    }))
}

/// What a render is reporting. `NoOp` is distinct from `DryRun` on purpose:
/// with `--execute` and an empty plan the flag WAS honoured and there was simply
/// nothing to do — telling the operator to "re-run with --execute" would read as
/// though their flag had been ignored.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Render {
    DryRun,
    NoOp,
    Applied,
}

fn headline(mode: Render) -> &'static str {
    match mode {
        Render::DryRun => "DRY RUN — nothing sent",
        Render::NoOp => "nothing to do — no matching orders",
        Render::Applied => "applied",
    }
}

fn render_plan(v: &Value, mode: Render) {
    if mode == Render::Applied {
        for step in v
            .get("applied")
            .and_then(Value::as_array)
            .unwrap_or(&vec![])
        {
            let s = step.get("step").unwrap_or(&Value::Null);
            println!(
                "  done: {} {} ${:.2} x{}",
                s.get("op").and_then(Value::as_str).unwrap_or("?"),
                s.get("direction").and_then(Value::as_str).unwrap_or("?"),
                s.get("price").and_then(Value::as_f64).unwrap_or(0.0),
                s.get("quantity").and_then(Value::as_f64).unwrap_or(0.0),
            );
        }
        return;
    }
    // Only a plan that PLACES an order measures against a market; a pure
    // stand-down has no mid, and printing a defaulted $0.00 band would be a lie.
    match (
        v.get("mid").and_then(Value::as_f64),
        v.pointer("/band/0").and_then(Value::as_f64),
        v.pointer("/band/1").and_then(Value::as_f64),
    ) {
        (Some(mid), Some(lo), Some(hi)) => println!(
            "{}. mid ${mid:.2}, reward band [${lo:.2}, ${hi:.2}]",
            headline(mode)
        ),
        _ => println!("{}.", headline(mode)),
    }
    for step in v.get("steps").and_then(Value::as_array).unwrap_or(&vec![]) {
        let op = step.get("op").and_then(Value::as_str).unwrap_or("?");
        println!(
            "  {op:>6} {} ${:.2} x{}{}",
            step.get("direction").and_then(Value::as_str).unwrap_or("?"),
            step.get("price").and_then(Value::as_f64).unwrap_or(0.0),
            step.get("quantity").and_then(Value::as_f64).unwrap_or(0.0),
            step.get("distFromMid")
                .and_then(Value::as_f64)
                .map(|d| format!("  ({d:.2} from mid)"))
                .unwrap_or_default(),
        );
    }
    for u in v
        .get("untouched")
        .and_then(Value::as_array)
        .unwrap_or(&vec![])
    {
        println!(
            "  keep   {} ${:.2} x{} (untouched)",
            u.get("direction").and_then(Value::as_str).unwrap_or("?"),
            u.get("price").and_then(Value::as_f64).unwrap_or(0.0),
            u.get("quantity").and_then(Value::as_f64).unwrap_or(0.0),
        );
    }
    for n in v.get("notes").and_then(Value::as_array).unwrap_or(&vec![]) {
        eprintln!("  \u{26a0} {}", n.as_str().unwrap_or_default());
    }
    if mode == Render::DryRun {
        eprintln!("re-run with --execute to apply");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "01SAMPLEPROPERTY000000000A";

    fn program() -> Value {
        json!({"propertyId": P, "allowedSpread": 2.0, "minContracts": 4.0, "dailyRewards": 10.0})
    }
    /// Market book (excluding ours): bid $50 x10, ask $54 x10 → with our orders
    /// added the inside is $50/$54, mid $52, band [$50, $54].
    fn book(mine: &[Value]) -> Value {
        let mut bids = vec![json!({"price": 50.0, "quantity": 10.0})];
        let mut asks = vec![json!({"price": 54.0, "quantity": 10.0})];
        for o in mine {
            let lvl = json!({"price": o["price"], "quantity": o["quantity"]});
            if o["direction"] == "buy" {
                bids.push(lvl)
            } else {
                asks.push(lvl)
            }
        }
        json!({"orderbook": {"bids": bids, "asks": asks}})
    }
    fn order(id: &str, dir: &str, price: f64, qty: f64) -> Value {
        json!({"orderId": id, "propertyId": P, "direction": dir,
               "price": price, "quantity": qty, "status": "active"})
    }
    fn targets() -> Targets {
        Targets {
            bid: None,
            ask: None,
            bid_qty: None,
            ask_qty: None,
            min_ask: None,
            allow_out_of_band: false,
        }
    }
    fn ops(plan: &Value) -> Vec<String> {
        plan["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                format!(
                    "{} {}",
                    s["op"].as_str().unwrap(),
                    s["direction"].as_str().unwrap()
                )
            })
            .collect()
    }

    #[test]
    fn moving_the_bid_leaves_the_ask_completely_alone() {
        // The rail that matters most: re-centering one side must never cancel the
        // other, which would drop an earning position to one-sided $0.
        let mine = [
            order("B1", "buy", 51.0, 4.0),
            order("A1", "sell", 53.0, 4.0),
        ];
        let plan = plan_recenter(
            &program(),
            &mine,
            &book(&mine),
            1000.0,
            10.0,
            Targets {
                bid: Some(51.5),
                ..targets()
            },
        )
        .unwrap();
        assert_eq!(ops(&plan), ["cancel buy", "create buy"]);
        assert_eq!(plan["untouched"].as_array().unwrap().len(), 1);
        assert_eq!(plan["untouched"][0]["direction"], "sell");
    }

    #[test]
    fn a_bid_that_would_cross_the_ask_is_refused() {
        // Crossing fills instantly as a taker — the opposite of resting liquidity.
        let mine = [order("B1", "buy", 51.0, 4.0)];
        let e = plan_recenter(
            &program(),
            &mine,
            &book(&mine),
            1000.0,
            10.0,
            Targets {
                bid: Some(54.0),
                ..targets()
            },
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("cross"), "{e:?}");
    }

    #[test]
    fn an_ask_that_would_cross_the_bid_is_refused() {
        let mine = [order("A1", "sell", 53.0, 4.0)];
        let e = plan_recenter(
            &program(),
            &mine,
            &book(&mine),
            1000.0,
            10.0,
            Targets {
                ask: Some(50.0),
                ..targets()
            },
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("cross"), "{e:?}");
    }

    #[test]
    fn our_own_resting_size_is_not_mistaken_for_the_market() {
        // We are the best bid at $51; re-pricing to $51.50 must not be read as
        // crossing our OWN order. Only the market's $54 ask can be crossed.
        let mine = [order("B1", "buy", 51.0, 4.0)];
        let plan = plan_recenter(
            &program(),
            &mine,
            &book(&mine),
            1000.0,
            10.0,
            Targets {
                bid: Some(51.5),
                ..targets()
            },
        )
        .unwrap();
        assert_eq!(ops(&plan), ["cancel buy", "create buy"]);
    }

    #[test]
    fn an_uncovered_bid_is_refused_before_anything_is_cancelled() {
        let mine = [order("B1", "buy", 51.0, 4.0)];
        let e = plan_recenter(
            &program(),
            &mine,
            &book(&mine),
            100.0,
            10.0,
            Targets {
                bid: Some(51.5),
                ..targets()
            },
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("cover"), "{e:?}");
    }

    #[test]
    fn coverage_counts_the_side_being_left_in_place() {
        // Moving only the ask must still respect the untouched bid's $204 of cover.
        let mine = [
            order("B1", "buy", 51.0, 4.0),
            order("A1", "sell", 53.0, 4.0),
        ];
        let e = plan_recenter(
            &program(),
            &mine,
            &book(&mine),
            205.0,
            3.0,
            Targets {
                ask: Some(53.5),
                ..targets()
            },
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("token"), "{e:?}");
    }

    #[test]
    fn an_ask_beyond_held_tokens_is_refused() {
        let mine = [order("A1", "sell", 53.0, 4.0)];
        let e = plan_recenter(
            &program(),
            &mine,
            &book(&mine),
            1000.0,
            2.0,
            Targets {
                ask: Some(53.5),
                ..targets()
            },
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("held"), "{e:?}");
    }

    #[test]
    fn a_size_below_min_contracts_is_refused() {
        let mine = [order("B1", "buy", 51.0, 4.0)];
        let e = plan_recenter(
            &program(),
            &mine,
            &book(&mine),
            1000.0,
            10.0,
            Targets {
                bid: Some(51.5),
                bid_qty: Some(2),
                ..targets()
            },
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("minContracts"), "{e:?}");
    }

    #[test]
    fn an_out_of_band_target_is_refused_unless_explicitly_allowed() {
        let mine = [order("B1", "buy", 51.0, 4.0)];
        let e = plan_recenter(
            &program(),
            &mine,
            &book(&mine),
            1000.0,
            10.0,
            Targets {
                bid: Some(45.0),
                ..targets()
            },
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("band"), "{e:?}");
        // With the override it proceeds, but says it will earn nothing.
        let plan = plan_recenter(
            &program(),
            &mine,
            &book(&mine),
            1000.0,
            10.0,
            Targets {
                bid: Some(45.0),
                allow_out_of_band: true,
                ..targets()
            },
        )
        .unwrap();
        assert_eq!(ops(&plan), ["cancel buy", "create buy"]);
        assert!(plan["notes"][0].as_str().unwrap().contains("earn nothing"));
    }

    #[test]
    fn min_ask_blocks_a_below_cost_sell() {
        let mine = [order("A1", "sell", 53.0, 4.0)];
        let e = plan_recenter(
            &program(),
            &mine,
            &book(&mine),
            1000.0,
            10.0,
            Targets {
                ask: Some(52.5),
                min_ask: Some(53.0),
                ..targets()
            },
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("min-ask"), "{e:?}");
    }

    #[test]
    fn size_defaults_to_whatever_is_already_resting() {
        let mine = [order("B1", "buy", 51.0, 6.0)];
        let plan = plan_recenter(
            &program(),
            &mine,
            &book(&mine),
            1000.0,
            10.0,
            Targets {
                bid: Some(51.5),
                ..targets()
            },
        )
        .unwrap();
        let create = plan["steps"].as_array().unwrap().last().unwrap();
        assert_eq!(create["quantity"], 6.0);
    }

    #[test]
    fn both_sides_move_when_both_prices_are_given() {
        let mine = [
            order("B1", "buy", 51.0, 4.0),
            order("A1", "sell", 53.0, 4.0),
        ];
        let plan = plan_recenter(
            &program(),
            &mine,
            &book(&mine),
            1000.0,
            10.0,
            Targets {
                bid: Some(51.5),
                ask: Some(52.5),
                ..targets()
            },
        )
        .unwrap();
        assert_eq!(
            ops(&plan),
            ["cancel buy", "create buy", "cancel sell", "create sell"]
        );
        assert!(plan["untouched"].as_array().unwrap().is_empty());
    }

    #[test]
    fn cancel_always_precedes_the_matching_create() {
        // Posting first would double-commit capital and can breach per-property
        // coverage, which auto-cancels BOTH orders.
        let mine = [order("B1", "buy", 51.0, 4.0)];
        let plan = plan_recenter(
            &program(),
            &mine,
            &book(&mine),
            1000.0,
            10.0,
            Targets {
                bid: Some(51.5),
                ..targets()
            },
        )
        .unwrap();
        let steps = plan["steps"].as_array().unwrap();
        assert_eq!(steps[0]["op"], "cancel");
        assert_eq!(steps[1]["op"], "create");
    }

    fn state(mine: &[Value], usdc: f64, held: f64) -> State {
        State {
            program: program(),
            mine: mine.to_vec(),
            book: book(mine),
            usdc,
            held,
        }
    }

    // ── provision ────────────────────────────────────────────────────────────

    #[test]
    fn provision_plans_both_sides_at_once() {
        let st = state(&[], 1000.0, 10.0);
        let plan = plan_provision(&st, 51.0, 53.0, Some(4), None).unwrap();
        assert_eq!(ops(&plan), ["create buy", "create sell"]);
    }

    #[test]
    fn provision_size_defaults_to_min_contracts() {
        let st = state(&[], 1000.0, 10.0);
        let plan = plan_provision(&st, 51.0, 53.0, None, None).unwrap();
        assert_eq!(plan["steps"][0]["quantity"], 4.0);
    }

    #[test]
    fn provision_refuses_rather_than_leaving_a_paid_for_one_sided_half() {
        // Only 2 tokens against a 4-token ask: the ask can't be covered. The bid
        // alone would rest, consume cover, and earn NOTHING — so plan neither.
        let st = state(&[], 1000.0, 2.0);
        let e = plan_provision(&st, 51.0, 53.0, Some(4), None).unwrap_err();
        assert!(format!("{e:?}").contains("held"), "{e:?}");
    }

    #[test]
    fn provision_refuses_when_already_quoting() {
        let mine = [order("B1", "buy", 51.0, 4.0)];
        let st = state(&mine, 1000.0, 10.0);
        let e = plan_provision(&st, 51.0, 53.0, Some(4), None).unwrap_err();
        assert!(format!("{e:?}").contains("recenter"), "{e:?}");
    }

    #[test]
    fn provision_enforces_the_same_rails_as_recenter() {
        let st = state(&[], 1000.0, 10.0);
        // crossing
        assert!(plan_provision(&st, 54.0, 55.0, Some(4), None).is_err());
        // out of band
        assert!(plan_provision(&st, 45.0, 53.0, Some(4), None).is_err());
        // below minContracts
        assert!(plan_provision(&st, 51.0, 53.0, Some(1), None).is_err());
        // below the ask floor
        assert!(plan_provision(&st, 51.0, 52.5, Some(4), Some(53.0)).is_err());
    }

    // ── pull ─────────────────────────────────────────────────────────────────

    #[test]
    fn pull_cancels_everything_by_default() {
        let mine = [
            order("B1", "buy", 51.0, 4.0),
            order("A1", "sell", 53.0, 4.0),
        ];
        let st = state(&mine, 1000.0, 10.0);
        let plan = plan_pull(&st, None, None).unwrap();
        assert_eq!(ops(&plan), ["cancel buy", "cancel sell"]);
        assert!(plan["untouched"].as_array().unwrap().is_empty());
    }

    #[test]
    fn keep_above_protects_a_recovery_ask_from_a_stand_down() {
        // The whole point: standing down must not sweep away the cost-basis ask
        // parked above the band waiting to recover the position.
        let mine = [
            order("B1", "buy", 51.0, 4.0),
            order("A1", "sell", 72.80, 8.0),
        ];
        let st = state(&mine, 1000.0, 10.0);
        let plan = plan_pull(&st, Some(70.0), None).unwrap();
        assert_eq!(ops(&plan), ["cancel buy"]);
        let kept = &plan["untouched"][0];
        assert_eq!(kept["price"], 72.80);
        assert_eq!(kept["reason"], "at or above --keep-above");
    }

    #[test]
    fn keep_above_never_protects_a_bid() {
        // It guards recovery ASKS; a bid at the same price is still pulled.
        let mine = [order("B1", "buy", 71.0, 4.0)];
        let st = state(&mine, 1000.0, 10.0);
        let plan = plan_pull(&st, Some(70.0), None).unwrap();
        assert_eq!(ops(&plan), ["cancel buy"]);
    }

    #[test]
    fn pull_can_be_limited_to_one_side() {
        let mine = [
            order("B1", "buy", 51.0, 4.0),
            order("A1", "sell", 53.0, 4.0),
        ];
        let st = state(&mine, 1000.0, 10.0);
        let plan = plan_pull(&st, None, Some(Side::Buy)).unwrap();
        assert_eq!(ops(&plan), ["cancel buy"]);
        assert_eq!(plan["untouched"][0]["reason"], "other side");
    }

    #[test]
    fn pull_flags_when_it_leaves_a_non_earning_remainder() {
        let mine = [
            order("B1", "buy", 51.0, 4.0),
            order("A1", "sell", 72.80, 8.0),
        ];
        let st = state(&mine, 1000.0, 10.0);
        let plan = plan_pull(&st, Some(70.0), None).unwrap();
        assert!(
            plan["notes"][0].as_str().unwrap().contains("earns nothing"),
            "{plan}"
        );
    }

    #[test]
    fn a_pull_plan_carries_no_market_because_it_places_nothing() {
        // A stand-down measures against no mid or band. The renderer must omit
        // them rather than default to $0.00, which reads as a real (wrong) market.
        let mine = [order("B1", "buy", 51.0, 4.0)];
        let st = state(&mine, 1000.0, 10.0);
        let plan = plan_pull(&st, None, None).unwrap();
        assert!(plan.get("mid").is_none(), "pull should not invent a mid");
        assert!(plan.get("band").is_none(), "pull should not invent a band");
        // Plans that DO place an order still carry both.
        let placed = plan_provision(&state(&[], 1000.0, 10.0), 51.0, 53.0, Some(4), None).unwrap();
        assert!(placed["mid"].as_f64().is_some());
        assert!(placed["band"][0].as_f64().is_some());
    }

    #[test]
    fn pull_with_nothing_to_cancel_plans_nothing() {
        let st = state(&[], 1000.0, 10.0);
        let plan = plan_pull(&st, None, None).unwrap();
        assert!(plan["steps"].as_array().unwrap().is_empty());
    }

    #[test]
    fn partial_failure_message_names_what_landed_and_what_did_not() {
        // In --json mode this string IS the whole report (stdout carries only the
        // single error DTO), so it has to stand alone.
        let applied = [json!({"step": {"op": "cancel", "direction": "buy",
                                       "price": 62.25, "quantity": 3.0}})];
        let failed = json!({"op": "create", "direction": "buy",
                            "price": 61.50, "quantity": 3.0});
        let msg = partial_failure_message(&applied, &failed, "upstream 500");
        assert!(msg.contains("create buy $61.50 x3"), "{msg}");
        assert!(msg.contains("cancel buy $62.25 x3"), "{msg}");
        assert!(msg.contains("upstream 500"), "{msg}");
        assert!(msg.contains("earning $0"), "{msg}");
        assert!(msg.contains("rewards eligibility"), "{msg}");
    }

    #[test]
    fn partial_failure_before_anything_applied_says_nothing_landed() {
        let failed = json!({"op": "cancel", "direction": "buy",
                            "price": 62.25, "quantity": 3.0});
        let msg = partial_failure_message(&[], &failed, "boom");
        assert!(msg.contains("Already applied: nothing"), "{msg}");
    }

    #[test]
    fn partial_failure_diagnostic_lists_every_applied_step() {
        let applied = [json!({"step": {"op": "cancel", "direction": "buy",
                                       "price": 62.25, "quantity": 3.0}})];
        let failed = json!({"op": "create", "direction": "buy",
                            "price": 61.50, "quantity": 3.0});
        let lines = partial_failure_lines(&applied, &failed);
        assert!(lines[0].contains("FAILED partway"));
        assert!(lines
            .iter()
            .any(|l| l.contains("applied: cancel buy $62.25")));
        assert!(lines
            .iter()
            .any(|l| l.contains("FAILED:  create buy $61.50")));
    }

    #[test]
    fn a_one_sided_book_has_no_mid_to_measure_the_band_against() {
        let b = json!({"orderbook": {"bids": [{"price": 50.0, "quantity": 4.0}], "asks": []}});
        let mine = [order("B1", "buy", 50.0, 4.0)];
        let e = plan_recenter(
            &program(),
            &mine,
            &b,
            1000.0,
            10.0,
            Targets {
                bid: Some(50.5),
                ..targets()
            },
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("one-sided"), "{e:?}");
    }
}
