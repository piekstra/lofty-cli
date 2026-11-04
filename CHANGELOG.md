# Changelog

All notable changes to `lofty-cli` are documented here, from 0.2.0 on. Newest first. The
project follows [SemVer](https://semver.org/); until 1.0, minors carry the breaking-change
signal.

## 0.2.3 — 2026-11-04

- `lofty_trades`: a row with no `tradeId` is an upstream error. It used to fall to the count within its day.
- `auth logout --forget` says what it does not end: a running `lofty mcp --http` keeps the token it started with, and one on `$LOFTY_MCP_TOKEN` never had an item to remove.
- cli-common v0.29.0, up from v0.27.0.

## 0.2.2 — 2026-11-03

- `lofty_orders` and `lofty_trades` page by position, so a row that arrives between pages repeats
  nothing and skips nothing, past a `lofty_trades` answer with `complete: false`, whose latest-200
  window can lose its oldest row. Rows of the same day now run in a fixed order of their own, not by
  time of day. A cursor from an earlier version still works, as the count of rows it holds.
- The tools read what a failure means from the error's reason, not its wording.
- cli-common v0.27.0, up from v0.24.0.

## 0.2.1 — 2026-11-03

- `lofty api GET http://...` is refused with exit 2 before the keychain or the network,
  and so is a `base_url` off `https`. Either one used to send the API key in the clear.
  Plain `http` to this machine still works.
- A redirect that leaves `https` is refused, and the error says why. Built on cli-common
  v0.24.0.

## 0.2.0 — 2026-11-03

- `lofty mcp`: read-only tools for Claude, `lofty_account`, `lofty_orders`, `lofty_trades`
  and `lofty_property`. They run over stdio for Claude Code and Claude Desktop, and with
  `--http` on 127.0.0.1 behind a bearer token for Claude on the web and mobile through your
  own Cloudflare Worker and tunnel. No tool can place or cancel an order or swap, whatever
  the key allows, and a test holds every request a tool makes to a list of reads.
- `$LOFTY_API_KEY`, for these tools only, comes before the keychain. The Claude Desktop
  extension passes its key that way.
- Releases are cut by pushing a tag, no longer by merging a version bump. They attach
  `lofty-<version>.mcpb`, a Claude Desktop extension, and a Windows x86_64 tarball, and a
  release is published only once every asset is there.
- `self-update` says to restart a running `lofty mcp --http` and to install the new
  `.mcpb`.
- A failure that got no answer from Lofty no longer prints its URL.
- cli-common v0.22.0, up from v0.15.1. The minimum Rust version is 1.89.
