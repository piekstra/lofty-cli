//! `lofty orders` — list, inspect, place, and cancel limit orders.
//!
//! Placing and cancelling are mutations: they prompt for confirmation, or
//! require `--force` when non-interactive (exit 6 otherwise). Both send an
//! `Idempotency-Key` automatically.

use clap::Subcommand;
use pk_cli_core::{output, CliError};
use serde_json::{json, Value};

use super::{confirm, emit, table_view, Ctx};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List your orders (all properties by default).
    #[command(alias = "ls")]
    List {
        /// Filter to one property.
        #[arg(long)]
        property_id: Option<String>,
        /// active | pending | executing | executed | cancelled | expired | intent.
        #[arg(long)]
        status: Option<String>,
    },
    /// Get one order by ID.
    Get { order_id: String },
    /// Place a limit order (requires a trading-enabled key).
    Create {
        #[arg(long)]
        property_id: String,
        /// buy or sell.
        #[arg(long)]
        direction: String,
        /// Price per token in USD (min $0.01).
        #[arg(long)]
        price: f64,
        /// Number of tokens (min 1).
        #[arg(long)]
        quantity: u32,
        /// Expiry as Unix ms (min 29 days out; default 30 days).
        #[arg(long)]
        expire_at: Option<u64>,
        /// Skip the confirmation prompt.
        #[arg(long)]
        force: bool,
    },
    /// Cancel an active order.
    Cancel {
        order_id: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        force: bool,
    },
}

pub fn run(ctx: &Ctx, cmd: &Cmd) -> Result<(), CliError> {
    match cmd {
        Cmd::List {
            property_id,
            status,
        } => {
            let client = ctx.client()?;
            let mut q: Vec<(&str, String)> = Vec::new();
            match property_id {
                Some(id) => q.push(("propertyId", id.clone())),
                None => q.push(("all", "true".into())),
            }
            if let Some(s) = status {
                q.push(("status", s.clone()));
            }
            let payload = client.get("/public/v1/orders", &q)?;
            emit(ctx, "orders-list", payload, |v| {
                let orders = v
                    .get("orders")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                output::table(&table_view(
                    &orders,
                    &[
                        "orderId",
                        "propertyId",
                        "direction",
                        "price",
                        "quantity",
                        "status",
                        "expireAt",
                    ],
                ));
            });
            Ok(())
        }
        Cmd::Get { order_id } => {
            let client = ctx.client()?;
            let payload = client.get(&format!("/public/v1/orders/{order_id}"), &[])?;
            emit(ctx, "order", payload, output::render);
            Ok(())
        }
        Cmd::Create {
            property_id,
            direction,
            price,
            quantity,
            expire_at,
            force,
        } => {
            if !matches!(direction.as_str(), "buy" | "sell") {
                return Err(CliError::Usage(
                    "--direction must be `buy` or `sell`".into(),
                ));
            }
            if *price < 0.01 {
                return Err(CliError::Usage("--price must be at least $0.01".into()));
            }
            if *quantity < 1 {
                return Err(CliError::Usage("--quantity must be at least 1".into()));
            }
            confirm(
                ctx,
                *force,
                &format!(
                    "place limit {direction} of {quantity} token(s) @ ${price:.2} on {property_id} (total ${:.2})",
                    *price * f64::from(*quantity)
                ),
            )?;
            let mut body = json!({
                "propertyId": property_id,
                "direction": direction,
                "price": price,
                "quantity": quantity,
            });
            if let Some(exp) = expire_at {
                body["expireAt"] = json!(exp);
            }
            let client = ctx.client()?;
            let payload = client.post("/public/v1/orders", &body)?;
            emit(ctx, "order-created", payload, output::render);
            Ok(())
        }
        Cmd::Cancel { order_id, force } => {
            confirm(ctx, *force, &format!("cancel order {order_id}"))?;
            let client = ctx.client()?;
            let payload = client.delete(&format!("/public/v1/orders/{order_id}"))?;
            emit(ctx, "order-cancelled", payload, output::render);
            Ok(())
        }
    }
}
