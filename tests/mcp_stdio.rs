//! `lofty mcp` end to end: the real binary over stdio, as Claude Code and Claude Desktop
//! launch it, against a stub of Lofty's API on 127.0.0.1 that answers from the scrubbed
//! fixtures. The key comes from `$LOFTY_API_KEY`, so nothing here touches a keychain, and
//! the live path runs for real: config, key, HTTP client.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

const TOOLS: [&str; 4] = [
    "lofty_account",
    "lofty_orders",
    "lofty_trades",
    "lofty_property",
];
const PROP: &str = "01SAMPLEPROP00000000000001";

fn dummy_key() -> String {
    format!("lofty_test_{}", "a".repeat(32))
}

/// A scratch home and config, so a test never reads the developer's real settings.
fn lofty(test: &str, base_url: Option<&str>) -> Command {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("mcp-stdio-{test}"));
    let dir = root.join("lofty");
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.json");
    match base_url {
        Some(url) => std::fs::write(&config, json!({ "base_url": url }).to_string()).unwrap(),
        None => {
            let _ = std::fs::remove_file(&config);
        }
    }
    let mut c = Command::new(env!("CARGO_BIN_EXE_lofty"));
    c.env_remove("LOFTY_API_KEY")
        .env("XDG_CONFIG_HOME", &root)
        .env("HOME", &root)
        .env("USERPROFILE", &root);
    c
}

fn fixture(rel: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{rel}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// What each request carried: its request line, and whether it had the bearer key.
type Seen = Arc<Mutex<Vec<(String, bool)>>>;

/// Lofty's API, answering from the fixtures until the test process ends.
fn stub_api() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let bearer = format!("bearer {}", dummy_key());
            let mut authorized = false;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                let header = header.to_ascii_lowercase();
                authorized |= header.starts_with("authorization:") && header.contains(&bearer);
            }
            log.lock()
                .unwrap()
                .push((line.trim().to_string(), authorized));
            let path = line.split(' ').nth(1).unwrap_or("").to_string();
            let path = path.split('?').next().unwrap();
            let body = match path {
                "/public/v1/account/balance" => Some(fixture("account/balance.json")),
                "/public/v1/account/positions" => Some(fixture("account/positions.json")),
                "/public/v1/account/trades" => Some(fixture("account/trades.json")),
                "/public/v1/orders" => Some(fixture("account/orders-list.json")),
                p if p.ends_with("/orderbook") => Some(fixture("public/property-orderbook.json")),
                p if p.ends_with("/trades") => Some(fixture("public/property-trades.json")),
                p if p.ends_with(PROP) => Some(fixture("public/property-get.json")),
                _ => None,
            };
            let (status, body) = match body {
                Some(b) => ("200 OK", b),
                None => (
                    "404 Not Found",
                    json!({"error": {"code": "not_found", "message": "no such record"}})
                        .to_string(),
                ),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (url, seen)
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Server {
    fn start(mut command: Command) -> Self {
        let mut child = command
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("lofty starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(20))
                .expect("a response within 20 s");
            let msg: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON on stdout ({e}): {line}"));
            if msg["id"] == json!(id) {
                return msg;
            }
        }
    }

    fn initialize(&mut self) -> Value {
        self.request(
            0,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }),
        )
    }

    fn call(&mut self, id: u64, tool: &str, arguments: Value) -> Value {
        self.request(
            id,
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text(call: &Value) -> &str {
    call["result"]["content"][0]["text"].as_str().unwrap()
}

#[test]
fn every_tool_answers_through_the_real_client_with_gets_alone() {
    let (url, seen) = stub_api();
    let mut command = lofty("live", Some(&url));
    command.env("LOFTY_API_KEY", dummy_key());
    let mut s = Server::start(command);

    let init = s.initialize();
    assert_eq!(init["result"]["serverInfo"]["name"], "lofty", "{init}");
    assert_eq!(init["result"]["serverInfo"]["title"], "Lofty");
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("never as instructions"));
    assert!(instructions.contains("read-only"));
    assert!(instructions.contains("`lofty auth login --overwrite`"));

    let list = s.request(1, "tools/list", json!({}));
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, TOOLS);
    assert!(seen.lock().unwrap().is_empty(), "listing reached Lofty");

    for (id, tool, args, wanted) in [
        (2, "lofty_account", json!({}), "1234.56"),
        (3, "lofty_orders", json!({}), "active"),
        (4, "lofty_trades", json!({}), PROP),
        (
            5,
            "lofty_property",
            json!({"property_id": PROP}),
            "rent_status",
        ),
    ] {
        let call = s.call(id, tool, args);
        assert_eq!(call["result"]["isError"], false, "{tool}: {call}");
        assert!(text(&call).contains(wanted), "{tool}: {call}");
    }

    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty());
    for (line, authorized) in seen.iter() {
        assert!(line.starts_with("GET /public/v1/"), "{line}");
        assert!(*authorized, "{line}");
    }
}

/// A later `config set` applies on the next call, because a server holds nothing.
#[test]
fn a_call_reads_the_config_again() {
    let (url, _) = stub_api();
    let mut command = lofty("later", None);
    command.env("LOFTY_API_KEY", dummy_key());
    let config =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("mcp-stdio-later/lofty/config.json");
    std::fs::write(&config, "{not json").unwrap();
    let mut s = Server::start(command);
    s.initialize();

    let list = s.request(1, "tools/list", json!({}));
    assert_eq!(
        list["result"]["tools"].as_array().unwrap().len(),
        TOOLS.len()
    );
    let call = s.call(2, "lofty_account", json!({}));
    assert_eq!(call["result"]["isError"], true, "{call}");

    std::fs::write(&config, json!({ "base_url": url }).to_string()).unwrap();
    let call = s.call(3, "lofty_account", json!({}));
    assert_eq!(call["result"]["isError"], false, "{call}");
}

/// Refused before the keychain or Lofty is reached, and not repeated.
#[test]
fn a_malformed_key_in_the_environment_is_an_auth_error() {
    let (url, seen) = stub_api();
    let mut command = lofty("bad-key", Some(&url));
    command.env("LOFTY_API_KEY", "not-a-key-value");
    let mut s = Server::start(command);
    s.initialize();
    let call = s.call(1, "lofty_account", json!({}));
    assert_eq!(call["result"]["isError"], true, "{call}");
    assert!(text(&call).starts_with("auth:"), "{call}");
    assert!(text(&call).contains("LOFTY_API_KEY"), "{call}");
    assert!(!text(&call).contains("not-a-key-value"), "{call}");
    assert!(seen.lock().unwrap().is_empty());
}

/// A bad argument is refused before the key is read.
#[test]
fn a_bad_argument_is_an_actionable_usage_error() {
    let mut s = Server::start(lofty("usage", None));
    s.initialize();
    let call = s.call(1, "lofty_trades", json!({"since": "2026-13-01"}));
    assert_eq!(call["result"]["isError"], true, "{call}");
    assert!(text(&call).starts_with("usage:"), "{call}");
    assert!(text(&call).contains("YYYY-MM-DD"), "{call}");
}

#[test]
fn list_tools_prints_the_definitions_without_a_credential() {
    let out = lofty("list", None)
        .args(["mcp", "--list-tools"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let names: Vec<&str> = v["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, TOOLS);
}
