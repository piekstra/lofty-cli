//! `lofty catalog` — the observed internal platform endpoint inventory
//! (124 endpoints captured from the marketplace SPA). Useful for exploring
//! what exists beyond the official SDK surface and for `api --internal`.

use pk_cli_core::{output, CliError};
use serde_json::{json, Value};

use super::{emit, Ctx};
use crate::catalog::{Kind, ENDPOINTS};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Filter by group (e.g. exchange, properties, lp-rewards).
    #[arg(long)]
    pub group: Option<String>,

    /// Only reads / only writes.
    #[arg(long, value_parser = ["read", "write", "admin"])]
    pub kind: Option<String>,
}

pub fn run(ctx: &Ctx, args: &Args) -> Result<(), CliError> {
    let rows: Vec<Value> = ENDPOINTS
        .iter()
        .filter(|e| args.group.as_deref().is_none_or(|g| e.group == g))
        .filter(|e| {
            args.kind.as_deref().is_none_or(|k| match e.kind {
                Kind::Read => k == "read",
                Kind::Write => k == "write",
                Kind::Admin => k == "admin",
            })
        })
        .map(|e| json!({ "path": e.path, "name": e.name, "group": e.group, "kind": e.kind }))
        .collect();
    emit(ctx, "catalog", json!({ "endpoints": rows.clone() }), |_| {
        output::table(&rows);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_covers_the_captured_surface() {
        assert_eq!(ENDPOINTS.len(), 124);
        // The market-making core must be present.
        for path in [
            "/exchange/v2/getpropertyorderbook",
            "/exchange/v2/createorder",
            "/exchange/v2/cancelorder",
            "/lp-rewards/dashboard",
            "/properties/v2/marketplace",
        ] {
            assert!(ENDPOINTS.iter().any(|e| e.path == path), "missing {path}");
        }
        // Anything that spends or mutates must be marked Write (or Admin).
        for e in ENDPOINTS {
            if e.path.contains("create") || e.path.contains("cancel") {
                assert_ne!(e.kind, Kind::Read, "{} should not be Read", e.path);
            }
        }
    }
}
