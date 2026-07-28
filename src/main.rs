//! `lofty` — piekstra-family CLI for the Lofty (lofty.ai) marketplace SDK API.
//!
//! Conforms to piekstra-cli/1. The SDK surface (`/public/v1/*`) is the primary
//! target; the internal platform API stays reachable via `api --internal`.

mod catalog;
mod client;
mod commands;
mod config;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use pk_cli_auth::{AuthStatus, LoginArgs, LogoutArgs, SetCredentialArgs};
use pk_cli_config::ConfigStore;
use pk_cli_core::info::{AuthInfo, CliInfo};
use pk_cli_core::{output, CliError, CommonArgs};
use pk_cli_secrets::CredentialStore;
use pk_cli_selfupdate::{SelfUpdateArgs, Updater};

use client::LoftyClient;
use commands::{
    account, amm, api, catalog as catalog_cmd, orders, properties, quote, rewards, Ctx,
};
use config::{Config, KEYCHAIN_ACCOUNT};

const BIN: &str = "lofty";
const REPO: &str = "piekstra/lofty-cli";

/// Lofty marketplace from the command line — market data, order books,
/// LP-reward programs, and (with a trading key) limit orders and AMM swaps.
#[derive(Parser, Debug)]
#[command(name = BIN, version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    common: CommonArgs,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// API-key management and session status.
    #[command(subcommand)]
    Auth(AuthCmd),
    /// Non-secret settings.
    #[command(subcommand)]
    Config(ConfigCmd),
    /// Marketplace properties: listings, order books, trades.
    #[command(subcommand)]
    Properties(properties::Cmd),
    /// Your limit orders: list, inspect, place, cancel.
    #[command(subcommand)]
    Orders(orders::Cmd),
    /// Your account: balance, positions, trade history.
    #[command(subcommand)]
    Account(account::Cmd),
    /// LP-reward (market-making) programs and payout history.
    #[command(subcommand)]
    Rewards(rewards::Cmd),
    /// AMM pools, quotes, and swaps.
    #[command(subcommand)]
    Amm(amm::Cmd),
    /// Safe primitives for moving a resting two-sided quote (dry run by default).
    #[command(subcommand)]
    Quote(quote::Cmd),
    /// Raw API passthrough (SDK surface; --internal for the website API).
    Api(api::Args),
    /// The observed internal platform endpoint inventory.
    Catalog(catalog_cmd::Args),
    /// Update to the latest release from GitHub.
    SelfUpdate(SelfUpdateArgs),
    /// Print a shell completion script.
    Completions { shell: Shell },
    /// Machine-readable capability discovery (cli-info/v1).
    Info,
}

#[derive(Subcommand, Debug)]
enum AuthCmd {
    /// Store your Lofty API key in the OS keychain (verified live first).
    Login(LoginArgs),
    /// Report credential state (auth-status/v1).
    Status,
    /// Clear the session; --forget also removes the stored API key.
    Logout(LogoutArgs),
    /// Raw keychain write for rotation / headless setup.
    SetCredential(SetCredentialArgs),
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Print the resolved config file path.
    Path,
    /// Show the effective configuration.
    Show,
    /// Set a config key (base_url, username).
    Set { key: String, value: String },
    /// Remove a config key.
    Unset { key: String },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(&cli) {
        std::process::exit(output::fail(&e, cli.common.json));
    }
}

fn run(cli: &Cli) -> Result<(), CliError> {
    let store = ConfigStore::new(BIN);
    let creds = CredentialStore::for_binary(BIN);
    let cfg: Config = store.load()?;
    let ctx = Ctx {
        common: &cli.common,
        cfg: &cfg,
        creds: &creds,
    };

    match &cli.command {
        Command::Auth(cmd) => auth(cli, cmd, &store, &creds, &cfg),
        Command::Config(cmd) => config_cmd(cli, cmd, &store),
        Command::Properties(cmd) => properties::run(&ctx, cmd),
        Command::Orders(cmd) => orders::run(&ctx, cmd),
        Command::Account(cmd) => account::run(&ctx, cmd),
        Command::Rewards(cmd) => rewards::run(&ctx, cmd),
        Command::Amm(cmd) => amm::run(&ctx, cmd),
        Command::Quote(cmd) => quote::run(&ctx, cmd),
        Command::Api(args) => api::run(&ctx, args),
        Command::Catalog(args) => catalog_cmd::run(&ctx, args),
        Command::SelfUpdate(args) => Updater {
            repo: REPO.into(),
            binary: BIN.into(),
            target: env!("BUILD_TARGET").into(),
            current: env!("CARGO_PKG_VERSION").into(),
        }
        .run(args, cli.common.json, cli.common.quiet),
        Command::Completions { shell } => {
            clap_complete::generate(*shell, &mut Cli::command(), BIN, &mut std::io::stdout());
            Ok(())
        }
        Command::Info => {
            let info = CliInfo::new(
                BIN,
                env!("CARGO_PKG_VERSION"),
                &format!("https://github.com/{REPO}"),
                AuthInfo {
                    required: true,
                    method: "password".into(),
                    login_hint: Some(format!("{BIN} auth login")),
                },
                &[
                    "properties",
                    "orders",
                    "account",
                    "rewards",
                    "amm",
                    "quote",
                    "api",
                    "catalog",
                ],
            );
            output::json(&serde_json::to_value(&info).unwrap());
            Ok(())
        }
    }
}

fn auth(
    cli: &Cli,
    cmd: &AuthCmd,
    store: &ConfigStore,
    creds: &CredentialStore,
    cfg: &Config,
) -> Result<(), CliError> {
    match cmd {
        AuthCmd::Login(args) => {
            if creds.get(KEYCHAIN_ACCOUNT)?.is_some() && !args.overwrite {
                return Err(CliError::Usage(
                    "an API key is already stored; pass --overwrite to replace it".into(),
                ));
            }
            let prompt = if args.non_interactive {
                None
            } else {
                Some("Lofty API key (lofty_live_…)")
            };
            let secret = args.source.read(prompt)?;
            if !looks_like_key(secret.expose()) {
                return Err(CliError::Usage(
                    "that doesn't look like a Lofty API key (expected lofty_live_… or lofty_test_…)"
                        .into(),
                ));
            }
            if !args.no_verify {
                let client = LoftyClient::with_key(cfg, secret.expose())?;
                client.get("/public/v1/account/balance", &[])?;
                if !cli.common.quiet {
                    eprintln!("key verified against /account/balance");
                }
            }
            creds.set(KEYCHAIN_ACCOUNT, &secret)?;
            if !cli.common.quiet {
                eprintln!("API key stored in the OS keychain ({})", creds.service());
            }
            Ok(())
        }
        AuthCmd::Status => {
            let stored = creds.get(KEYCHAIN_ACCOUNT)?.is_some();
            let mut status = AuthStatus::new(true, stored, pk_cli_auth::AuthMethod::Password);
            status.username = cfg.username.clone();
            status.credential_in_keychain = Some(stored);
            status.emit(cli.common.json);
            Ok(())
        }
        AuthCmd::Logout(args) => {
            if args.forget {
                creds.delete(KEYCHAIN_ACCOUNT)?;
                store.clear()?;
                if !cli.common.quiet {
                    eprintln!("API key removed from the keychain; config cleared");
                }
            } else if !cli.common.quiet {
                eprintln!("nothing session-scoped to clear (API keys are stateless); use --forget to remove the stored key");
            }
            Ok(())
        }
        AuthCmd::SetCredential(args) => {
            if creds.get(KEYCHAIN_ACCOUNT)?.is_some() && !args.overwrite {
                return Err(CliError::Usage(
                    "an API key is already stored; pass --overwrite to replace it".into(),
                ));
            }
            let secret = args.source.read(None)?;
            creds.set(KEYCHAIN_ACCOUNT, &secret)?;
            if !cli.common.quiet {
                eprintln!("API key stored");
            }
            Ok(())
        }
    }
}

fn looks_like_key(s: &str) -> bool {
    let rest = s
        .strip_prefix("lofty_live_")
        .or_else(|| s.strip_prefix("lofty_test_"));
    matches!(rest, Some(r) if r.len() >= 32 && r.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn config_cmd(cli: &Cli, cmd: &ConfigCmd, store: &ConfigStore) -> Result<(), CliError> {
    match cmd {
        ConfigCmd::Path => {
            println!("{}", store.path()?.display());
            Ok(())
        }
        ConfigCmd::Show => {
            let cfg: Config = store.load()?;
            let v = serde_json::to_value(&cfg).unwrap_or_default();
            if cli.common.json {
                output::json(&v);
            } else {
                output::render(&v);
            }
            Ok(())
        }
        ConfigCmd::Set { key, value } => {
            let mut cfg: Config = store.load()?;
            match key.as_str() {
                "base_url" => cfg.base_url = Some(value.clone()),
                "username" => cfg.username = Some(value.clone()),
                other => return Err(unknown_key(other)),
            }
            store.save(&cfg)
        }
        ConfigCmd::Unset { key } => {
            let mut cfg: Config = store.load()?;
            match key.as_str() {
                "base_url" => cfg.base_url = None,
                "username" => cfg.username = None,
                other => return Err(unknown_key(other)),
            }
            store.save(&cfg)
        }
    }
}

fn unknown_key(key: &str) -> CliError {
    CliError::Usage(format!(
        "unknown config key `{key}` (known: {})",
        config::KNOWN_KEYS.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_shape_validation() {
        assert!(looks_like_key(&format!("lofty_live_{}", "a".repeat(32))));
        assert!(looks_like_key(&format!("lofty_test_{}", "B2c".repeat(11))));
        assert!(!looks_like_key("lofty_live_short"));
        assert!(!looks_like_key(&format!("sk_live_{}", "a".repeat(32))));
        assert!(!looks_like_key(&format!("lofty_live_{}!", "a".repeat(31))));
    }
}
