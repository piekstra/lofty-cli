//! `lofty amm` — AMM pools, on-chain price quotes, and swaps.
//!
//! `swap` executes a real trade and is gated like every mutation: confirm or
//! `--force` (exit 6 non-interactive). Slippage bounds are REQUIRED — the SDK
//! contract wants `max_usdc` on buys and `min_usdc` on sells.

use clap::Subcommand;
use pk_cli_core::{output, CliError};
use serde_json::{json, Value};

use super::{confirm, emit, table_view, Ctx};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List active AMM pools.
    #[command(alias = "ls")]
    Pools {
        /// Filter to one property.
        #[arg(long)]
        property_id: Option<String>,
    },
    /// Get one pool by numeric ID.
    Pool { pool_id: u64 },
    /// Exact on-chain quote. Pass --tokens or --usdc (not both).
    Quote {
        #[arg(long)]
        pool_id: u64,
        /// buy or sell.
        #[arg(long)]
        side: String,
        /// Quote for this many property tokens.
        #[arg(long)]
        tokens: Option<f64>,
        /// Quote for this much USDC.
        #[arg(long)]
        usdc: Option<f64>,
    },
    /// Execute an instant swap against a pool (real trade!).
    Swap {
        #[arg(long)]
        pool_id: u64,
        /// buy or sell.
        #[arg(long)]
        side: String,
        /// Token amount to swap.
        #[arg(long)]
        tokens: f64,
        /// Max USDC to pay (required on buys unless --max-slippage-pct is given).
        #[arg(long)]
        max_usdc: Option<f64>,
        /// Min USDC to receive (required on sells unless --max-slippage-pct is given).
        #[arg(long)]
        min_usdc: Option<f64>,
        /// Derive the bound from a FRESH quote, allowing this much slippage.
        /// Safer than hand-picking a number: the bound is computed, not rounded.
        #[arg(long, value_name = "PCT")]
        max_slippage_pct: Option<f64>,
        /// Proceed even when the bound implies more slippage than ABSURD_SLIPPAGE_PCT.
        #[arg(long)]
        allow_high_slippage: bool,
        /// Skip the confirmation prompt.
        #[arg(long)]
        force: bool,
    },
}

/// Refuse a bound further than this from the live quote unless said explicitly.
///
/// Pinned to the platform's own buy-fee load (platform 3% + pool LP 2% = 5%), not
/// picked by taste: a slippage tolerance wider than the ENTIRE fee schedule cannot
/// plausibly be deliberate, so it is far more likely a typo or a rounded-up guess.
/// This is a "you did not mean this" backstop — express a real tolerance with
/// --max-slippage-pct, which derives the bound exactly.
///
/// It matters most under --force, where no prompt is shown and a loose bound would
/// otherwise pass unseen (exactly how a 9.2% bound got submitted by hand).
const ABSURD_SLIPPAGE_PCT: f64 = 5.0;

pub fn run(ctx: &Ctx, cmd: &Cmd) -> Result<(), CliError> {
    match cmd {
        Cmd::Pools { property_id } => {
            let mut q: Vec<(&str, String)> = Vec::new();
            if let Some(id) = property_id {
                q.push(("propertyId", id.clone()));
            }
            let client = ctx.client()?;
            let payload = client.get("/public/v1/amm/pools", &q)?;
            emit(ctx, "amm-pools", payload, |v| {
                let pools = v
                    .get("pools")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                output::table(&table_view(
                    &pools,
                    &["poolId", "propertyId", "priceLow", "priceHigh"],
                ));
            });
            Ok(())
        }
        Cmd::Pool { pool_id } => {
            let client = ctx.client()?;
            let payload = client.get(&format!("/public/v1/amm/pools/{pool_id}"), &[])?;
            emit(ctx, "amm-pool", payload, output::render);
            Ok(())
        }
        Cmd::Quote {
            pool_id,
            side,
            tokens,
            usdc,
        } => {
            validate_side(side)?;
            if tokens.is_some() == usdc.is_some() {
                return Err(CliError::Usage(
                    "pass exactly one of --tokens or --usdc".into(),
                ));
            }
            let mut q = vec![("poolId", pool_id.to_string()), ("side", side.clone())];
            if let Some(t) = tokens {
                q.push(("tokenAmount", t.to_string()));
            }
            if let Some(u) = usdc {
                q.push(("usdcAmount", u.to_string()));
            }
            let client = ctx.client()?;
            let payload = client.get("/public/v1/amm/quote", &q)?;
            emit(ctx, "amm-quote", payload, output::render);
            Ok(())
        }
        Cmd::Swap {
            pool_id,
            side,
            tokens,
            max_usdc,
            min_usdc,
            max_slippage_pct,
            allow_high_slippage,
            force,
        } => {
            validate_side(side)?;
            if max_usdc.is_some() && min_usdc.is_some() {
                return Err(CliError::Usage(
                    "pass only the bound for your side: --max-usdc on buys, --min-usdc on sells"
                        .into(),
                ));
            }
            if max_slippage_pct.is_some_and(|p| !p.is_finite() || p < 0.0) {
                return Err(CliError::Usage(
                    "--max-slippage-pct must be a non-negative percent".into(),
                ));
            }
            let buying = side == "buy";
            if max_slippage_pct.is_none() {
                match (buying, max_usdc, min_usdc) {
                    (true, None, _) => return Err(CliError::Usage(
                        "buys need a slippage bound: --max-usdc <USD> or --max-slippage-pct <PCT>"
                            .into(),
                    )),
                    (false, _, None) => return Err(CliError::Usage(
                        "sells need a slippage bound: --min-usdc <USD> or --max-slippage-pct <PCT>"
                            .into(),
                    )),
                    _ => {}
                }
            }

            let client = ctx.client()?;
            // Always price a FRESH quote before trading. Two reasons: it derives the
            // bound for --max-slippage-pct, and it lets an explicit bound be checked
            // against reality instead of accepted on faith — a bound passed straight
            // through unexamined is how a hand-rounded number silently authorises
            // far more slippage than intended.
            let quote = client.get(
                "/public/v1/amm/quote",
                &[
                    ("poolId", pool_id.to_string()),
                    ("side", side.clone()),
                    ("tokenAmount", tokens.to_string()),
                ],
            )?;
            // Buys are bounded on the swap amount (fees land on top of it); sells are
            // bounded on proceeds. Compare like with like.
            let reference = quote
                .get("usdcAmount")
                .and_then(Value::as_f64)
                .ok_or_else(|| CliError::Other("quote returned no usdcAmount".into()))?;
            let total_debit = quote.get("totalDebit").and_then(Value::as_f64);
            let fees = quote.pointer("/fees/total").and_then(Value::as_f64);

            let bound = match (max_slippage_pct, buying) {
                (Some(pct), true) => reference * (1.0 + pct / 100.0),
                (Some(pct), false) => reference * (1.0 - pct / 100.0),
                (None, true) => max_usdc.unwrap(),
                (None, false) => min_usdc.unwrap(),
            };
            let implied = implied_slippage_pct(bound, reference, buying);
            if implied > ABSURD_SLIPPAGE_PCT && !allow_high_slippage {
                return Err(CliError::Usage(format!(
                    "bound ${bound:.2} is {implied:.1}% from the live quote ${reference:.2} — that authorises far more slippage than you likely intend. Use --max-slippage-pct for an exact bound, or --allow-high-slippage to proceed."
                )));
            }

            let detail = match (buying, total_debit, fees) {
                (true, Some(td), Some(f)) => format!(
                    "quote ${reference:.2} + ${f:.2} fees = ${td:.2} expected; bound ${bound:.2} ({implied:.1}% slippage allowed)"
                ),
                (true, _, _) => format!(
                    "quote ${reference:.2}; bound ${bound:.2} ({implied:.1}% slippage allowed)"
                ),
                (false, _, _) => format!(
                    "quote ${reference:.2} proceeds; floor ${bound:.2} ({implied:.1}% slippage allowed)"
                ),
            };
            confirm(
                ctx,
                *force,
                &format!("swap ({side}) {tokens} token(s) on pool {pool_id} — {detail}"),
            )?;
            let mut body = json!({
                "poolId": pool_id,
                "side": side,
                "tokenAmount": tokens,
            });
            if buying {
                body["maxUsdcAmount"] = json!(round2(bound));
            } else {
                body["minUsdcAmount"] = json!(round2(bound));
            }
            let client = ctx.client()?;
            let payload = client.post("/public/v1/amm/swap", &body)?;
            emit(ctx, "amm-swap", payload, output::render);
            Ok(())
        }
    }
}

/// How much worse than the live quote a bound permits, in percent. Buys are
/// bounded above (paying more), sells below (receiving less), so the sign flips.
/// A bound better than the quote is 0, not negative slippage. Pure (unit-tested).
fn implied_slippage_pct(bound: f64, reference: f64, buying: bool) -> f64 {
    if reference <= 0.0 {
        return 0.0;
    }
    let pct = if buying {
        (bound / reference - 1.0) * 100.0
    } else {
        (1.0 - bound / reference) * 100.0
    };
    pct.max(0.0)
}

fn round2(n: f64) -> f64 {
    (n * 100.0).round() / 100.0
}

fn validate_side(side: &str) -> Result<(), CliError> {
    if matches!(side, "buy" | "sell") {
        Ok(())
    } else {
        Err(CliError::Usage("--side must be `buy` or `sell`".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slippage_is_measured_against_the_live_quote() {
        // The bound that actually got submitted by hand: $56 on a ~$51.22 quote.
        // It reads as 9.3% — comfortably over the 5% backstop, so it is refused.
        let pct = implied_slippage_pct(56.00, 51.22, true);
        assert!((pct - 9.33).abs() < 0.05, "{pct}");
        assert!(pct > ABSURD_SLIPPAGE_PCT);
        // A 1% bound is well inside it.
        assert!(implied_slippage_pct(51.73, 51.22, true) < 1.05);
    }

    #[test]
    fn sells_are_bounded_the_other_way() {
        // On a sell the bound is a FLOOR on proceeds, so slippage is the shortfall.
        assert!((implied_slippage_pct(49.00, 50.00, false) - 2.0).abs() < 1e-9);
        // Buying and selling at the same numbers are not the same tolerance.
        assert!(implied_slippage_pct(49.00, 50.00, true) < 1e-9);
    }

    #[test]
    fn a_bound_better_than_the_quote_is_zero_not_negative() {
        // Never report "-3% slippage" and never let a favourable bound look absurd.
        assert_eq!(implied_slippage_pct(48.00, 50.00, true), 0.0);
        assert_eq!(implied_slippage_pct(52.00, 50.00, false), 0.0);
    }

    #[test]
    fn a_zero_reference_cannot_divide() {
        assert_eq!(implied_slippage_pct(10.0, 0.0, true), 0.0);
    }
}
