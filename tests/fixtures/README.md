# Test fixtures — raw Lofty API responses

Captured live from `https://api.lofty.ai/public/v1` (SDK surface, Bearer API
key) on 2026-07-14, kept as independent files so the raw shapes are easy to
inspect. Tests load these files; they are never embedded as string literals.

- `public/` — real market data, unmodified (large lists trimmed to 2–3 items).
- `account/` — real response **structure**, but personal values (ids, balances,
  amounts) replaced with representative dummies per repo policy.
- The old-fixtures at the top level (`marketplace.json`, `orderbook.json`,
  `property-info.json`) are from the internal `/prod` API, kept for the
  `api --internal` passthrough shapes.

Upstream shape notes (re-verified against SDK 0.2.3, 2026-07-15):
- `orderbook` now returns `orderbook.{bids,asks}` (each `{price, quantity}`),
  matching the SDK README — the older nested `orderbook.orderBook.buyOrders`
  envelope is gone. Our parsers accept both.
- `properties` list was trimmed from 89 → 53 fields; internal-only admin fields
  (`hideMkt`, `hide_details`, `reserveOwnerId`, `dao_app_id`, …) were removed.
  The combined `address` string is gone; use `address_line1`/`city`/`state`.
- `GET /public/v1/account/lp-positions` now returns HTTP 200 (was 500).
- Still open: `GET /public/v1/properties/` (trailing slash) returns a list
  instead of 404; no public OpenAPI spec yet.
