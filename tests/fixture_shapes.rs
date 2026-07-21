//! Contract tests against the raw captured API responses in
//! `tests/fixtures/` (independent files by design — open them to see exactly
//! what the API returns). If Lofty changes a shape, re-capture the fixture and
//! these tests show precisely which CLI views are affected.

use serde_json::Value;

fn fixture(rel: &str) -> Value {
    let path = format!("{}/tests/fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parsing {path}: {e}"))
}

#[test]
fn properties_list_has_the_columns_the_table_view_uses() {
    let v = fixture("public/properties-list.json");
    let props = v.pointer("/result/properties").unwrap().as_array().unwrap();
    assert!(!props.is_empty());
    for p in props {
        for field in [
            "id",
            "address_line1",
            "city",
            "state",
            "sale_price",
            "tokens",
            "coc",
            "cap_rate",
            "slug",
        ] {
            assert!(p.get(field).is_some(), "property missing `{field}`");
        }
        // SDK 0.2.3 trimmed the public list; the internal-only admin fields are gone.
        for gone in ["hideMkt", "hide_details", "reserveOwnerId", "dao_app_id"] {
            assert!(
                p.get(gone).is_none(),
                "expected `{gone}` to be trimmed from the public list"
            );
        }
    }
    // The MM `trading`/`liquidity` blocks appear only on properties with active
    // order state — present on some rows, not guaranteed on all.
    assert!(
        props.iter().any(|p| p.get("trading").is_some()),
        "no property carries a trading block"
    );
    assert!(
        props.iter().any(|p| p.get("liquidity").is_some()),
        "no property carries a liquidity block"
    );
    assert!(v.get("page").is_some() && v.get("pageSize").is_some());
}

#[test]
fn orderbook_has_bids_and_asks() {
    let v = fixture("public/property-orderbook.json");
    // SDK 0.2.3 aligned the API with the README: orderbook.{bids,asks}, each an
    // array of {price, quantity}. (Older responses nested orderBook.buyOrders.)
    let book = v.pointer("/orderbook").expect("orderbook");
    let bids = book["bids"].as_array().expect("bids array");
    assert!(!bids.is_empty(), "fixture should have at least one bid");
    for o in bids {
        for field in ["price", "quantity"] {
            assert!(o.get(field).is_some(), "order missing `{field}`");
        }
    }
    assert!(book.get("asks").is_some(), "orderbook should carry asks");
}

#[test]
fn lp_programs_carry_every_qualification_rule() {
    let v = fixture("public/lp-programs.json");
    let programs = v["programs"].as_array().unwrap();
    assert!(!programs.is_empty(), "no LP programs in fixture");
    for p in programs {
        for field in [
            "propertyId",
            "dailyRewards",
            "perBlockRewards",
            "blockDurationMs",
            "blocksPerDay",
            "allowedSpread",
            "minContracts",
            "minTwoSidedLiquidity",
            "minOrderAgeMs",
            "slug",
        ] {
            assert!(p.get(field).is_some(), "program missing `{field}`");
        }
        // Economic sanity: hourly blocks that sum to the daily pool.
        let daily = p["dailyRewards"].as_f64().unwrap();
        let per_block = p["perBlockRewards"].as_f64().unwrap();
        let blocks = p["blocksPerDay"].as_f64().unwrap();
        assert!((per_block * blocks - daily).abs() < 1e-6);
    }
}

#[test]
fn account_balance_has_all_four_balances() {
    let v = fixture("account/balance.json");
    for field in ["usdc", "algo", "rentBalance", "giftBalance"] {
        assert!(v.get(field).is_some(), "balance missing `{field}`");
    }
}

#[test]
fn positions_expose_cost_basis_for_pnl() {
    let v = fixture("account/positions.json");
    for p in v["positions"].as_array().unwrap() {
        for field in ["propertyId", "currentTokens", "costBasis", "currentValue"] {
            assert!(p.get(field).is_some(), "position missing `{field}`");
        }
    }
    assert!(v["totals"].get("totalProperties").is_some());
}

#[test]
fn orders_list_has_status_lifecycle_fields() {
    let v = fixture("account/orders-list.json");
    for o in v["orders"].as_array().unwrap() {
        for field in [
            "orderId",
            "propertyId",
            "direction",
            "price",
            "quantity",
            "status",
        ] {
            assert!(o.get(field).is_some(), "order missing `{field}`");
        }
    }
}

#[test]
fn amm_quote_has_slippage_analysis() {
    let v = fixture("public/amm-quote-buy.json");
    for field in [
        "poolId",
        "side",
        "tokenAmount",
        "usdcAmount",
        "usdcPerToken",
        "referencePrice",
        "slippage",
        "priceImpact",
    ] {
        assert!(v.get(field).is_some(), "quote missing `{field}`");
    }
}

#[test]
fn amm_pools_expose_two_sided_prices() {
    let v = fixture("public/amm-pools.json");
    let pools = v["pools"].as_array().unwrap();
    assert!(!pools.is_empty());
    for p in pools {
        for field in ["poolId", "priceLow", "priceHigh"] {
            assert!(p.get(field).is_some(), "pool missing `{field}`");
        }
    }
}

#[test]
fn internal_marketplace_fixture_keeps_mm_economics() {
    // From the internal /prod API (api --internal): fee schedule + depth per listing.
    let v = fixture("marketplace.json");
    let props = v.pointer("/data/properties").unwrap().as_array().unwrap();
    for p in props {
        let liq = p.get("liquidity").expect("liquidity block");
        for field in [
            "mtBuyFeePct",
            "mtSellFeePct",
            "lpFeePct",
            "baseStaked",
            "quoteStaked",
        ] {
            assert!(liq.get(field).is_some(), "liquidity missing `{field}`");
        }
    }
}
