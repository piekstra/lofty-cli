# Changelog

All notable changes to `lofty-cli` are documented here, from 0.2.0 on. Newest first. The
project follows [SemVer](https://semver.org/); until 1.0, minors carry the breaking-change
signal.

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
