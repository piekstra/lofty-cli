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
# from source (recent stable Rust; MSRV 1.88)
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
$ lofty rewards reconcile               # audit payouts: totals, math check, gap detection
$ lofty rewards eligibility             # are your orders earning right now, and if not why
$ lofty account balance|positions|trades
$ lofty account coverage                # what's reserved backing your bids vs free to spend
$ lofty account breakeven --margin 5    # fee-inclusive sell price (basis hides the buy fee)
$ lofty orders list|get|create|cancel   # mutations confirm, or --force
$ lofty quote recenter --property-id <ID> --bid <P>   # move a quote safely (dry run by default)
$ lofty quote provision --property-id <ID> --bid <P> --ask <Q>   # post a fresh two-sided quote
$ lofty quote pull --property-id <ID> --keep-above <P>           # stand down, keeping recovery asks
$ lofty amm pools|quote|swap            # swap needs a slippage bound (see Safety)
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

### Example — reconcile your payouts

`reconcile` pulls your full payout history and checks it: per-property totals,
an internal-consistency check (`MISMATCH` counts payouts where the amount ≠
`percentOfPool × pool-per-block` — a nonzero value is worth reporting to Lofty),
and `GAPS` (payout periods that got nothing mid-streak — candidate misses to
investigate; some are legitimate ineligibility). Numbers below are illustrative.

```console
$ lofty rewards reconcile
AVG%POOL | EARNED  | GAPS | MISMATCH | PAYOUTS | PROPERTY
20.0     | $3.5000 | 1    | 0        | 48      | Sample City, ST 00000
total: $3.5000 across 48 payout(s); 1 gap period(s)
  ⚠ Sample City, ST 00000: 1 period(s) with no payout — verify eligibility: 2026-01-02 13:00 UTC
```

### Example — am I actually earning?

LP rewards have several qualification rules that must **all** hold at once, and
missing any one silently pays you nothing. The costliest is that liquidity must be
**two-sided** — a bid alone earns *nothing*, no matter how large or how close to
mid it rests.

`eligibility` checks every open order against the published rules (two-sided,
at least `minContracts`, within `allowedSpread` of the book mid, covered by your
balances), projects your pool share from the published score formula, and names
the reason whenever something earns nothing. Numbers below are illustrative.

```console
$ lofty rewards eligibility
Sample City, ST 00000: EARNING — 27.4% of pool ≈ $2.74/day (mid $61.63, competing score 5.0)
Other City, ST 11111: $0 — not two-sided: no qualifying ASK (one-sided liquidity earns nothing)
    sell $13.60 x1 — out of band, below minContracts
projected total: $2.74/day across 2 properties
```

Qualifying a *side* and *scoring* are separate: an order that is sized and covered
establishes that side exists, while only in-band orders accrue score — so a
position can be two-sided yet still score nothing, and the output distinguishes
the two rather than collapsing them into one boolean.

### Example — what can I actually spend?

Lofty funds open orders from your live balances: buys need the USDC and sells
need the shares, *for as long as the order is open*, **across all your open
orders on a property combined** — and orders your wallet can't cover are
canceled automatically. The check is **per property, not aggregate**, so several
properties' bids can lean on the same USDC at once. Two consequences your raw
balance won't tell you:

- The wallet floor every bid must clear is your **largest single-property bid
  total** — that much is *reserved*, and only the surplus above it is spendable.
- The **sum** of your bids can legitimately exceed your wallet while they rest
  (`overCommitted`) — but one fill spends cash the other properties were also
  counting on, which can cascade into automatic cancellations.

`coverage` computes both, checks every order against its cover, and with
`--spend` simulates a purchase to show what it would uncover. Numbers below are
illustrative.

```console
$ lofty account coverage
ASK COVERED | ASK QTY | BID $  | BID COVERED | HELD | PROPERTY
true        | 4       | 190.00 | true        | 4    | 01SAMPLEPROPERTY000000000A
true        | 8       | 120.00 | true        | 8    | 01SAMPLEPROPERTY000000000B
wallet $200.00 — $190.00 reserved (largest single-property bid), $10.00 free to spend
  note: $310.00 total bid exposure exceeds the wallet — fine while they rest
  (coverage is per property), but one fill spends cash the others also rely on

$ lofty account coverage --spend 50
...
spending $50.00 leaves $150.00 — EXCEEDS free capital
  ⚠ would uncover 01SAMPLEPROPERTY000000000A (bids $190.00)
```

### Example — what price actually breaks even?

A position's `costBasis` is what you paid **for the tokens**. The platform buy fee
was charged *on top of* it, so it is **not** your true cost — and two fees bracket
a round trip, both paid out of the sale:

```
trueCost = costBasis × (1 + buyFee)
ask      = trueCost × (1 + margin) / (1 − sellFee)
```

Selling at your displayed basis therefore books a loss twice over. `breakeven`
does the arithmetic with **live fee rates** — read from the property
(`mtSellFeePct`) and its AMM pool, so it reflects the venue you actually trade on.
AMM rates are all-in: a swap pays the platform fee **plus** the pool's LP fee,
which the pool reports as separate lines (e.g. buy = `platformBuy` 3% + `lp` 2% =
5%). The order book has no LP fee.

The buy fee you paid depends on where you bought, which the API doesn't record —
so both venues are priced and labelled rather than guessing. Pass `--buy-fee` if
you know it. Numbers below are illustrative.

```console
$ lofty account breakeven --margin 5
01SAMPLEPROPERTY000000000A: cost basis $63.68/token (excludes the buy fee) — selling on book at 3.50% fee
ACQUIRED VIA | ASK +5.0% | BREAK-EVEN ASK | BUY FEE % | TRUE COST
amm          | $72.75    | $69.29         | 5.00      | $66.86
book         | $71.37    | $67.97         | 3.00      | $65.59
```

So a $63.68 basis needs **$69.29** just to break even, and **$72.75** to net 5%.
`--sell-venue amm` prices an exit into the pool instead, `--cost` prices a
hypothetical position, and `--sell-fee` / `--buy-fee` override the published rates.

### Example — moving a quote without shooting yourself in the foot

`quote recenter` is a **mechanism**, not a strategy: you supply the target prices
and it performs the move safely. It has no opinion about *where* to quote.

It is a **dry run unless you pass `--execute`** — the dangerous thing needs a
flag, the safe thing happens when you forget one.

```console
$ lofty quote recenter --property-id <ID> --bid 61.50
DRY RUN — nothing sent. mid $62.98, reward band [$60.98, $64.97]
  cancel buy $62.25 x3
  create buy $61.50 x3  (1.48 from mid)
  keep   sell $72.80 x8 (untouched)
re-run with --execute to apply
```

Sides you don't give a price for are **left alone**, so re-centering a bid can
never orphan an earning ask into a non-earning one-sided position. Before sending
anything it refuses to:

| Rail | Why |
|------|-----|
| cross the market | a bid at/above the best ask fills instantly as a **taker** — this command only rests liquidity |
| exceed your cover | Lofty **auto-cancels** uncovered orders (counting the side left in place) |
| go below `minContracts` | the order would rest but earn nothing |
| leave the reward band | same — earns nothing (override with `--allow-out-of-band`) |
| undercut `--min-ask` | guards a break-even/cost floor against an accidental below-cost sell |

Cancel always precedes the matching create: posting first would double-commit
capital and can breach per-property coverage, which auto-cancels **both** orders.
The trade-off is a brief one-sided window (the API has no atomic modify) — if the
re-post fails, the command says so loudly, because that window earns nothing.

`provision` posts a fresh two-sided quote and **refuses to leave you one-sided** —
if either leg fails a rail (usually too few tokens to back the ask), nothing is
sent and the shortfall is reported, because the half that *would* rest costs cover
and earns nothing. It never buys inventory for you; that stays an explicit
`amm swap`.

`pull` stands down from a property. `--keep-above` protects orders you mean to
leave resting — typically a cost-basis recovery ask parked above the band — so
standing down never sweeps away the sell that's waiting to recover your position:

```console
$ lofty quote pull --property-id <ID> --keep-above 13.00
DRY RUN — nothing sent.
  keep   sell $13.60 x1 (untouched)
re-run with --execute to apply
```

All three share one implementation of the rails (`check_placement`), so they
cannot drift apart.

## JSON, exit codes, limits

- **`--json`** on any command prints one machine-readable DTO to stdout, tagged
  with a `schema` field (e.g. `"schema":"rewards-programs/v1"`); diagnostics go
  to stderr, never mixed in.
- **Exit codes:** `0` ok · `2` usage · `3` auth · `4` not found · `5` upstream ·
  `6` confirmation required (non-interactive without `--force`).
- **Rate limits** (per key): 300 reads/min, 30 writes/min; writes also capped at
  60/min per account across keys.

## Safety

**Limit orders are priced against the market before they are sent.** `orders
create` reads BOTH the order book and the trades feed — they disagree, and
trusting the wrong one is how a sell got priced 20% under the market on an
illiquid book and filled instantly:

```console
$ lofty orders create --property-id <ID> --direction sell --price 57.12 --quantity 1
error: sell at $57.12 is AT OR BELOW the best bid $71.09 — it will fill
immediately as a taker, not rest on the book
  orderbook   bid $50.00  ask —
  trades feed bid $71.09  ask $71.43
  last trade  $67.50   (42 recent print(s))

If you understand and accept that this may lose money against the listed price,
re-run with --accept-below-market.
```

It refuses an order that would cross, that sits more than 5% worse than the best
reference (highest bid / lowest ask / last print), **or that cannot be priced at
all because the property has no trade history** — an empty tape is a reason to
stop, not to shrug. `--accept-below-market` is the explicit acknowledgement.

**Swap slippage bounds are checked, not trusted.** Every `amm swap` prices a
fresh quote first and compares your bound against it, reporting the tolerance it
implies:

```console
$ lofty amm swap --pool-id <ID> --side buy --tokens 1 --max-usdc 56.00
error: bound $56.00 is 9.2% from the live quote $51.27 — that authorises far more
slippage than you likely intend. Use --max-slippage-pct for an exact bound, or
--allow-high-slippage to proceed.
```

A bound is **refused above 5%** from the quote — pinned to the platform's own fee
load (3% platform + 2% pool LP), not to taste: a tolerance wider than the entire
fee schedule is far likelier a typo or a rounded guess than an intention. Prefer
`--max-slippage-pct 1`, which derives the bound from the quote instead of asking
you to pick a number. The check applies **under `--force` too**, where no prompt
is shown and a loose bound would otherwise pass unseen.

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
