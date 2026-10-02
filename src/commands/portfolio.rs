//! `lofty account portfolio` — how your holdings are weighted by location,
//! property, and rent.
//!
//! Pure report logic lives here so it is unit-testable without a client; the
//! command in `account.rs` only gathers the inputs (`/account/positions` plus
//! one `/properties/{id}` per held property).

use std::collections::BTreeMap;

use pk_cli_core::output;
use serde_json::{json, Value};

/// Why a property does or does not pay rent right now.
///
/// The SDK exposes no rent-payment feed, so this reads the listing flags Lofty
/// itself publishes: a vacant property pays nothing, a delinquent tenant pays
/// nothing, and a property whose projected cash flow is zero or negative has
/// nothing to distribute. `monthly_rent` is deliberately NOT consulted — it is
/// observed as `0` on occupied, distributing properties.
fn rent_status(listing: Option<&Value>) -> &'static str {
    let Some(p) = listing else {
        return "unknown";
    };
    if p.get("is_occupied").and_then(Value::as_bool) == Some(false) {
        "vacant"
    } else if p.get("is_delinquent").and_then(Value::as_bool) == Some(true) {
        "delinquent"
    } else if p
        .get("projected_annual_cash_flow")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        <= 0.0
    {
        "no-cash-flow"
    } else {
        "renting"
    }
}

/// Percent of `part` in `whole`, or `None` when there is no whole to divide.
fn share(part: f64, whole: f64) -> Option<f64> {
    (whole > 0.0).then(|| part / whole * 100.0)
}

/// Build the composition report. Pure (unit-tested).
///
/// `positions` is the `/account/positions` array; `listings` maps a property id
/// to its `/properties/{id}` record (the inner `property` object). A held
/// property with no listing is still weighted by value, under an `unknown`
/// location, and counted as non-renting — dropping it would silently inflate
/// every other share.
///
/// Weights by value use each position's `currentValue` (tokens x current
/// price). Your daily rent from a property is its projected annual cash flow,
/// spread over its issued tokens, times the tokens you hold, over 365 days.
pub fn portfolio(positions: &[Value], listings: &BTreeMap<String, Value>) -> Value {
    let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let text = |v: Option<&Value>, k: &str| {
        v.and_then(|p| p.get(k))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    };

    struct Row {
        id: String,
        address: Option<String>,
        city: String,
        state: String,
        tokens: f64,
        value: f64,
        status: &'static str,
        daily_rent: f64,
    }

    let mut rows: Vec<Row> = Vec::new();
    for p in positions {
        let tokens = num(p, "currentTokens");
        if tokens <= 0.0 {
            continue; // fully sold — no longer part of the portfolio
        }
        let id = p
            .get("propertyId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let listing = listings.get(&id);
        let value = match p.get("currentValue").and_then(Value::as_f64) {
            Some(v) => v,
            None => tokens * num(p, "currentPrice"),
        };
        let status = rent_status(listing);
        let daily_rent = match listing {
            Some(l) if status == "renting" && num(l, "tokens") > 0.0 => {
                num(l, "projected_annual_cash_flow") / 365.0 * tokens / num(l, "tokens")
            }
            _ => 0.0,
        };
        rows.push(Row {
            address: text(listing, "address_line1"),
            city: text(listing, "city").unwrap_or_else(|| "unknown".into()),
            state: text(listing, "state").unwrap_or_else(|| "unknown".into()),
            id,
            tokens,
            value,
            status,
            daily_rent,
        });
    }

    let total_value: f64 = rows.iter().map(|r| r.value).sum();
    let total_rent: f64 = rows.iter().map(|r| r.daily_rent).sum();
    let renting: Vec<&Row> = rows.iter().filter(|r| r.status == "renting").collect();
    let renting_value: f64 = renting.iter().map(|r| r.value).sum();

    // Group by a key, summing value, then order largest first.
    let group = |key: &dyn Fn(&Row) -> (String, Option<String>)| -> Vec<Value> {
        let mut g: BTreeMap<(String, Option<String>), (usize, f64, f64)> = BTreeMap::new();
        for r in &rows {
            let e = g.entry(key(r)).or_insert((0, 0.0, 0.0));
            e.0 += 1;
            e.1 += r.value;
            e.2 += r.daily_rent;
        }
        let mut out: Vec<Value> = g
            .into_iter()
            .map(|((name, state), (n, value, rent))| {
                let mut o = json!({
                    "properties": n,
                    "valueUsd": value,
                    "valueSharePct": share(value, total_value),
                    "dailyRentUsd": rent,
                    "rentSharePct": share(rent, total_rent),
                });
                match state {
                    Some(st) => {
                        o["city"] = json!(name);
                        o["state"] = json!(st);
                    }
                    None => o["state"] = json!(name),
                }
                o
            })
            .collect();
        out.sort_by(|a, b| num(b, "valueUsd").total_cmp(&num(a, "valueUsd")));
        out
    };
    let by_state = group(&|r| (r.state.clone(), None));
    let by_city = group(&|r| (r.city.clone(), Some(r.state.clone())));

    let mut properties: Vec<Value> = rows
        .iter()
        .map(|r| {
            let renting = r.status == "renting";
            json!({
                "propertyId": r.id,
                "address": r.address,
                "city": r.city,
                "state": r.state,
                "tokens": r.tokens,
                "valueUsd": r.value,
                "valueSharePct": share(r.value, total_value),
                "rentStatus": r.status,
                "renting": renting,
                "dailyRentUsd": r.daily_rent,
                // Non-paying properties are excluded from the rent weighting
                // rather than shown as 0%, so the renting ones sum to 100.
                "rentSharePct": if renting { share(r.daily_rent, total_rent) } else { None },
            })
        })
        .collect();
    properties.sort_by(|a, b| num(b, "valueUsd").total_cmp(&num(a, "valueUsd")));

    json!({
        "totals": {
            "properties": rows.len(),
            "valueUsd": total_value,
            "dailyRentUsd": total_rent,
            "rentingProperties": renting.len(),
            "nonRentingProperties": rows.len() - renting.len(),
            "rentingPropertySharePct": share(renting.len() as f64, rows.len() as f64),
            "rentingValueUsd": renting_value,
            "rentingValueSharePct": share(renting_value, total_value),
        },
        "byState": by_state,
        "byCity": by_city,
        "properties": properties,
    })
}

fn pct(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_f64)
        .map(|p| format!("{p:.1}%"))
        .unwrap_or_else(|| "-".into())
}

fn usd(v: &Value, k: &str, places: usize) -> String {
    format!(
        "${:.*}",
        places,
        v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
    )
}

/// Human view: headline, then one table per weighting.
pub fn render(v: &Value) {
    let t = &v["totals"];
    let count = |k: &str| t.get(k).and_then(Value::as_u64).unwrap_or(0);
    println!(
        "{} properties, {} estimated value, {}/day projected rent",
        count("properties"),
        usd(t, "valueUsd", 2),
        usd(t, "dailyRentUsd", 4),
    );
    println!(
        "renting: {} of {} properties ({}), {} of holdings value",
        count("rentingProperties"),
        count("properties"),
        pct(t, "rentingPropertySharePct"),
        pct(t, "rentingValueSharePct"),
    );

    let arr = |k: &str| {
        v.get(k)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };

    println!("\nBY STATE");
    output::table(
        &arr("byState")
            .iter()
            .map(|g| {
                json!({
                    "state": g["state"], "properties": g["properties"],
                    "value": usd(g, "valueUsd", 2), "value %": pct(g, "valueSharePct"),
                    "rent %": pct(g, "rentSharePct"),
                })
            })
            .collect::<Vec<_>>(),
    );

    println!("\nBY CITY");
    output::table(
        &arr("byCity")
            .iter()
            .map(|g| {
                json!({
                    "city": format!(
                        "{}, {}",
                        g["city"].as_str().unwrap_or("?"),
                        g["state"].as_str().unwrap_or("?")
                    ),
                    "properties": g["properties"],
                    "value": usd(g, "valueUsd", 2), "value %": pct(g, "valueSharePct"),
                    "rent %": pct(g, "rentSharePct"),
                })
            })
            .collect::<Vec<_>>(),
    );

    println!("\nBY PROPERTY");
    output::table(
        &arr("properties")
            .iter()
            .map(|p| {
                json!({
                    "property": p["address"].as_str().unwrap_or_else(|| p["propertyId"].as_str().unwrap_or("?")),
                    "location": format!(
                        "{}, {}",
                        p["city"].as_str().unwrap_or("?"),
                        p["state"].as_str().unwrap_or("?")
                    ),
                    "value": usd(p, "valueUsd", 2), "value %": pct(p, "valueSharePct"),
                    "rent/day": usd(p, "dailyRentUsd", 4), "rent %": pct(p, "rentSharePct"),
                    "status": p["rentStatus"],
                })
            })
            .collect::<Vec<_>>(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const POSITIONS: &str = include_str!("../../tests/fixtures/account/positions-portfolio.json");
    const LISTINGS: &str = include_str!("../../tests/fixtures/portfolio/property-listings.json");

    fn inputs() -> (Vec<Value>, BTreeMap<String, Value>) {
        let positions: Value = serde_json::from_str(POSITIONS).unwrap();
        let listings: Value = serde_json::from_str(LISTINGS).unwrap();
        // The fixture is a list of `/properties/{id}` responses, envelope and all.
        let listings = listings
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let p = r["property"].clone();
                (p["id"].as_str().unwrap().to_string(), p)
            })
            .collect();
        (positions["positions"].as_array().unwrap().clone(), listings)
    }

    fn report() -> Value {
        let (p, l) = inputs();
        portfolio(&p, &l)
    }

    fn close(a: &Value, b: f64) {
        let a = a.as_f64().unwrap_or_else(|| panic!("not a number: {a}"));
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    fn find<'a>(arr: &'a Value, k: &str, want: &str) -> &'a Value {
        arr.as_array()
            .unwrap()
            .iter()
            .find(|x| x[k] == want)
            .unwrap_or_else(|| panic!("no {k}={want}"))
    }

    #[test]
    fn weights_holdings_by_state_on_estimated_value() {
        // Fixture: OH $4000 + $2000, MI $3000, TN $1000 → $10000 total.
        let r = report();
        close(&r["totals"]["valueUsd"], 10_000.0);
        close(&find(&r["byState"], "state", "OH")["valueSharePct"], 60.0);
        close(&find(&r["byState"], "state", "MI")["valueSharePct"], 30.0);
        close(&find(&r["byState"], "state", "TN")["valueSharePct"], 10.0);
        // Largest weighting first.
        assert_eq!(r["byState"][0]["state"], "OH");
        assert_eq!(r["byState"][0]["properties"], 2);
    }

    #[test]
    fn cities_are_keyed_with_their_state() {
        let r = report();
        let cities = r["byCity"].as_array().unwrap();
        assert_eq!(cities.len(), 4);
        let nash = find(&r["byCity"], "city", "Nashville");
        assert_eq!(nash["state"], "TN");
        close(&nash["valueSharePct"], 10.0);
    }

    #[test]
    fn fully_sold_positions_are_not_holdings() {
        // The fixture carries a zero-token position; it must not count anywhere.
        let r = report();
        assert_eq!(r["totals"]["properties"], 4);
        assert_eq!(r["properties"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn rent_share_excludes_non_paying_properties() {
        // Renting: A ($3650/yr over 1000 tokens, hold 80) and C ($7300/yr over
        // 2000 tokens, hold 60). B is vacant, D delinquent.
        let r = report();
        let a = find(&r["properties"], "propertyId", "01SAMPLEPROP0000000000000A");
        let c = find(&r["properties"], "propertyId", "01SAMPLEPROP0000000000000C");
        close(&a["dailyRentUsd"], 3650.0 / 365.0 * 80.0 / 1000.0); // $0.80
        close(&c["dailyRentUsd"], 7300.0 / 365.0 * 60.0 / 2000.0); // $0.60
        close(&r["totals"]["dailyRentUsd"], 1.4);
        close(&a["rentSharePct"], 0.8 / 1.4 * 100.0);
        close(&c["rentSharePct"], 0.6 / 1.4 * 100.0);
        for id in ["01SAMPLEPROP0000000000000B", "01SAMPLEPROP0000000000000D"] {
            let p = find(&r["properties"], "propertyId", id);
            assert!(p["rentSharePct"].is_null(), "{id} should be excluded");
            close(&p["dailyRentUsd"], 0.0);
            assert_eq!(p["renting"], false);
        }
    }

    #[test]
    fn reports_why_a_property_is_not_paying() {
        let r = report();
        let status = |id: &str| find(&r["properties"], "propertyId", id)["rentStatus"].clone();
        assert_eq!(status("01SAMPLEPROP0000000000000A"), "renting");
        assert_eq!(status("01SAMPLEPROP0000000000000B"), "vacant");
        assert_eq!(status("01SAMPLEPROP0000000000000D"), "delinquent");
    }

    #[test]
    fn renting_share_by_count_and_by_value() {
        // 2 of 4 properties pay rent; they hold $4000 + $3000 of $10000.
        let r = report();
        let t = &r["totals"];
        assert_eq!(t["rentingProperties"], 2);
        assert_eq!(t["nonRentingProperties"], 2);
        close(&t["rentingPropertySharePct"], 50.0);
        close(&t["rentingValueSharePct"], 70.0);
    }

    #[test]
    fn a_property_with_no_listing_is_still_weighted() {
        // Dropping it would inflate every other share; it lands under `unknown`
        // and counts as non-renting.
        let (positions, mut listings) = inputs();
        listings.remove("01SAMPLEPROP0000000000000D");
        let r = portfolio(&positions, &listings);
        close(&r["totals"]["valueUsd"], 10_000.0);
        let d = find(&r["properties"], "propertyId", "01SAMPLEPROP0000000000000D");
        assert_eq!(d["rentStatus"], "unknown");
        assert_eq!(d["state"], "unknown");
        close(
            &find(&r["byState"], "state", "unknown")["valueSharePct"],
            10.0,
        );
    }

    #[test]
    fn zero_cash_flow_counts_as_not_renting() {
        let mut l = BTreeMap::new();
        l.insert(
            "X".to_string(),
            json!({"is_occupied": true, "projected_annual_cash_flow": 0, "tokens": 10}),
        );
        assert_eq!(rent_status(l.get("X")), "no-cash-flow");
    }

    #[test]
    fn an_empty_portfolio_has_no_shares_rather_than_nan() {
        let r = portfolio(&[], &BTreeMap::new());
        assert_eq!(r["totals"]["properties"], 0);
        assert!(r["totals"]["rentingValueSharePct"].is_null());
        assert!(r["byState"].as_array().unwrap().is_empty());
    }
}
