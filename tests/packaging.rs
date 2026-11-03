//! The Claude Desktop extension manifest (`packaging/mcpb/manifest.json`)
//! against the binary it packages, so a release cannot ship a manifest that
//! names a tool or a version the binary does not have, or one that asks for
//! a credential. The release workflow validates the schema with the `mcpb`
//! CLI.

use std::process::Command;

use serde_json::{json, Value};

fn manifest() -> Value {
    let path = format!(
        "{}/packaging/mcpb/manifest.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn the_manifest_version_is_the_crate_version() {
    assert_eq!(manifest()["version"], env!("CARGO_PKG_VERSION"));
}

/// AGENTS.md names the spec's tag; a pin bump that forgets it leaves agents
/// reading the wrong spec.
#[test]
fn agents_md_names_the_pinned_cli_common_tag() {
    let root = env!("CARGO_MANIFEST_DIR");
    let cargo = std::fs::read_to_string(format!("{root}/Cargo.toml")).unwrap();
    let tag = cargo
        .lines()
        .find(|l| l.starts_with("pk-cli-core"))
        .and_then(|l| l.split("tag = \"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .expect("pk-cli-core is pinned to a tag");
    let agents = std::fs::read_to_string(format!("{root}/AGENTS.md")).unwrap();
    assert!(
        agents.contains(&format!("**{tag}**")),
        "AGENTS.md does not name {tag}"
    );
}

#[test]
fn the_manifest_lists_exactly_the_served_tools() {
    let listed: Vec<String> = manifest()["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    let out = Command::new(env!("CARGO_BIN_EXE_lofty"))
        .args(["mcp", "--list-tools"])
        .env("XDG_CONFIG_HOME", env!("CARGO_TARGET_TMPDIR"))
        .env("HOME", env!("CARGO_TARGET_TMPDIR"))
        .env("USERPROFILE", env!("CARGO_TARGET_TMPDIR"))
        .output()
        .unwrap();
    assert!(out.status.success());
    let served: Vec<String> = serde_json::from_slice::<Value>(&out.stdout).unwrap()["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(listed, served);
}

/// The API key can be pasted, so the extension takes it as a sensitive setting and passes
/// it as `$LOFTY_API_KEY` on every platform, and nothing else.
#[test]
fn every_platform_runs_mcp_with_the_key_from_a_sensitive_setting() {
    let m = manifest();
    let settings = m["user_config"].as_object().unwrap();
    assert_eq!(settings.keys().collect::<Vec<_>>(), ["api_key"]);
    assert_eq!(settings["api_key"]["sensitive"], true);
    assert_eq!(settings["api_key"]["required"], true);
    let says = settings["api_key"]["description"].as_str().unwrap();
    assert!(says.contains("read-only"), "{says}");

    let config = &m["server"]["mcp_config"];
    let mut configs = vec![config.clone()];
    for os in ["win32", "linux"] {
        configs.push(config["platform_overrides"][os].clone());
    }
    for c in &configs {
        assert_eq!(c["args"], json!(["mcp"]), "{c}");
        assert!(
            c["command"]
                .as_str()
                .unwrap()
                .starts_with("${__dirname}/server/"),
            "{c}"
        );
        assert_eq!(
            c["env"],
            json!({"LOFTY_API_KEY": "${user_config.api_key}"}),
            "{c}"
        );
    }
    let text = m["long_description"].as_str().unwrap();
    assert!(text.contains("goes to Claude"), "{text}");
    assert_eq!(m["license"], "MIT");
}
