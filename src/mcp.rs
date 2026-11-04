//! `lofty mcp`: the user's Lofty account and one property's market as MCP tools (cli-common
//! DESIGN.md §1.9, the MCP profile), for Claude Code, Claude Desktop and any MCP client.
//!
//! Four read-only tools: `lofty_account`, `lofty_orders`, `lofty_trades` and
//! `lofty_property`. This CLI can place and cancel orders and swap, on the same API key,
//! so a tool cannot: it reaches Lofty only through [`Reads`], whose one method is a `GET`,
//! over paths built here from fixed text and a checked property id.
//!
//! The key is read once per call, from `$LOFTY_API_KEY` (how the Claude Desktop extension
//! passes it) and then the keychain, and is not held between calls, so a new one applies on
//! the next. A rejected key's error names where that key came from, since `auth login` does
//! not replace a variable. An API key does not rotate, so nothing here renews anything or
//! takes a lock.
//!
//! An answer Lofty did not give is an error and never a zero: a field of the wrong type
//! fails the call, a field Lofty left out is absent from the result, and no result carries
//! the account's `userId` or a chain transaction id.

use std::time::{Duration, Instant};

use pk_cli_config::ConfigStore;
use pk_cli_core::{dates, CliError, Reason};
use pk_cli_mcp::{
    parse_args, ListArgs, Order, Page, PageRequest, PositionKey, PositionRequest, Server, ToolDef,
    ToolError, ToolOutput, Toolset,
};
use pk_cli_secrets::{CredentialStore, Secret};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::LoftyClient;
use crate::commands::book::{self, BookSide};
use crate::commands::orders::is_open;
use crate::commands::portfolio::RentStatus;
use crate::config::{Config, KEYCHAIN_ACCOUNT};

pub const ACCOUNT_TOOL: &str = "lofty_account";
pub const ORDERS_TOOL: &str = "lofty_orders";
pub const TRADES_TOOL: &str = "lofty_trades";
pub const PROPERTY_TOOL: &str = "lofty_property";

/// How a person gives this CLI a key. `auth login` refuses to replace a stored one
/// without `--overwrite`, and `--overwrite` is harmless when there is none.
const SIGN_IN: &str = "lofty auth login --overwrite";

/// The environment variable the Claude Desktop extension passes its key in.
pub const KEY_VAR: &str = "LOFTY_API_KEY";
/// What to do about a key from [`KEY_VAR`], where `auth login` would not help: it writes the
/// keychain, which the variable wins over, and the extension's own `lofty` is not on PATH.
const CORRECT_THE_PASSED_KEY: &str = "Ask the user to correct the API key in the Claude Desktop \
     extension's settings, or in $LOFTY_API_KEY where the server is started, then call again";

/// The most trades Lofty is asked for. It says nothing when it has more.
const TRADES_ASKED: usize = 200;
/// How long a call spends looking up the addresses of its positions.
const ADDRESS_BUDGET: Duration = Duration::from_secs(30);
/// Book levels on each side of a detailed property.
const DETAIL_ROWS: usize = 10;

/// The one thing a tool can do at Lofty.
pub trait Reads {
    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, CliError>;
}

impl Reads for LoftyClient {
    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, CliError> {
        LoftyClient::get(self, path, query)
    }
}

/// What a call opens: its own client, with the config and key as they are now.
pub trait Source {
    type Api: Reads;
    fn open(&self) -> Result<Self::Api, CliError>;
    /// Whether a call's key comes from [`KEY_VAR`] and not the keychain.
    fn key_is_passed_in(&self) -> bool;
}

pub struct LiveSource {
    store: ConfigStore,
    creds: CredentialStore,
}

impl LiveSource {
    pub fn new(store: ConfigStore, creds: CredentialStore) -> Self {
        Self { store, creds }
    }
}

impl Source for LiveSource {
    type Api = LoftyClient;

    fn open(&self) -> Result<LoftyClient, CliError> {
        let cfg: Config = self.store.load()?;
        let key = api_key(std::env::var(KEY_VAR).ok(), || {
            self.creds.get(KEYCHAIN_ACCOUNT)
        })?;
        LoftyClient::with_key(&cfg, key.expose())
    }

    fn key_is_passed_in(&self) -> bool {
        passed_in(std::env::var(KEY_VAR).ok()).is_some()
    }
}

/// An empty variable is an unset one.
fn passed_in(from_env: Option<String>) -> Option<String> {
    from_env.filter(|k| !k.trim().is_empty())
}

/// The key a call uses. A malformed variable is refused here, without repeating it, rather
/// than sent to Lofty.
fn api_key(
    from_env: Option<String>,
    stored: impl FnOnce() -> Result<Option<Secret>, CliError>,
) -> Result<Secret, CliError> {
    match passed_in(from_env) {
        Some(key) if crate::looks_like_key(key.trim()) => Ok(Secret::new(key.trim())),
        Some(_) => Err(CliError::Auth(format!(
            "${KEY_VAR} is not a Lofty API key (expected lofty_live_… or lofty_test_…)"
        ))),
        None => stored()?.ok_or_else(|| CliError::Auth("no API key is stored".into())),
    }
}

/// A provider failure as a tool error, with the fix that one has.
fn tool_err(e: CliError, key_is_passed_in: bool) -> ToolError {
    match e.reason() {
        // Lofty's trap (docs/api.md): a wrong key, and a path or id that does not exist,
        // can both answer 403.
        Some(Reason::Forbidden) => {
            return ToolError::auth(format!(
                "Lofty refused this read, {}, which can mean a wrong key or an id that does not \
                 exist",
                e.message()
            ))
            .with_fix(if key_is_passed_in {
                format!(
                    "Check any property_id against lofty_account or lofty.ai. If it is right: \
                     {CORRECT_THE_PASSED_KEY}"
                )
            } else {
                "Check any property_id against lofty_account or lofty.ai. If it is right, ask \
                 the user to run `lofty auth login --overwrite` in a terminal, then call again"
                    .to_string()
            });
        }
        Some(Reason::BadRequest) => {
            return ToolError::usage(format!("Lofty refused these arguments: {}", e.message()));
        }
        _ => {}
    }
    match e {
        CliError::Auth(m) if key_is_passed_in => {
            ToolError::auth(m).with_fix(CORRECT_THE_PASSED_KEY)
        }
        CliError::NotFound(m) => ToolError::not_found(format!("Lofty has no such record: {m}"))
            .with_fix("Check property_id against lofty_account or lofty.ai, then call again"),
        // A person's answer to a prompt, or a keyring that is not running: not a bad key.
        CliError::Keychain(m) => ToolError::other(format!("the keychain could not be used: {m}"))
            .with_fix(if cfg!(target_os = "macos") {
                "Ask the user to allow `lofty` in the keychain prompt, or in Keychain Access, \
                     then call the tool again"
            } else {
                "Ask the user to unlock or start their desktop keyring, then call the tool \
                     again"
            }),
        other => other.into(),
    }
}

// ---- reading Lofty's answers ----------------------------------------------------

fn malformed(what: impl std::fmt::Display) -> ToolError {
    ToolError::upstream(format!("Lofty's answer was not the expected shape: {what}"))
}

fn rows<'a>(v: &'a Value, key: &str) -> Result<&'a Vec<Value>, ToolError> {
    v.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| malformed(format!("{key} is not a list")))
}

/// Absent or null is `None`: Lofty did not say. Any other type is an error.
fn present<'a>(row: &'a Value, key: &str) -> Option<&'a Value> {
    row.get(key).filter(|v| !v.is_null())
}

fn text(row: &Value, key: &str) -> Result<Option<String>, ToolError> {
    present(row, key)
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| malformed(format!("{key} is not text")))
        })
        .transpose()
}

fn number(row: &Value, key: &str) -> Result<Option<f64>, ToolError> {
    present(row, key)
        .map(|v| {
            v.as_f64()
                .ok_or_else(|| malformed(format!("{key} is not a number")))
        })
        .transpose()
}

/// A USD amount, as the decimal string Lofty's number prints as.
fn money(row: &Value, key: &str) -> Result<Option<String>, ToolError> {
    Ok(number(row, key)?.map(|n| n.to_string()))
}

fn required<T>(value: Option<T>, key: &str) -> Result<T, ToolError> {
    value.ok_or_else(|| malformed(format!("{key} is missing")))
}

/// A timestamp in milliseconds as its UTC date.
fn iso_date(ms: f64) -> String {
    dates::fmt_iso(dates::civil_from_unix((ms / 1000.0).floor() as i64))
}

/// A checked id, because it goes into a path: what a model passes cannot name another
/// endpoint, even for a read.
fn property_id(raw: Option<&str>) -> Result<Option<&str>, ToolError> {
    let Some(id) = non_empty(raw) else {
        return Ok(None);
    };
    if (10..=40).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric()) {
        Ok(Some(id))
    } else {
        Err(ToolError::usage(
            "property_id must be an id as lofty_account gives it, 10 to 40 letters and digits",
        ))
    }
}

fn direction(raw: Option<&str>) -> Result<Option<String>, ToolError> {
    match non_empty(raw).map(str::to_ascii_lowercase).as_deref() {
        None => Ok(None),
        Some(d @ ("buy" | "sell")) => Ok(Some(d.to_string())),
        Some(other) => Err(ToolError::usage(format!(
            "direction must be buy or sell, got `{other}`"
        ))),
    }
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

fn iso(name: &str, raw: Option<&str>) -> Result<Option<String>, ToolError> {
    non_empty(raw)
        .map(|d| {
            dates::parse_iso(d).map(dates::fmt_iso).map_err(|_| {
                ToolError::usage(format!("{name} must be a date as YYYY-MM-DD, got `{d}`"))
            })
        })
        .transpose()
}

/// A listing's own envelope, or the bare listing, as `venue_fees` accepts.
fn listing_of(v: &Value) -> &Value {
    v.get("property").unwrap_or(v)
}

fn address_of(listing: &Value) -> Result<Option<String>, ToolError> {
    if let Some(whole) = text(listing, "address")? {
        return Ok(Some(whole));
    }
    Ok(
        match (
            text(listing, "address_line1")?,
            text(listing, "address_line2")?,
        ) {
            (Some(a), Some(b)) => Some(format!("{a}, {b}")),
            (a, b) => a.or(b),
        },
    )
}

// ---- lofty_account --------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AccountArgs {
    /// Only this property.
    #[serde(default)]
    pub property_id: Option<String>,
    /// Also list properties fully sold. Default: false.
    #[serde(default)]
    pub include_sold: bool,
    #[serde(flatten)]
    pub list: ListArgs,
}

#[derive(Serialize)]
struct AccountFilter<'a> {
    property_id: Option<&'a str>,
    include_sold: bool,
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct PositionItem {
    pub property_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    pub tokens: f64,
    /// Paid for the tokens, without the buy fee.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_basis: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rent_earned: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_spent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_sell_value: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AccountOut {
    pub usdc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub algo: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rent_balance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gift_balance: Option<String>,
    /// Properties still held, across the account.
    pub properties_held: usize,
    /// The sum of their current_value. Absent when Lofty left one out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_value: Option<String>,
    /// Rent ever earned, sold properties included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_rent_earned: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(flatten)]
    pub page: Page<PositionItem>,
}

fn position(row: &Value, detailed: bool) -> Result<PositionItem, ToolError> {
    Ok(PositionItem {
        property_id: required(text(row, "propertyId")?, "propertyId")?,
        address: None,
        tokens: required(number(row, "currentTokens")?, "currentTokens")?,
        cost_basis: money(row, "costBasis")?,
        current_value: money(row, "currentValue")?,
        current_price: money(row, "currentPrice")?,
        rent_earned: money(row, "totalRentEarned")?,
        total_spent: money(row, "totalSpent")?.filter(|_| detailed),
        total_sell_value: money(row, "totalSellValue")?.filter(|_| detailed),
    })
}

// ---- lofty_orders ---------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OrdersArgs {
    /// Only this property.
    #[serde(default)]
    pub property_id: Option<String>,
    /// buy or sell.
    #[serde(default)]
    pub direction: Option<String>,
    /// Lofty's status, such as cancelled, or all. Default: active.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(flatten)]
    pub list: ListArgs,
}

#[derive(Serialize)]
struct OrdersFilter<'a> {
    property_id: Option<&'a str>,
    direction: Option<&'a str>,
    status: Option<&'a str>,
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct OrderItem {
    pub order_id: String,
    pub property_id: String,
    pub direction: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    /// Tokens still open, on an active order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<f64>,
    pub status: String,
    pub created: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct OrdersOut {
    /// Orders matching, across every page.
    pub matched: usize,
    #[serde(flatten)]
    pub page: Page<OrderItem>,
}

// ---- lofty_trades ---------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TradesArgs {
    /// Only this property.
    #[serde(default)]
    pub property_id: Option<String>,
    /// buy or sell.
    #[serde(default)]
    pub direction: Option<String>,
    /// First day, UTC, YYYY-MM-DD.
    #[serde(default)]
    pub since: Option<String>,
    /// Last day, UTC, YYYY-MM-DD.
    #[serde(default)]
    pub until: Option<String>,
    #[serde(flatten)]
    pub list: ListArgs,
}

#[derive(Serialize)]
struct TradesFilter<'a> {
    property_id: Option<&'a str>,
    direction: Option<&'a str>,
    since: Option<&'a str>,
    until: Option<&'a str>,
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct TradeItem {
    pub property_id: String,
    pub direction: String,
    pub date: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<f64>,
    /// The fee on the user's side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trade_id: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TradesOut {
    /// Trades matching, across every page.
    pub matched: usize,
    /// False when older trades can be missing.
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(flatten)]
    pub page: Page<TradeItem>,
}

// ---- lofty_property -------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PropertyArgs {
    /// The property's id, from lofty_account.
    pub property_id: String,
    #[serde(default)]
    pub response_format: Option<pk_cli_mcp::ResponseFormat>,
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct BookLevel {
    pub price: String,
    pub quantity: f64,
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct PropertyOut {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub property_type: Option<String>,
    /// renting, vacant, delinquent, no-cash-flow or unknown.
    pub rent_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monthly_rent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cap_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coc: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projected_rental_yield: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projected_annual_return: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projected_annual_cash_flow: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_investment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book_bid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book_ask: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feed_bid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feed_ask: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_trade_price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_trade_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_investors: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub beds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baths: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sqft: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year_built: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sale_price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bids: Vec<BookLevel>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub asks: Vec<BookLevel>,
}

// ---- the toolset ----------------------------------------------------------------

/// The toolset `lofty mcp` serves.
pub struct LoftyTools<S: Source> {
    source: S,
}

impl<S: Source> LoftyTools<S> {
    pub fn new(source: S) -> Self {
        Self { source }
    }

    fn open(&self) -> Result<S::Api, ToolError> {
        self.source.open().map_err(|e| self.err(e))
    }

    fn err(&self, e: CliError) -> ToolError {
        tool_err(e, self.source.key_is_passed_in())
    }
}

impl<S: Source> Toolset for LoftyTools<S> {
    fn server(&self) -> Server {
        Server::new("lofty", env!("CARGO_PKG_VERSION"), "Lofty", "lofty")
            .sign_in_command(SIGN_IN)
            .instructions(
                "These tools are read-only: none can place or cancel an order or swap, whatever \
                 the API key allows. Money is USD as decimal strings; token counts and percents \
                 are numbers; dates are UTC. Each page reads Lofty again, at 300 reads a minute \
                 per key.",
            )
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![
            ToolDef::read(
                ACCOUNT_TOOL,
                "Lofty balances and holdings",
                "The user's Lofty balances, and each property's tokens, cost basis, value and rent \
                 earned. Use it for what the account is worth or has earned. It reads Lofty once \
                 per position on the page, for addresses.",
            )
            .input::<AccountArgs>()
            .output::<AccountOut>(),
            ToolDef::read(
                ORDERS_TOOL,
                "Lofty limit orders",
                "The user's limit orders, newest day first and in no time order within a day, resting ones unless status says \
                 otherwise. Use it to see what is on the book and at what price. An active \
                 order's quantity is what is still open.",
            )
            .input::<OrdersArgs>()
            .output::<OrdersOut>(),
            ToolDef::read(
                TRADES_TOOL,
                "Lofty executed trades",
                "The user's executed buys and sells, newest day first and in no time order within a day, from Lofty's latest 200. Use \
                 it for what was traded and when. complete says whether older ones can be \
                 missing.",
            )
            .input::<TradesArgs>()
            .output::<TradesOut>(),
            ToolDef::read(
                PROPERTY_TOOL,
                "One Lofty property and its market",
                "One property: its figures, whether it pays rent, and its best bid and ask from \
                 both the order book and the trade feed, which can disagree. Use it before \
                 discussing a price. Detailed adds the book and the description.",
            )
            .input::<PropertyArgs>()
            .output::<PropertyOut>(),
        ]
    }

    async fn call(&self, name: &str, args: Value) -> Result<ToolOutput, ToolError> {
        match name {
            ACCOUNT_TOOL => self.account(parse_args(args)?),
            ORDERS_TOOL => self.orders(parse_args(args)?),
            TRADES_TOOL => self.trades(parse_args(args)?),
            PROPERTY_TOOL => self.property(parse_args(args)?),
            other => Err(ToolError::other(format!("no handler for {other}"))),
        }
    }
}

impl<S: Source> LoftyTools<S> {
    fn account(&self, args: AccountArgs) -> Result<ToolOutput, ToolError> {
        let only = property_id(args.property_id.as_deref())?;
        let page = PageRequest::new(
            &args.list,
            &AccountFilter {
                property_id: only,
                include_sold: args.include_sold,
            },
        )?;
        let detailed = args.list.format().is_detailed();

        let api = self.open()?;
        let balance = api
            .get("/public/v1/account/balance", &[])
            .map_err(|e| self.err(e))?;
        let positions = api
            .get("/public/v1/account/positions", &[])
            .map_err(|e| self.err(e))?;

        let mut all = rows(&positions, "positions")?
            .iter()
            .map(|row| position(row, detailed))
            .collect::<Result<Vec<_>, _>>()?;
        all.retain(|p| p.tokens >= 0.0);
        let held: Vec<&PositionItem> = all.iter().filter(|p| p.tokens > 0.0).collect();
        let properties_held = held.len();
        let total_value = held
            .iter()
            .map(|p| p.current_value.as_deref()?.parse::<f64>().ok())
            .sum::<Option<f64>>()
            .map(|sum| format!("{sum:.2}"));
        let total_rent_earned = positions
            .get("totals")
            .map(|t| money(t, "totalRentEarned"))
            .transpose()?
            .flatten();

        let mut matched: Vec<PositionItem> = all
            .into_iter()
            .filter(|p| args.include_sold || p.tokens > 0.0)
            .filter(|p| only.is_none_or(|id| p.property_id == id))
            .collect();

        let window = page.offset()..page.offset().saturating_add(page.limit());
        let unlocated = locate(&api, &mut matched, window).map_err(|e| self.err(e))?;
        let note = (unlocated > 0).then(|| {
            format!(
                "{unlocated} addresses could not be looked up this time; lofty_property gives \
                 one by property_id."
            )
        });

        ToolOutput::new(&AccountOut {
            usdc: required(money(&balance, "usdc")?, "usdc")?,
            algo: number(&balance, "algo")?,
            rent_balance: money(&balance, "rentBalance")?,
            gift_balance: money(&balance, "giftBalance")?,
            properties_held,
            total_value,
            total_rent_earned,
            note,
            page: page.page(matched, "property_id")?,
        })
    }

    fn orders(&self, args: OrdersArgs) -> Result<ToolOutput, ToolError> {
        let only = property_id(args.property_id.as_deref())?;
        let side = direction(args.direction.as_deref())?;
        let status = non_empty(args.status.as_deref()).map(str::to_ascii_lowercase);
        let page = PositionRequest::new(
            &args.list,
            &OrdersFilter {
                property_id: only,
                direction: side.as_deref(),
                status: status.as_deref(),
            },
        )?;

        // `all=true` and a filter here: Lofty's `propertyId` query reports a partially
        // filled order's original quantity (commands/orders.rs).
        let answer = self
            .open()?
            .get("/public/v1/orders", &[("all", "true".into())])
            .map_err(|e| self.err(e))?;
        let mut kept: Vec<(PositionKey, OrderItem)> = Vec::new();
        for row in rows(&answer, "orders")? {
            let keep = match status.as_deref() {
                None => is_open(row),
                Some("all") => true,
                Some(wanted) => text(row, "status")?.as_deref() == Some(wanted),
            };
            if !keep {
                continue;
            }
            let created = required(number(row, "createdAt")?, "createdAt")?;
            let item = OrderItem {
                order_id: required(text(row, "orderId")?, "orderId")?,
                property_id: required(text(row, "propertyId")?, "propertyId")?,
                direction: required(text(row, "direction")?, "direction")?,
                price: money(row, "price")?,
                quantity: number(row, "quantity")?,
                status: required(text(row, "status")?, "status")?,
                created: iso_date(created),
                expires: number(row, "expireAt")?.map(iso_date),
            };
            if only.is_none_or(|id| item.property_id == id)
                && side.as_deref().is_none_or(|d| item.direction == d)
            {
                let key = PositionKey::new(item.created.as_str(), &item.order_id);
                kept.push((key, item));
            }
        }
        ToolOutput::new(&OrdersOut {
            matched: kept.len(),
            page: page.page(kept, Order::Descending, "property_id, direction or status")?,
        })
    }

    fn trades(&self, args: TradesArgs) -> Result<ToolOutput, ToolError> {
        let only = property_id(args.property_id.as_deref())?;
        let side = direction(args.direction.as_deref())?;
        let since = iso("since", args.since.as_deref())?;
        let until = iso("until", args.until.as_deref())?;
        if let (Some(s), Some(u)) = (&since, &until) {
            if s > u {
                return Err(ToolError::usage(format!(
                    "since ({s}) is after until ({u})"
                )));
            }
        }
        let page = PositionRequest::new(
            &args.list,
            &TradesFilter {
                property_id: only,
                direction: side.as_deref(),
                since: since.as_deref(),
                until: until.as_deref(),
            },
        )?;
        let detailed = args.list.format().is_detailed();

        let mut query = vec![("limit", TRADES_ASKED.to_string())];
        if let Some(id) = only {
            query.push(("propertyId", id.to_string()));
        }
        if let Some(d) = &side {
            query.push(("direction", d.clone()));
        }
        let answer = self
            .open()?
            .get("/public/v1/account/trades", &query)
            .map_err(|e| self.err(e))?;
        let trades = rows(&answer, "trades")?;
        let complete = trades.len() < TRADES_ASKED;

        let mut kept: Vec<(PositionKey, TradeItem)> = Vec::new();
        for row in trades {
            let created = required(number(row, "createdAt")?, "createdAt")?;
            let date = iso_date(created);
            let trade_direction = required(text(row, "direction")?, "direction")?;
            let trade_id = required(text(row, "tradeId")?, "tradeId")?;
            let fee_key = if trade_direction == "buy" {
                "buyerFeeAmount"
            } else {
                "sellerFeeAmount"
            };
            let item = TradeItem {
                property_id: required(text(row, "propertyId")?, "propertyId")?,
                direction: trade_direction,
                price: money(row, "price")?,
                quantity: number(row, "quantity")?,
                fee: money(row, fee_key)?.filter(|_| detailed),
                trade_id: detailed.then(|| trade_id.clone()),
                date,
            };
            if only.is_none_or(|id| item.property_id == id)
                && side.as_deref().is_none_or(|d| item.direction == d)
                && since.as_deref().is_none_or(|s| item.date.as_str() >= s)
                && until.as_deref().is_none_or(|u| item.date.as_str() <= u)
            {
                let key = PositionKey::new(item.date.as_str(), &trade_id);
                kept.push((key, item));
            }
        }
        ToolOutput::new(&TradesOut {
            matched: kept.len(),
            complete,
            note: (!complete).then(|| {
                format!(
                    "Lofty sent the {TRADES_ASKED} it was asked for, so older trades can be \
                     missing, and any count or total here can be short."
                )
            }),
            page: page.page(
                kept,
                Order::Descending,
                "property_id, direction or since/until",
            )?,
        })
    }

    fn property(&self, args: PropertyArgs) -> Result<ToolOutput, ToolError> {
        let id = property_id(Some(&args.property_id))?
            .ok_or_else(|| ToolError::usage("property_id is required"))?;
        let detailed = args.response_format.unwrap_or_default().is_detailed();

        let api = self.open()?;
        let got = api
            .get(&format!("/public/v1/properties/{id}"), &[])
            .map_err(|e| self.err(e))?;
        let listing = listing_of(&got);
        let answer_book = api
            .get(&format!("/public/v1/properties/{id}/orderbook"), &[])
            .map_err(|e| self.err(e))?;
        if answer_book.get("orderbook").is_none_or(|b| !b.is_object()) {
            return Err(malformed("orderbook is missing"));
        }
        let feed = api
            .get(&format!("/public/v1/properties/{id}/trades"), &[])
            .map_err(|e| self.err(e))?;

        let bids = book::levels(&answer_book, BookSide::Bids);
        let asks = book::levels(&answer_book, BookSide::Asks);
        let levels = |levels: &[book::Level]| -> Vec<BookLevel> {
            levels
                .iter()
                .take(DETAIL_ROWS)
                .filter(|_| detailed)
                .map(|l| BookLevel {
                    price: l.price.to_string(),
                    quantity: l.qty,
                })
                .collect()
        };
        let trades = rows(&feed, "recentTrades")?;
        let last = trades
            .iter()
            .map(|t| Ok((number(t, "createdAt")?, t)))
            .collect::<Result<Vec<_>, ToolError>>()?
            .into_iter()
            .filter_map(|(created, t)| created.map(|c| (c, t)))
            .max_by(|a, b| a.0.total_cmp(&b.0));
        let more = |v: Option<f64>| v.filter(|_| detailed);
        ToolOutput::new(&PropertyOut {
            id: required(text(listing, "id")?, "id")?,
            address: address_of(listing)?,
            property_type: text(listing, "property_type")?,
            rent_status: RentStatus::of(Some(listing)).as_str().to_string(),
            monthly_rent: money(listing, "monthly_rent")?,
            cap_rate: number(listing, "cap_rate")?,
            coc: number(listing, "coc")?,
            projected_rental_yield: number(listing, "projected_rental_yield")?,
            projected_annual_return: number(listing, "projected_annual_return")?,
            projected_annual_cash_flow: money(listing, "projected_annual_cash_flow")?,
            tokens: number(listing, "tokens")?,
            total_investment: money(listing, "total_investment")?,
            book_bid: book::best(&bids, BookSide::Bids).map(|p| p.to_string()),
            book_ask: book::best(&asks, BookSide::Asks).map(|p| p.to_string()),
            feed_bid: money(&feed, "bestBid")?,
            feed_ask: money(&feed, "bestAsk")?,
            last_trade_price: last.map(|(_, t)| money(t, "price")).transpose()?.flatten(),
            last_trade_date: last.map(|(created, _)| iso_date(created)),
            total_investors: number(&feed, "totalInvestors")?,
            beds: more(number(listing, "beds")?),
            baths: more(number(listing, "baths")?),
            sqft: more(number(listing, "sqft")?),
            year_built: more(number(listing, "year_built")?),
            sale_price: money(listing, "sale_price")?.filter(|_| detailed),
            description: text(listing, "description")?.filter(|_| detailed),
            bids: levels(&bids),
            asks: levels(&asks),
        })
    }
}

/// Fill in the address of the positions on this page: Lofty's positions name only an
/// id, which is no answer to what someone holds. An address is a label and no figure,
/// so one that cannot be had is left out and counted, including one Lofty answers 403
/// for, as it can a listing it no longer serves. A rejected key is still an error. A rate
/// limit or the budget stops the lookups, so they do not use up the key's reads. Returns
/// how many are missing.
fn locate<A: Reads>(
    api: &A,
    positions: &mut [PositionItem],
    window: std::ops::Range<usize>,
) -> Result<usize, CliError> {
    let deadline = Instant::now() + ADDRESS_BUDGET;
    let mut missing = 0;
    let mut stopped = false;
    let end = window.end.min(positions.len());
    let start = window.start.min(end);
    for position in &mut positions[start..end] {
        if stopped || Instant::now() >= deadline {
            missing += 1;
            continue;
        }
        let path = format!("/public/v1/properties/{}", position.property_id);
        match api.get(&path, &[]) {
            Ok(v) => match address_of(listing_of(&v)) {
                Ok(Some(address)) => position.address = Some(address),
                Ok(None) | Err(_) => missing += 1,
            },
            Err(e) if e.reason() == Some(Reason::Forbidden) => missing += 1,
            Err(e) if e.is_auth() || matches!(e, CliError::Keychain(_)) => return Err(e),
            Err(CliError::NotFound(_)) => missing += 1,
            Err(_) => {
                missing += 1;
                stopped = true;
            }
        }
    }
    Ok(missing)
}

#[cfg(test)]
mod tests;
