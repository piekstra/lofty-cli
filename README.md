# lofty — Lofty marketplace CLI

[![CI](https://github.com/piekstra/lofty-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/piekstra/lofty-cli/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/piekstra/lofty-cli?sort=semver)](https://github.com/piekstra/lofty-cli/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A fast, keychain-secured, agent-friendly command line for the
[Lofty](https://www.lofty.ai) fractional real-estate marketplace: market data,
order books, LP-reward (market-making) programs, and — with a trading-enabled
API key — limit orders and AMM swaps.

Built on Lofty's official SDK wire contract (`@loftyaicode/sdk`):
`https://api.lofty.ai/public/v1/*` with `Authorization: Bearer lofty_live_…`.
Every command has a `--json` mode emitting a `schema`-tagged DTO, so it composes
cleanly in scripts and agents.

> **Not affiliated with or endorsed by Lofty.** An independent, unofficial
> client. Trading commands move real money on your real account — read
> [Safety](#safety).

## Install

```console
# from source (Rust 1.82+)
$ cargo install --git https://github.com/piekstra/lofty-cli --locked

# or clone + build
$ git clone https://github.com/piekstra/lofty-cli && cd lofty-cli
$ make install        # cargo install --path . --force
```

Prebuilt binaries for macOS and Linux are attached to each
[release](https://github.com/piekstra/lofty-cli/releases); once installed,
`lofty self-update` upgrades in place.

## Quick start

```console
$ lofty auth login            # paste your API key (Lofty → Account → Settings → API Keys)
$ lofty auth status --json
$ lofty rewards programs      # who pays for liquidity, and the rules
```

The key is verified against `/account/balance` before it's stored (skip with
`--no-verify`) and lives only in the OS keychain (service `piekstra.lofty`) —
never on disk, never in argv, never in logs. Enable **Trading** on the key in
your Lofty dashboard to place/cancel orders and swap.

## Commands

```console
$ lofty properties list                 # marketplace listings
$ lofty properties orderbook <ID>       # live bids/asks
$ lofty properties trades <ID>          # recent fills + best bid/ask
$ lofty rewards programs                # LP-reward programs and their terms
$ lofty rewards history --since <ms>    # your reward payouts
$ lofty account balance|positions|trades
$ lofty orders list|get|create|cancel   # mutations confirm, or --force
$ lofty amm pools|quote|swap            # swap needs --max-usdc / --min-usdc
$ lofty api GET /public/v1/amm/pools    # raw passthrough (Bearer attached)
$ lofty api --internal GET /properties/v2/marketplace   # website API (open reads)
$ lofty catalog --group exchange        # observed endpoint inventory
```

### Example — LP-reward programs

```console
$ lofty rewards programs
ALLOWEDSPREAD | DAILYREWARDS | MINCONTRACTS | MINTWOSIDEDLIQUIDITY | PROPERTYID                 | SLUG
2             | 10           | 4            | 1                    | 01FMN4Q2M9MW33ERXJKG61MTRD | 2094-W-34th-Place_Cleveland-OH
2             | 10           | 2            | 1                    | 01JGMSZ4NEGCFF8T8CEE5XXYQ7 | 2221-E-Chase-St_Baltimore-MD
```

```console
$ lofty --json rewards programs | jq '.programs[0]'
{
  "address": { "line1": "2094 W 34th Place", "line2": "Cleveland, OH 44113" },
  "allowedSpread": 2,
  "blockDurationMs": 3600000,
  "blocksPerDay": 24,
  "dailyRewards": 10,
  "minContracts": 4,
  "minOrderAgeMs": 3000,
  "minTwoSidedLiquidity": 1
}
```

## JSON, exit codes, limits

- **`--json`** on any command prints one machine-readable DTO to stdout, tagged
  with a `schema` field (e.g. `"schema":"rewards-programs/v1"`); diagnostics go
  to stderr, never mixed in.
- **Exit codes:** `0` ok · `2` usage · `3` auth · `4` not found · `5` upstream ·
  `6` confirmation required (non-interactive without `--force`).
- **Rate limits** (per key): 300 reads/min, 30 writes/min; writes also capped at
  60/min per account across keys.

## Safety

Placing orders, cancelling, and swapping are **real trades on your real
account.** They prompt for confirmation; in scripts and agents pass `--force`
explicitly to proceed non-interactively (otherwise they exit `6`). Writes carry
an automatic `Idempotency-Key`, so retrying a failed create won't double-submit.
Read-only by default: nothing mutates without an explicit mutating subcommand.

## Development

```console
$ make verify     # fmt-check + clippy (-D warnings) + tests + smoke
$ make test       # unit + integration
$ cargo run -- rewards programs
```

Tests are fully **offline**: raw captured API responses live in
`tests/fixtures/` as independent files (see its README) and the contract tests
load them from disk — no network, no credentials. See
[CONTRIBUTING.md](CONTRIBUTING.md) and [AGENTS.md](AGENTS.md).

## License

[MIT](LICENSE) © piekstra. Contributions welcome under the same terms — see
[CONTRIBUTING.md](CONTRIBUTING.md).
