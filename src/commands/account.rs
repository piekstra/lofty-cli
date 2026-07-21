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
    }
}
