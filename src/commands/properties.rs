//! `lofty properties` — marketplace listings, order books, trade history.

use clap::Subcommand;
use pk_cli_core::{output, CliError};
use serde_json::Value;

use super::{emit, table_view, Ctx};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List marketplace properties.
    #[command(alias = "ls")]
    List {
        /// Page number (1-based).
        #[arg(long, default_value_t = 1)]
        page: u32,
        /// Items per page (max 200).
        #[arg(long, default_value_t = 50)]
        page_size: u32,
        /// RESIDENTIAL, COMMERCIAL, or ALL.
        #[arg(long, default_value = "ALL")]
        property_type: String,
        /// Location filter (default all).
        #[arg(long)]
        location: Option<String>,
    },
    /// Get one property by its ID.
    Get { property_id: String },
    /// Current order book (bids and asks) for a property.
    Orderbook { property_id: String },
    /// Recent trades and market summary for a property.
    Trades { property_id: String },
}

pub fn run(ctx: &Ctx, cmd: &Cmd) -> Result<(), CliError> {
    let client = ctx.client()?;
    match cmd {
        Cmd::List {
            page,
            page_size,
            property_type,
            location,
        } => {
            let mut q = vec![
                ("page", page.to_string()),
                ("pageSize", page_size.to_string()),
                ("propertyType", property_type.clone()),
            ];
            if let Some(loc) = location {
                q.push(("location", loc.clone()));
            }
            let payload = client.get("/public/v1/properties", &q)?;
            emit(ctx, "properties-list", payload, |v| {
                let props = v
                    .pointer("/result/properties")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let rows = table_view(
                    &props,
                    &[
                        "id",
                        "address_line1",
                        "city",
                        "state",
                        "sale_price",
                        "tokens",
                        "coc",
                        "cap_rate",
                    ],
                );
                output::table(&rows);
            });
            Ok(())
        }
        Cmd::Get { property_id } => {
            let payload = client.get(&format!("/public/v1/properties/{property_id}"), &[])?;
            emit(ctx, "property", payload, output::render);
            Ok(())
        }
        Cmd::Orderbook { property_id } => {
            let payload = client.get(
                &format!("/public/v1/properties/{property_id}/orderbook"),
                &[],
            )?;
            emit(ctx, "orderbook", payload, |v| {
                // SDK 0.2.3+: orderbook.{bids,asks}. Older: orderbook.orderBook.{buyOrders,sellOrders}.
                for (label, new_key, old_key) in [
                    ("BIDS", "bids", "buyOrders"),
                    ("ASKS", "asks", "sellOrders"),
                ] {
                    let orders = v
                        .pointer(&format!("/orderbook/{new_key}"))
                        .or_else(|| v.pointer(&format!("/orderbook/orderBook/{old_key}")))
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    eprintln!("{label} ({})", orders.len());
                    output::table(&table_view(&orders, &["price", "quantity", "expireAt"]));
                }
            });
            Ok(())
        }
        Cmd::Trades { property_id } => {
            let payload =
                client.get(&format!("/public/v1/properties/{property_id}/trades"), &[])?;
            emit(ctx, "property-trades", payload, |v| {
                if let (Some(bid), Some(ask)) = (v.get("bestBid"), v.get("bestAsk")) {
                    eprintln!("best bid {bid} | best ask {ask}");
                }
                let trades = v
                    .get("recentTrades")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                output::table(&table_view(
                    &trades,
                    &["price", "quantity", "direction", "createdAt"],
                ));
            });
            Ok(())
        }
    }
}
