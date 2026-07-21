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
    }
}
