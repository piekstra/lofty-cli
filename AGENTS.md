# AGENTS.md

Guidance for AI coding agents (and humans) working in this repo. Tool-agnostic;
`CLAUDE.md` points here.

## What this is

`lofty` — a Rust CLI for the [Lofty](https://www.lofty.ai) fractional
real-estate marketplace API. A thin, Lofty-specific layer over the shared
[`cli-common`](https://github.com/piekstra/cli-common) `pk-cli-*` crates (auth,
http, config, secrets, self-update). This repo owns only the Lofty commands and
their DTOs.

## Build, test, lint

```console
make verify     # fmt-check + clippy -D warnings + tests + smoke — the CI gate
make test       # unit + integration
make build      # debug build
cargo run -- rewards programs
```

Run `make verify` before considering a change done — it's exactly what CI runs.

## Layout

- `src/main.rs` — clap command tree, arg validation, exit-code mapping.
- `src/commands/*.rs` — one module per top-level command (properties, orders,
  account, rewards, amm, quote, api, catalog). Each renders a human table and a
  `--json` DTO.
  - `quote` holds the mutating quote primitives (mechanism only — the caller
    supplies target prices; the CLI never decides where to quote). They are a
    **dry run unless `--execute`**, and enforce shared rails before sending:
    never cross the market, never exceed cover, never go under `minContracts`,
    stay inside the reward band, and touch only the sides given a price, so one
    side can never be orphaned into a non-earning position.
- `src/client.rs` — HTTP against the SDK surface (`/public/v1`) and the internal
  website API (`--internal`, `/prod`).
- `src/config.rs` — non-secret config; the API key is keychain-only.
- `src/catalog.rs` — the observed endpoint inventory (a static harvest of the
  website bundle; never live-verified, and records no HTTP method).
- `tests/` — offline contract/shape tests + `tests/fixtures/` (see its README).
- `docs/api.md` — the two API surfaces (SDK vs internal) and the auth boundary
  between them, plus the traps: notably that a gated route and a nonexistent one
  return identical `403`s, so this API cannot be probed for route discovery and
  `catalog` entries cannot be confirmed that way.

## Conventions (do not break these)

- **`--json` on every command**, emitting one DTO tagged with a `schema` field
  (e.g. `"schema":"rewards-programs/v1"`). Human output → stdout as a table;
  diagnostics → stderr. Keep the human and JSON paths in sync. A breaking DTO
  change bumps the `/vN` suffix.
- **Exit codes:** 0 ok · 2 usage · 3 auth · 4 not found · 5 upstream · 6
  confirmation required. Validate args and confirm **before** touching the
  keychain or network (so `--help`/bad-args never prompt or hang).
- **Read-only by default.** Mutations (`orders create/cancel`, `amm swap`)
  prompt for confirmation and require `--force` to run non-interactively (exit 6
  otherwise). Writes carry an automatic `Idempotency-Key`.
- **Secrets** come from the OS keychain or stdin — never argv, never logs, never
  a file in the repo.

## Safety & privacy (this ships to a public repo and trades real money)

- Never commit an API key, real account id, balance, address, or any PII.
- `tests/fixtures/account/` are **sanitized** — dummy ids (`01SAMPLE…`) and
  amounts, never a real account capture. `public/` and internal market fixtures
  hold real *public* marketplace data only. Any new fixture must scrub account
  ids, balances, and amounts to dummies.
- Mutating commands move real money; treat any change to their confirmation /
  `--force` handling as safety-critical.

## Definition of done

`make verify` green, tests cover the change, `--json` and human output both
updated, no secrets/PII in the diff, and mutations still confirm + honor
`--force`.
