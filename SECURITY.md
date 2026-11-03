# Security

`lofty` holds an API key that, if you enabled Trading on it, can place orders and swaps on
your real account. Report anything sensitive to the repository owner directly rather than
in a public issue.

## Secrets

| Secret | Where it lives | Notes |
| --- | --- | --- |
| Lofty API key | OS keychain, service `piekstra.lofty`, item `api-key`. For the MCP tools only, `$LOFTY_API_KEY` comes first, and the Claude Desktop extension keeps its key in Claude Desktop's settings and passes it that way | Sent only to `base_url`, `https://api.lofty.ai` unless you changed it in your own config file |
| `--http` bearer token | OS keychain, service `piekstra.lofty`, item `mcp-http-token`, or `LOFTY_MCP_TOKEN`; **a copy in your Cloudflare Worker**, as its `HOME_LOFTY_TOKEN` secret | Only if you use `lofty mcp --http`. It admits a caller to the read-only tools. It signs in to nothing at Lofty |

Secrets never go on argv, in a log or in a file in this repository. Config is non-secret, and
comes from `~/.config/lofty`; nothing in a working directory is read.

## The MCP server

`lofty mcp` serves four read-only tools to an MCP client (Claude Code, Claude Desktop) over
stdio; the server opens no port unless run with `--http` (below). **What the tools return
goes to Claude on every path**, as part of your conversation: your balances, holdings, cost
basis, rent earned, limit orders and trades, and public figures about a property. No result
carries your Lofty user id or a chain transaction id.

No tool can place or cancel an order, swap or change anything at Lofty, even with a key that
allows trading, and `--allow-writes` adds nothing, since every tool is read-only. A tool
reaches Lofty only through a seam with `get` and nothing else. Every path is fixed text, and
a `property_id` goes into a path only after a check that it is 10 to 40 letters and digits, so
what a model passes cannot name another endpoint, even for a read. `src/mcp/tests.rs` holds
this to the requests the tools actually make. Create a read-only key for this all the same: a
read-only key cannot trade whatever runs it.

The server reads the config and the key on each call and holds neither between calls, so a new
key applies on the next call. A tool never prompts for a key or creates a keychain item: a
missing or rejected key in the keychain is an error asking the user to run `lofty auth login
--overwrite` in a terminal, and one from `$LOFTY_API_KEY`, which wins over the keychain, asks them
to correct it where it is set, such as the Claude Desktop extension's settings. On macOS a binary macOS has not granted raises a keychain prompt on
a call, and a server left unattended waits on it. The Claude Desktop extension does not read the
keychain for the key, since it is given one.

`lofty mcp --http --port <PORT>` serves the same tools over HTTP for the user's own Cloudflare
Worker, which reaches it through the user's own Cloudflare Tunnel (cli-common's `connector/`). It
binds 127.0.0.1 only, with no flag for another address, and refuses to start without a bearer
token of 32 or more characters, from `LOFTY_MCP_TOKEN` or the keychain item `mcp-http-token`
(`piekstra.lofty`), which `lofty mcp --new-token` makes from 256 random bits and replaces only
with `--overwrite`. Every request must carry the token (compared in constant time) and a `Host`
of `127.0.0.1` or `localhost`, and must not carry an `Origin`; there are no CORS headers. The
Lofty key stays on your computer: the Worker sends tool calls and receives results, and never
sees it.

**Tool results also cross Cloudflare on this path**, through your tunnel and your Worker, on
their way to Claude. Who holds what, and what a leaked token can do, is in the connector's
[threat model](https://github.com/piekstra/cli-common/blob/main/connector/README.md#threat-model).

`lofty auth logout --forget` removes the API key and `mcp-http-token`. A server already running
keeps the token it started with and goes on answering, without a key, until it is stopped; plain
`auth logout` clears nothing. To revoke the connector's access and keep serving, run `lofty mcp
--new-token --overwrite` and restart the server (`launchctl kickstart -k gui/$(id
-u)/com.piekstra.lofty-mcp-http`); the old token works until the restart. To stop serving, run
`launchctl bootout gui/$(id -u)/com.piekstra.lofty-mcp-http` and stop the tunnel.

## Repository data policy

This repository is public. `.githooks/privacy-scan` and the fixture policy in
[`tests/fixtures/README.md`](tests/fixtures/README.md) keep API keys, account ids, balances and
holdings out of it; account fixtures hold dummies only.
