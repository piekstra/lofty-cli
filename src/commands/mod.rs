//! Domain command modules. Each read emits the provider payload (plus a
//! `schema` tag) in `--json` mode and a shaped table/kv view in text mode.

pub mod account;
pub mod amm;
pub mod api;
pub mod catalog;
pub mod orders;
pub mod properties;
pub mod quote;
pub mod rewards;

use pk_cli_core::{output, CliError, CommonArgs};
use pk_cli_secrets::CredentialStore;
use serde_json::Value;

use crate::client::LoftyClient;
use crate::config::Config;

pub struct Ctx<'a> {
    pub common: &'a CommonArgs,
    pub cfg: &'a Config,
    pub creds: &'a CredentialStore,
}

impl Ctx<'_> {
    pub fn client(&self) -> Result<LoftyClient, CliError> {
        LoftyClient::new(self.cfg, self.creds)
    }
}

/// Emit a DTO: tagged payload in JSON mode, rendered view in text mode.
pub fn emit(ctx: &Ctx, schema: &str, payload: Value, text: impl FnOnce(&Value)) {
    if ctx.common.json {
        let mut tagged = serde_json::Map::new();
        tagged.insert("schema".into(), Value::String(format!("{schema}/v1")));
        match payload {
            Value::Object(m) => tagged.extend(m),
            other => {
                tagged.insert("data".into(), other);
            }
        }
        output::json(&Value::Object(tagged));
    } else {
        text(&payload);
    }
}

/// Confirmation gate for mutations (SPEC §1.3): prompt when interactive,
/// exit 6 when not (unless --force).
pub fn confirm(ctx: &Ctx, force: bool, summary: &str) -> Result<(), CliError> {
    if force {
        return Ok(());
    }
    if !ctx.common.interactive() {
        return Err(CliError::ConfirmationRequired(format!(
            "{summary} — pass --force to proceed non-interactively"
        )));
    }
    eprint!("{summary} — proceed? [y/N] ");
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| CliError::Other(format!("reading confirmation: {e}")))?;
    if line.trim().eq_ignore_ascii_case("y") || line.trim().eq_ignore_ascii_case("yes") {
        Ok(())
    } else {
        Err(CliError::ConfirmationRequired("aborted by user".into()))
    }
}

/// Pull selected columns out of an array of objects for table rendering.
pub fn table_view(items: &[Value], columns: &[&str]) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            let mut row = serde_json::Map::new();
            for col in columns {
                if let Some(v) = item.get(*col) {
                    row.insert((*col).to_string(), v.clone());
                }
            }
            Value::Object(row)
        })
        .collect()
}
