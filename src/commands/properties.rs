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
        /// Filter by property type. Values are upstream-defined and passed
        /// through verbatim (observed: "single family", "vacation rental",
        /// "duplex", "triplex", "fourplex", "mixed use", "commercial").
        /// Omit to list every type.
        #[arg(long)]
        property_type: Option<String>,
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

/// Query for the marketplace listing. `propertyType` is omitted entirely when
/// no filter is given: the upstream enum changed out from under the old `ALL`
/// sentinel (every documented value now 400s with `invalid_property_type`),
/// and omission already means "all types". A user-supplied value is passed
/// through verbatim so new upstream values work without a CLI release.
fn list_query(
    page: u32,
    page_size: u32,
    property_type: Option<&str>,
    location: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut q = vec![
        ("page", page.to_string()),
        ("pageSize", page_size.to_string()),
    ];
    if let Some(pt) = property_type {
        q.push(("propertyType", pt.to_string()));
    }
    if let Some(loc) = location {
        q.push(("location", loc.to_string()));
    }
    q
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
            let q = list_query(
                *page,
                *page_size,
                property_type.as_deref(),
                location.as_deref(),
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omits_property_type_when_no_filter_is_given() {
        // Sending any sentinel (the old default was `ALL`) now 400s upstream;
        // "all types" must be expressed by leaving the parameter off.
        let q = list_query(1, 50, None, None);
        assert_eq!(
            q,
            vec![("page", "1".to_string()), ("pageSize", "50".to_string())]
        );
    }

    #[test]
    fn passes_a_property_type_through_verbatim() {
        // Upstream values contain spaces and are lowercase ("vacation rental");
        // no client-side casing, mapping, or validation — unknown-to-us values
        // must reach the API untouched so new upstream types just work.
        let q = list_query(2, 10, Some("vacation rental"), Some("Tiffin, OH"));
        assert_eq!(
            q,
            vec![
                ("page", "2".to_string()),
                ("pageSize", "10".to_string()),
                ("propertyType", "vacation rental".to_string()),
                ("location", "Tiffin, OH".to_string()),
            ]
        );
    }
}
