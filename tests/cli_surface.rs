//! Offline surface tests: flags, exit codes, and the JSON output contract.
//! No network; nothing here touches the live API.

use assert_cmd::Command;
use predicates::prelude::*;

fn lofty() -> Command {
    Command::cargo_bin("lofty").unwrap()
}

#[test]
fn help_lists_the_standard_surface() {
    lofty()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("auth"))
        .stdout(predicate::str::contains("properties"))
        .stdout(predicate::str::contains("orders"))
        .stdout(predicate::str::contains("rewards"))
        .stdout(predicate::str::contains("amm"))
        .stdout(predicate::str::contains("self-update"))
        .stdout(predicate::str::contains("completions"));
}

#[test]
fn usage_error_exits_2_with_json_error_dto() {
    let out = lofty()
        .args(["--json", "config", "set", "bogus_key", "x"])
        .assert()
        .code(2);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is a JSON error DTO");
    assert_eq!(v["error"]["code"], "usage");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown config key"));
}

#[test]
fn info_emits_cli_info_v1() {
    let out = lofty().arg("info").assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["schema"], "cli-info/v1");
    assert_eq!(v["name"], "lofty");
    assert_eq!(v["spec"], "piekstra-cli/1");
    let caps: Vec<&str> = v["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    for cap in ["properties", "orders", "account", "rewards", "amm", "api"] {
        assert!(caps.contains(&cap), "missing capability {cap}");
    }
}

#[test]
fn catalog_json_lists_the_internal_surface() {
    let out = lofty()
        .args(["--json", "catalog", "--group", "lp-rewards"])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["schema"], "catalog/v1");
    let eps = v["endpoints"].as_array().unwrap();
    assert_eq!(eps.len(), 3);
    assert!(eps
        .iter()
        .all(|e| e["path"].as_str().unwrap().starts_with("/lp-rewards/")));
}

#[test]
fn order_create_validates_before_any_network_or_confirmation() {
    // Bad direction is a usage error (exit 2) regardless of auth state.
    lofty()
        .args([
            "orders",
            "create",
            "--property-id",
            "X",
            "--direction",
            "hold",
            "--price",
            "1.0",
            "--quantity",
            "1",
        ])
        .assert()
        .code(2);
    // Sub-cent price rejected.
    lofty()
        .args([
            "orders",
            "create",
            "--property-id",
            "X",
            "--direction",
            "buy",
            "--price",
            "0.001",
            "--quantity",
            "1",
        ])
        .assert()
        .code(2);
}

#[test]
fn mutations_refuse_to_run_unconfirmed_when_non_interactive() {
    // Confirmation is checked before auth/network, so a valid-but-unconfirmed
    // mutation in non-interactive mode is deterministically exit 6 — it never
    // reaches the keychain or the API, and no order is placed.
    let out = lofty()
        .args([
            "--json",
            "orders",
            "create",
            "--property-id",
            "X",
            "--direction",
            "buy",
            "--price",
            "1.00",
            "--quantity",
            "1",
        ])
        .assert()
        .code(6);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["error"]["code"], "confirmation_required");
}

#[test]
fn amm_swap_requires_slippage_bounds() {
    lofty()
        .args([
            "amm",
            "swap",
            "--pool-id",
            "1",
            "--side",
            "buy",
            "--tokens",
            "1",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--max-usdc"));
    lofty()
        .args([
            "amm",
            "swap",
            "--pool-id",
            "1",
            "--side",
            "sell",
            "--tokens",
            "1",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--min-usdc"));
}

#[test]
fn amm_quote_wants_exactly_one_amount() {
    lofty()
        .args(["amm", "quote", "--pool-id", "1", "--side", "buy"])
        .assert()
        .code(2);
    lofty()
        .args([
            "amm",
            "quote",
            "--pool-id",
            "1",
            "--side",
            "buy",
            "--tokens",
            "1",
            "--usdc",
            "50",
        ])
        .assert()
        .code(2);
}

#[test]
fn completions_render_for_zsh() {
    lofty()
        .args(["completions", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("#compdef lofty"));
}

#[test]
fn quote_recenter_bad_args_fail_before_the_keychain_is_touched() {
    // AGENTS.md: validate args and confirm BEFORE touching the keychain or
    // network, so bad args never prompt or hang. A recenter naming neither side
    // must be a usage error (exit 2) — not an auth error, and not a keychain
    // prompt on a machine with no key stored.
    for args in [
        vec![
            "--json",
            "quote",
            "recenter",
            "--property-id",
            "01SAMPLEPROP00000000000001",
        ],
        vec![
            "--json",
            "quote",
            "recenter",
            "--property-id",
            "01SAMPLEPROP00000000000001",
            "--bid",
            "0",
        ],
    ] {
        let out = lofty().args(&args).assert().code(2);
        let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&stdout).expect("stdout is a JSON error DTO");
        assert_eq!(v["error"]["code"], "usage", "args {args:?} → {stdout}");
    }
}

#[test]
fn help_lists_the_quote_primitives() {
    lofty()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("quote"));
    lofty()
        .args(["quote", "recenter", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--execute"))
        .stdout(predicate::str::contains("--min-ask"))
        .stdout(predicate::str::contains("--allow-out-of-band"));
}
