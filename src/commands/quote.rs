//! `lofty quote` — safe primitives for moving a two-sided market-making quote.
//!
//! These are MECHANISM, not strategy: you supply the target prices, and the
//! command performs the move safely. It decides nothing about *where* to quote.
//!
//! Every subcommand is a DRY RUN by default — it prints the plan and changes
//! nothing until `--execute`. The dangerous thing requires a flag; the safe
//! thing happens when you forget one.

use clap::Subcommand;
use pk_cli_core::{output, CliError};
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
}

pub fn run(ctx: &Ctx, cmd: &Cmd) -> Result<(), CliError> {
    let client = ctx.client()?;
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
                emit(ctx, "quote-plan", plan, |v| render_plan(v, false));
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
                        // Report what already happened before failing — the operator
                        // needs to know whether a side is currently unquoted.
                        let partial = json!({"applied": done, "failedStep": step,
                                             "error": e.to_string()});
                        eprintln!("\u{26a0} recenter FAILED partway — a side may now be unquoted and earning $0:");
                        output::render(&partial);
                        return Err(e);
                    }
                }
            }
            emit(ctx, "quote-recentered", json!({"applied": done}), |v| {
                render_plan(v, true)
            });
            Ok(())
        }
    }
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
    let spread = num(program, "allowedSpread");
    let min_contracts = num(program, "minContracts");

    let side = |dir: &str| -> Vec<&Value> {
        mine.iter()
            .filter(|o| o.get("direction").and_then(Value::as_str) == Some(dir))
            .collect()
    };
    let (my_bids, my_asks) = (side("buy"), side("sell"));

    // Market book excluding our own resting size, so we never measure against
    // ourselves when checking for a cross.
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
    let mut bid_levels = levels("bids", "buyOrders");
    let mut ask_levels = levels("asks", "sellOrders");
    let subtract = |lv: &mut Vec<(f64, f64)>, ours: &[&Value]| {
        for o in ours {
            let (p, q) = (num(o, "price"), num(o, "quantity"));
            if let Some(l) = lv.iter_mut().find(|l| (l.0 - p).abs() < 0.005) {
                l.1 -= q;
            }
        }
        lv.retain(|l| l.1 > 0.0);
    };
    subtract(&mut bid_levels, &my_bids);
    subtract(&mut ask_levels, &my_asks);

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
    let mid = (full_bid + full_ask) / 2.0;
    let mkt_best_bid = bid_levels
        .iter()
        .map(|l| l.0)
        .fold(f64::NEG_INFINITY, f64::max);
    let mkt_best_ask = ask_levels.iter().map(|l| l.0).fold(f64::INFINITY, f64::min);

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
        if qty < min_contracts {
            return Err(CliError::Usage(format!(
                "{dir} size {qty} is below minContracts {min_contracts} — it would rest but earn nothing"
            )));
        }
        // Crossing check against the market with our own size removed.
        if dir == "buy" && mkt_best_ask.is_finite() && price >= mkt_best_ask {
            return Err(CliError::Usage(format!(
                "bid ${price:.2} would cross the best ask ${mkt_best_ask:.2} and fill immediately as a taker; this command only rests liquidity"
            )));
        }
        if dir == "sell" && mkt_best_bid.is_finite() && price <= mkt_best_bid {
            return Err(CliError::Usage(format!(
                "ask ${price:.2} would cross the best bid ${mkt_best_bid:.2} and fill immediately as a taker; this command only rests liquidity"
            )));
        }
        if dir == "sell" {
            if let Some(floor) = t.min_ask {
                if price < floor {
                    return Err(CliError::Usage(format!(
                        "ask ${price:.2} is below --min-ask ${floor:.2}"
                    )));
                }
            }
        }
        // Band check.
        let dist = (price - mid).abs();
        if dist > spread {
            if !t.allow_out_of_band {
                return Err(CliError::Usage(format!(
                    "{dir} ${price:.2} is outside the reward band [${:.2}, ${:.2}] (mid ${mid:.2}) and would earn nothing — pass --allow-out-of-band to place it anyway",
                    mid - spread,
                    mid + spread
                )));
            }
            notes.push(format!(
                "{dir} ${price:.2} is outside the band [${:.2}, ${:.2}] — it will rest but earn nothing",
                mid - spread,
                mid + spread
            ));
        }
        // Coverage, counting whatever we are leaving in place on that side.
        if dir == "buy" {
            let need = price * qty + untouched_bid_usd;
            if need > usdc {
                return Err(CliError::Usage(format!(
                    "bid needs ${need:.2} of cover but the wallet holds ${usdc:.2} — Lofty auto-cancels uncovered orders"
                )));
            }
        } else {
            let need = qty + untouched_ask_qty;
            if need > held {
                return Err(CliError::Usage(format!(
                    "ask needs {need} token(s) but only {held} are held — Lofty auto-cancels uncovered orders"
                )));
            }
        }

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
        "mid": mid, "band": [mid - spread, mid + spread],
        "steps": steps, "untouched": untouched, "notes": notes,
    }))
}

/// Render a plan (dry run) or the applied result.
fn render_plan(v: &Value, applied: bool) {
    if applied {
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
    let g = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    println!(
        "DRY RUN — nothing sent. mid ${:.2}, reward band [${:.2}, ${:.2}]",
        g("mid"),
        v.pointer("/band/0").and_then(Value::as_f64).unwrap_or(0.0),
        v.pointer("/band/1").and_then(Value::as_f64).unwrap_or(0.0),
    );
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
    eprintln!("re-run with --execute to apply");
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
