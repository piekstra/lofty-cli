//! The tools over the scrubbed fixtures in `tests/fixtures/`, through a seam that answers
//! from them and records every request.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use pk_cli_mcp::conformance::{assert_conforms, assert_fits, call, definitions_size};
use pk_cli_mcp::{tools_list_json, CallToolResult};
use serde_json::{json, Value};

use super::*;
use crate::client::{forbidden, with_status};

const PROP: &str = "01SAMPLEPROP00000000000001";

fn fixture(rel: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

type Answer = Result<Value, fn() -> CliError>;

/// A path and its query.
type Request = (String, Vec<(String, String)>);

#[derive(Clone, Default)]
struct Fake {
    answers: Rc<RefCell<HashMap<String, Answer>>>,
    /// What a listing lookup answers with, for any id not in `answers`.
    any_listing: Rc<RefCell<Option<Answer>>>,
    requests: Rc<RefCell<Vec<Request>>>,
}

impl Fake {
    fn answering(self, path: &str, answer: Value) -> Self {
        self.answers.borrow_mut().insert(path.into(), Ok(answer));
        self
    }

    fn failing(self, path: &str, e: fn() -> CliError) -> Self {
        self.answers.borrow_mut().insert(path.into(), Err(e));
        self
    }

    fn listings_answering(self, answer: Answer) -> Self {
        *self.any_listing.borrow_mut() = Some(answer);
        self
    }

    fn requests(&self) -> Vec<String> {
        self.requests
            .borrow()
            .iter()
            .map(|(p, _)| p.clone())
            .collect()
    }

    fn queries(&self, path: &str) -> Vec<Vec<(String, String)>> {
        self.requests
            .borrow()
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, q)| q.clone())
            .collect()
    }
}

impl Reads for Fake {
    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, CliError> {
        self.requests.borrow_mut().push((
            path.to_string(),
            query
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        ));
        let answer = self.answers.borrow().get(path).cloned();
        let answer = answer.or_else(|| {
            path.starts_with("/public/v1/properties/")
                .then(|| self.any_listing.borrow().clone())
                .flatten()
        });
        match answer {
            Some(Ok(v)) => Ok(v),
            Some(Err(e)) => Err(e()),
            None => panic!("no fixture for {path}"),
        }
    }
}

impl Source for Fake {
    type Api = Fake;
    fn open(&self) -> Result<Fake, CliError> {
        Ok(self.clone())
    }
}

fn account_fake() -> Fake {
    Fake::default()
        .answering(
            "/public/v1/account/balance",
            fixture("account/balance.json"),
        )
        .answering(
            "/public/v1/account/positions",
            fixture("account/positions.json"),
        )
        .listings_answering(Ok(
            json!({"property": {"address": "100 Sample St, Sampleton"}}),
        ))
}

fn full_fake() -> Fake {
    account_fake()
        .answering("/public/v1/orders", fixture("account/orders-list.json"))
        .answering("/public/v1/account/trades", fixture("account/trades.json"))
        .answering(
            &format!("/public/v1/properties/{PROP}"),
            fixture("public/property-get.json"),
        )
        .answering(
            &format!("/public/v1/properties/{PROP}/orderbook"),
            fixture("public/property-orderbook.json"),
        )
        .answering(
            &format!("/public/v1/properties/{PROP}/trades"),
            fixture("public/property-trades.json"),
        )
}

fn tools(fake: &Fake) -> LoftyTools<Fake> {
    LoftyTools::new(fake.clone())
}

fn text_of(result: &CallToolResult) -> String {
    result.text().to_string()
}

fn ok(fake: &Fake, tool: &str, args: Value) -> Value {
    let t = tools(fake);
    let result = call(&t, tool, args);
    assert_fits(&t, tool, &result).clone()
}

fn error(fake: &Fake, tool: &str, args: Value) -> String {
    let result = call(&tools(fake), tool, args);
    assert!(result.is_error(), "{}", text_of(&result));
    text_of(&result)
}

fn all_tool_calls() -> Vec<(&'static str, Value)> {
    vec![
        (
            ACCOUNT_TOOL,
            json!({"include_sold": true, "response_format": "detailed"}),
        ),
        (
            ORDERS_TOOL,
            json!({"status": "all", "response_format": "detailed"}),
        ),
        (
            TRADES_TOOL,
            json!({"direction": "buy", "property_id": PROP, "response_format": "detailed"}),
        ),
        (
            PROPERTY_TOOL,
            json!({"property_id": PROP, "response_format": "detailed"}),
        ),
    ]
}

// ---- the profile -------------------------------------------------------------

#[test]
fn the_tools_conform_to_the_profile() {
    let fake = full_fake();
    assert_conforms(&tools(&fake));
    assert!(definitions_size(&tools(&fake)) < 8_000);
}

#[test]
fn listing_the_tools_opens_nothing() {
    struct Refuses;
    impl Source for Refuses {
        type Api = Fake;
        fn open(&self) -> Result<Fake, CliError> {
            panic!("listing the tools must not read the config or the key");
        }
    }
    let tools = LoftyTools::new(Refuses);
    let names: Vec<String> = tools.tools().into_iter().map(|t| t.name).collect();
    assert_eq!(
        names,
        [ACCOUNT_TOOL, ORDERS_TOOL, TRADES_TOOL, PROPERTY_TOOL]
    );
    let _ = tools_list_json(&tools);
}

#[test]
fn every_definition_is_a_read() {
    let fake = full_fake();
    let list = tools_list_json(&tools(&fake));
    for tool in list["tools"].as_array().unwrap() {
        assert_eq!(tool["annotations"]["readOnlyHint"], true, "{tool}");
        assert_eq!(tool["annotations"]["destructiveHint"], false, "{tool}");
    }
}

/// The seam has one method, a `GET`, so no tool can place, cancel or swap. This holds what
/// they do with it to a list.
#[test]
fn every_request_a_tool_makes_is_one_of_these_reads() {
    let fake = full_fake();
    for (tool, args) in all_tool_calls() {
        let result = call(&tools(&fake), tool, args.clone());
        assert!(!result.is_error(), "{tool}: {}", text_of(&result));
    }
    let requests = fake.requests();
    assert!(!requests.is_empty());
    for path in &requests {
        let allowed = matches!(
            path.as_str(),
            "/public/v1/account/balance"
                | "/public/v1/account/positions"
                | "/public/v1/account/trades"
                | "/public/v1/orders"
        ) || path
            .strip_prefix("/public/v1/properties/")
            .is_some_and(|rest| {
                let id = rest
                    .strip_suffix("/orderbook")
                    .or_else(|| rest.strip_suffix("/trades"))
                    .unwrap_or(rest);
                id.chars().all(|c| c.is_ascii_alphanumeric())
            });
        assert!(allowed, "{path}");
    }
}

#[test]
fn a_property_id_cannot_name_another_path() {
    let fake = full_fake();
    for bad in [
        "../account/balance",
        "a/b/c/d/e/f/g",
        "01SAMPLE?x=1",
        "short",
        "%2e%2e%2faccount",
    ] {
        for tool in [PROPERTY_TOOL, ACCOUNT_TOOL, ORDERS_TOOL, TRADES_TOOL] {
            let says = error(&fake, tool, json!({ "property_id": bad }));
            assert!(says.starts_with("usage:"), "{says}");
        }
    }
    assert!(fake.requests().is_empty(), "{:?}", fake.requests());
}

/// Dummy values: the fixture's `01SAMPLE…` ids stand for what must not come back.
#[test]
fn no_result_carries_the_account_id_or_a_chain_transaction() {
    let fake = full_fake();
    for (tool, args) in all_tool_calls() {
        let out = ok(&fake, tool, args).to_string();
        assert!(!out.contains("userId"), "{out}");
        assert!(!out.contains("01SAMPLE0000000000000000ID"), "{out}");
        assert!(!out.contains("TXNSAMPLE"), "{out}");
        assert!(!out.contains("ownerId"), "{out}");
    }
}

// ---- lofty_account ---------------------------------------------------------------

#[test]
fn the_account_has_the_balances_and_each_holding() {
    let fake = account_fake();
    let out = ok(&fake, ACCOUNT_TOOL, json!({}));
    assert_eq!(out["usdc"], "1234.56");
    assert_eq!(out["rent_balance"], "42.31");
    assert_eq!(out["gift_balance"], "0");
    assert_eq!(out["properties_held"], 1);
    assert_eq!(out["total_value"], "5100.00");
    assert_eq!(out["total_rent_earned"], "87.65");
    let item = &out["items"][0];
    assert_eq!(item["property_id"], PROP);
    assert_eq!(item["address"], "100 Sample St, Sampleton");
    assert_eq!(item["tokens"], 100.0);
    assert_eq!(item["cost_basis"], "5000");

    let out = ok(&fake, ACCOUNT_TOOL, json!({"response_format": "detailed"}));
    assert_eq!(out["items"][0]["total_spent"], "5000");
    assert!(item.get("total_spent").is_none(), "concise");
}

fn positions(rows: Value) -> Fake {
    account_fake().answering("/public/v1/account/positions", json!({ "positions": rows }))
}

#[test]
fn a_sold_position_is_history_unless_asked_for() {
    let fake = positions(json!([
        {"propertyId": "01SAMPLEPROP00000000000001", "currentTokens": 5, "currentValue": 10.0},
        {"propertyId": "01SAMPLEPROP00000000000002", "currentTokens": 0, "currentValue": 0.0},
    ]));
    let out = ok(&fake, ACCOUNT_TOOL, json!({}));
    assert_eq!(out["items"].as_array().unwrap().len(), 1);
    assert_eq!(out["properties_held"], 1);

    let out = ok(&fake, ACCOUNT_TOOL, json!({"include_sold": true}));
    assert_eq!(out["items"].as_array().unwrap().len(), 2);
    assert_eq!(out["properties_held"], 1);
}

#[test]
fn only_the_page_is_looked_up_and_the_total_covers_every_holding() {
    let rows: Vec<Value> = (0..3)
        .map(|i| json!({"propertyId": format!("01SAMPLEPROP0000000000000{i}"), "currentTokens": 1, "currentValue": 1.5}))
        .collect();
    let fake = positions(json!(rows));
    let out = ok(&fake, ACCOUNT_TOOL, json!({"limit": 1}));
    assert_eq!(out["items"].as_array().unwrap().len(), 1);
    assert_eq!(out["total_value"], "4.50");
    let lookups = fake
        .requests()
        .iter()
        .filter(|p| p.starts_with("/public/v1/properties/"))
        .count();
    assert_eq!(lookups, 1);
}

#[test]
fn a_total_lofty_did_not_give_is_absent_and_not_zero() {
    let fake = positions(json!([
        {"propertyId": "01SAMPLEPROP00000000000001", "currentTokens": 5, "currentValue": 10.0},
        {"propertyId": "01SAMPLEPROP00000000000002", "currentTokens": 5},
    ]));
    let out = ok(&fake, ACCOUNT_TOOL, json!({}));
    assert!(out.get("total_value").is_none(), "{out}");
    assert!(out.get("total_rent_earned").is_none(), "{out}");
}

#[test]
fn an_address_that_cannot_be_had_is_left_out_and_said() {
    let fake = account_fake().listings_answering(Err(|| CliError::NotFound("gone".into())));
    let out = ok(&fake, ACCOUNT_TOOL, json!({}));
    assert!(out["items"][0].get("address").is_none(), "{out}");
    assert!(
        out["note"].as_str().unwrap().contains("1 addresses"),
        "{out}"
    );
    assert_eq!(out["usdc"], "1234.56");
}

#[test]
fn a_rate_limit_stops_the_lookups_and_a_bad_key_fails_the_call() {
    let rows: Vec<Value> = (0..3)
        .map(|i| json!({"propertyId": format!("01SAMPLEPROP0000000000000{i}"), "currentTokens": 1}))
        .collect();
    let fake = positions(json!(rows))
        .listings_answering(Err(|| CliError::Upstream("rate limited (x): slow".into())));
    let out = ok(&fake, ACCOUNT_TOOL, json!({}));
    assert!(
        out["note"].as_str().unwrap().contains("3 addresses"),
        "{out}"
    );
    let lookups = fake
        .requests()
        .iter()
        .filter(|p| p.starts_with("/public/v1/properties/"))
        .count();
    assert_eq!(lookups, 1);

    let fake = account_fake().listings_answering(Err(|| {
        CliError::Auth("invalid or revoked API key (x): no".into())
    }));
    let says = error(&fake, ACCOUNT_TOOL, json!({}));
    assert!(says.starts_with("auth:"), "{says}");
}

#[test]
fn an_answer_of_the_wrong_shape_is_an_error_and_not_a_zero() {
    for (path, body) in [
        ("/public/v1/account/balance", json!({"algo": 1.0})),
        ("/public/v1/account/balance", json!({"usdc": "12.00"})),
        ("/public/v1/account/positions", json!({})),
        ("/public/v1/account/positions", json!({"positions": {}})),
        (
            "/public/v1/account/positions",
            json!({"positions": [{"propertyId": PROP}]}),
        ),
        (
            "/public/v1/account/positions",
            json!({"positions": [{"propertyId": PROP, "currentTokens": "5"}]}),
        ),
        (
            "/public/v1/account/positions",
            json!({"positions": [{"propertyId": PROP, "currentTokens": 5, "costBasis": "1"}]}),
        ),
    ] {
        let fake = account_fake().answering(path, body.clone());
        let says = error(&fake, ACCOUNT_TOOL, json!({}));
        assert!(says.starts_with("upstream:"), "{body}: {says}");
    }
}

// ---- lofty_orders ----------------------------------------------------------------

#[test]
fn orders_are_the_resting_ones_unless_asked_and_newest_first() {
    let fake = full_fake();
    let out = ok(&fake, ORDERS_TOOL, json!({}));
    let items = out["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert!(items.iter().all(|o| o["status"] == "active"), "{out}");
    assert_eq!(out["matched"], items.len());

    let out = ok(&fake, ORDERS_TOOL, json!({"status": "all", "limit": 200}));
    let all = out["items"].as_array().unwrap();
    assert!(all.len() > items.len());
    let created: Vec<&str> = all.iter().map(|o| o["created"].as_str().unwrap()).collect();
    let mut sorted = created.clone();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(created, sorted);

    let out = ok(
        &fake,
        ORDERS_TOOL,
        json!({"status": "CANCELLED", "limit": 200}),
    );
    let cancelled = out["items"].as_array().unwrap();
    assert!(!cancelled.is_empty());
    assert!(
        cancelled.iter().all(|o| o["status"] == "cancelled"),
        "{out}"
    );
}

/// `?propertyId=` reports a partially filled order's original quantity.
#[test]
fn an_order_filter_is_applied_here_and_not_by_a_query() {
    let fake = full_fake();
    let out = ok(
        &fake,
        ORDERS_TOOL,
        json!({"status": "all", "direction": "sell", "property_id": PROP}),
    );
    assert!(
        out["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["direction"] == "sell" && o["property_id"] == PROP),
        "{out}"
    );
    let queries = fake.queries("/public/v1/orders");
    assert_eq!(queries, [[("all".to_string(), "true".to_string())]]);
}

#[test]
fn a_bad_direction_is_a_usage_error() {
    let fake = full_fake();
    for tool in [ORDERS_TOOL, TRADES_TOOL] {
        let says = error(&fake, tool, json!({"direction": "short"}));
        assert!(says.starts_with("usage:"), "{says}");
    }
    assert!(fake.requests().is_empty());
}

// ---- lofty_trades ----------------------------------------------------------------

#[test]
fn trades_come_newest_first_with_dates_and_the_users_side_of_the_fee() {
    let fake = full_fake();
    let out = ok(&fake, TRADES_TOOL, json!({"response_format": "detailed"}));
    let items = out["items"].as_array().unwrap();
    let dates: Vec<&str> = items.iter().map(|t| t["date"].as_str().unwrap()).collect();
    let mut sorted = dates.clone();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(dates, sorted);
    assert_eq!(out["complete"], true);
    let raw = fixture("account/trades.json");
    let first = raw["trades"]
        .as_array()
        .unwrap()
        .iter()
        .max_by_key(|t| t["createdAt"].as_i64())
        .unwrap();
    let want_fee = if first["direction"] == "buy" {
        &first["buyerFeeAmount"]
    } else {
        &first["sellerFeeAmount"]
    };
    assert_eq!(items[0]["fee"], want_fee.as_f64().unwrap().to_string());

    let out = ok(&fake, TRADES_TOOL, json!({}));
    assert!(out["items"][0].get("fee").is_none(), "concise");
}

#[test]
fn a_range_narrows_trades_and_a_bad_one_is_refused() {
    let fake = Fake::default().answering(
        "/public/v1/account/trades",
        json!({"trades": [
            {"propertyId": PROP, "direction": "buy", "createdAt": 1_767_225_600_000_i64},
            {"propertyId": PROP, "direction": "sell", "createdAt": 1_751_328_000_000_i64},
        ]}),
    );
    let out = ok(&fake, TRADES_TOOL, json!({"since": "2026-01-01"}));
    assert_eq!(out["matched"], 1);
    assert_eq!(out["items"][0]["date"], "2026-01-01");
    let out = ok(&fake, TRADES_TOOL, json!({"until": "2025-12-31"}));
    assert_eq!(out["items"][0]["date"], "2025-07-01");
    for args in [
        json!({"since": "2026-13-01"}),
        json!({"since": "2026-02-01", "until": "2026-01-01"}),
    ] {
        let says = error(&fake, TRADES_TOOL, args);
        assert!(says.starts_with("usage:"), "{says}");
    }
}

#[test]
fn as_many_trades_as_were_asked_for_is_an_incomplete_list() {
    let rows: Vec<Value> = (0..TRADES_ASKED)
        .map(|i| json!({"propertyId": PROP, "direction": "buy", "createdAt": 1_767_225_600_000_i64 + i as i64}))
        .collect();
    let fake = Fake::default().answering("/public/v1/account/trades", json!({ "trades": rows }));
    let out = ok(&fake, TRADES_TOOL, json!({}));
    assert_eq!(out["complete"], false);
    assert!(
        out["note"].as_str().unwrap().contains("older trades"),
        "{out}"
    );
}

#[test]
fn a_trade_list_lofty_did_not_give_is_an_error() {
    for body in [
        json!({}),
        json!({"trades": [{"propertyId": PROP, "direction": "buy"}]}),
    ] {
        let fake = Fake::default().answering("/public/v1/account/trades", body);
        let says = error(&fake, TRADES_TOOL, json!({}));
        assert!(says.starts_with("upstream:"), "{says}");
    }
}

// ---- what Lofty refuses ------------------------------------------------------------

/// Lofty's own answer to arguments it does not take.
#[test]
fn arguments_lofty_refuses_are_a_usage_error() {
    let fake = Fake::default().failing("/public/v1/account/trades", || {
        with_status(400, "invalid_direction", "not a direction")
    });
    let says = error(&fake, TRADES_TOOL, json!({"direction": "buy"}));
    assert!(says.starts_with("usage:"), "{says}");
    assert!(says.contains("invalid_direction"), "{says}");
}

// ---- lofty_property --------------------------------------------------------------

#[test]
fn a_property_has_its_figures_rent_status_and_both_price_sources() {
    let fake = full_fake();
    let raw = fixture("public/property-get.json");
    let feed = fixture("public/property-trades.json");
    let out = ok(&fake, PROPERTY_TOOL, json!({"property_id": PROP}));
    assert_eq!(out["id"], raw["property"]["id"]);
    assert_eq!(out["address"], raw["property"]["address"]);
    assert_eq!(out["rent_status"], "renting");
    assert_eq!(out["book_bid"], "11");
    assert_eq!(out["book_ask"], "11.9");
    assert_eq!(
        out["feed_bid"],
        feed["bestBid"].as_f64().unwrap().to_string()
    );
    assert_eq!(
        out["feed_ask"],
        feed["bestAsk"].as_f64().unwrap().to_string()
    );
    assert!(out["last_trade_price"].is_string(), "{out}");
    assert!(out.get("description").is_none(), "concise");
    assert!(out.get("bids").is_none(), "concise");
    assert!(out.get("beds").is_none(), "concise");

    let out = ok(
        &fake,
        PROPERTY_TOOL,
        json!({"property_id": PROP, "response_format": "detailed"}),
    );
    assert_eq!(out["description"], raw["property"]["description"]);
    assert_eq!(out["bids"][0]["price"], "11");
    assert_eq!(out["bids"][0]["quantity"], 42.0);
    assert_eq!(out["beds"], raw["property"]["beds"].as_f64().unwrap());
    assert!(out["asks"].as_array().unwrap().len() <= DETAIL_ROWS);
}

#[test]
fn a_book_lofty_did_not_give_is_an_error_and_not_an_empty_market() {
    let fake = full_fake().answering(
        &format!("/public/v1/properties/{PROP}/orderbook"),
        json!({}),
    );
    let says = error(&fake, PROPERTY_TOOL, json!({"property_id": PROP}));
    assert!(says.starts_with("upstream:"), "{says}");
}

/// The envelope `book::levels` also reads: `orderbook.orderBook.{buyOrders,sellOrders}`.
#[test]
fn the_legacy_book_envelope_is_read_too() {
    let fake = full_fake().answering(
        &format!("/public/v1/properties/{PROP}/orderbook"),
        fixture("competitiveness/orderbook-legacy-ids.json"),
    );
    let out = ok(&fake, PROPERTY_TOOL, json!({"property_id": PROP}));
    assert_eq!(out["book_bid"], "49.81");
    assert_eq!(out["book_ask"], "52.75");
}

#[test]
fn a_missing_property_names_where_ids_come_from() {
    let fake = Fake::default().failing(&format!("/public/v1/properties/{PROP}"), || {
        CliError::NotFound("not_found: no such property".into())
    });
    let says = error(&fake, PROPERTY_TOOL, json!({"property_id": PROP}));
    assert!(says.starts_with("not_found:"), "{says}");
    assert!(says.contains("lofty_account"), "{says}");
}

// ---- what a failure says ---------------------------------------------------------

fn failing_with(e: fn() -> CliError) -> String {
    let fake = Fake::default().failing(&format!("/public/v1/properties/{PROP}"), e);
    error(&fake, PROPERTY_TOOL, json!({"property_id": PROP}))
}

#[test]
fn a_missing_or_rejected_key_names_the_one_command_that_fixes_it() {
    let no_key: fn() -> CliError = || api_key(None, || Ok(None)).err().unwrap();
    let rejected: fn() -> CliError =
        || CliError::Auth("invalid or revoked API key (unauthorized): no".into());
    for e in [no_key, rejected] {
        let says = failing_with(e);
        assert!(says.starts_with("auth:"), "{says}");
        assert_eq!(
            says.matches("`lofty auth login --overwrite`").count(),
            1,
            "{says}"
        );
    }
}

/// A 403 here can be a wrong key or an id that does not exist (docs/api.md), and these
/// reads are not trading calls.
#[test]
fn a_403_says_what_it_can_mean_and_not_to_enable_trading() {
    let says = failing_with(|| forbidden("forbidden", "Forbidden"));
    assert!(says.starts_with("auth:"), "{says}");
    assert!(says.contains("does not exist"), "{says}");
    assert!(says.contains("lofty_account"), "{says}");
    assert!(!says.contains("enable Trading"), "{says}");
}

#[test]
fn a_keychain_that_could_not_be_used_is_not_a_bad_key() {
    let says = failing_with(|| CliError::Keychain("reading credential: denied".into()));
    let fix = if cfg!(target_os = "macos") {
        "Keychain Access"
    } else {
        "desktop keyring"
    };
    assert!(says.contains(fix), "{says}");
    assert!(!says.contains("auth login"), "{says}");
}

#[test]
fn a_rate_limit_asks_to_wait() {
    let says = failing_with(|| CliError::Upstream("rate limited (x): slow".into()));
    assert!(says.starts_with("upstream:"), "{says}");
    assert!(says.contains("Try again"), "{says}");
}

// ---- the key ---------------------------------------------------------------------

fn dummy_key() -> String {
    format!("lofty_test_{}", "a".repeat(32))
}

#[test]
fn the_environment_wins_and_the_keychain_is_not_read() {
    let key = api_key(Some(dummy_key()), || panic!("the keychain was read")).unwrap();
    assert_eq!(key.expose(), dummy_key());
}

#[test]
fn an_empty_variable_falls_through_to_the_keychain() {
    let key = api_key(Some("  ".into()), || Ok(Some(Secret::new(dummy_key())))).unwrap();
    assert_eq!(key.expose(), dummy_key());
}

#[test]
fn a_malformed_variable_is_refused_without_repeating_it() {
    let e = api_key(Some("not-a-key-value".into()), || panic!("read"))
        .err()
        .unwrap();
    let m = e.to_string();
    assert!(m.contains(KEY_VAR), "{m}");
    assert!(!m.contains("not-a-key-value"), "{m}");
}

#[test]
fn iso_date_is_the_utc_date() {
    assert_eq!(iso_date(1_767_225_600_000.0), "2026-01-01");
    assert_eq!(iso_date(1_767_225_599_999.0), "2025-12-31");
}
