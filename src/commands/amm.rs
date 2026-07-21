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
        /// Max USDC to pay (required on buys).
        #[arg(long)]
        max_usdc: Option<f64>,
        /// Min USDC to receive (required on sells).
        #[arg(long)]
        min_usdc: Option<f64>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        force: bool,
    },
}

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
            force,
        } => {
            validate_side(side)?;
            match side.as_str() {
                "buy" if max_usdc.is_none() => {
                    return Err(CliError::Usage(
                        "--max-usdc is required on buys (slippage bound)".into(),
                    ))
                }
                "sell" if min_usdc.is_none() => {
                    return Err(CliError::Usage(
                        "--min-usdc is required on sells (slippage bound)".into(),
                    ))
                }
                _ => {}
            }
            let bound = match side.as_str() {
                "buy" => format!("max ${:.2}", max_usdc.unwrap()),
                _ => format!("min ${:.2}", min_usdc.unwrap()),
            };
            confirm(
                ctx,
                *force,
                &format!("swap ({side}) {tokens} token(s) on pool {pool_id} ({bound})"),
            )?;
            let mut body = json!({
                "poolId": pool_id,
                "side": side,
                "tokenAmount": tokens,
            });
            if let Some(m) = max_usdc {
                body["maxUsdcAmount"] = json!(m);
            }
            if let Some(m) = min_usdc {
                body["minUsdcAmount"] = json!(m);
            }
            let client = ctx.client()?;
            let payload = client.post("/public/v1/amm/swap", &body)?;
            emit(ctx, "amm-swap", payload, output::render);
            Ok(())
        }
    }
}

fn validate_side(side: &str) -> Result<(), CliError> {
    if matches!(side, "buy" | "sell") {
        Ok(())
    } else {
        Err(CliError::Usage("--side must be `buy` or `sell`".into()))
    }
}
