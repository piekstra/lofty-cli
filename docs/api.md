# API surfaces, auth boundaries, and traps

What `lofty` talks to, what it deliberately cannot reach, and the ways this API
misleads you. Written down because two of these cost real debugging time and
are invisible from the outside.

## The two surfaces

### SDK surface — `https://api.lofty.ai/public/v1`

Lofty's official, documented wire contract (the `@loftyaicode/sdk` package).
Auth is `Authorization: Bearer lofty_live_…`, a key minted in the dashboard and
stored only in the OS keychain. Every domain command in this CLI targets this
surface, and `lofty api` is its raw passthrough.

The key's permission model has exactly **one** axis: read-only versus
read-and-trade. A trading-disabled key refuses mutations and nothing else —
there is no per-resource scope system, so there is no "documents" or "tax"
permission to switch on. If a route is missing from this surface, no key
setting will reveal it.

### Internal platform surface — `https://api.lofty.ai/prod`

The API the lofty.ai website's own front end calls. It authenticates with a
**Cognito idToken tied to a browser login session** (see also SigV4 paths); a
handful of its reads are open to the world.

`lofty api --internal` attaches **no credential at all**, by design. That is
why its help text says only publicly open endpoints answer — for example
`GET /prod/properties/v2/marketplace` returns data anonymously. Everything
account-scoped does not.

**The SDK API key does not authenticate this surface.** Routing an internal
path through the authenticated client (by passing the full URL, so the Bearer
header is attached) still fails. The two credential systems are unrelated: one
is an API key for the public SDK, the other is a website session.

## Trap: a `403` here tells you nothing

Verified live on 2026-08-11:

| Request (unauthenticated) | Result |
| --- | --- |
| `GET /prod/properties/v2/marketplace` | `200` — an open read |
| `GET /prod/taxdocuments/v2/all` | `403 {"message":"Forbidden"}` |
| `GET /prod/zzz/v2/not-real` (a path that certainly does not exist) | `403 {"message":"Forbidden"}` |

A route that is real but gated and a route that was never there return
**byte-identical** responses. Adding a malformed `Authorization` header changes
nothing — an AWS Cognito authorizer would normally answer `401 Unauthorized`
for a token it cannot parse, and this surface does not. The SDK surface behaves
the same way: an invented `/public/v1/…` path yields an auth error, not a `404`.

Two consequences, both easy to get wrong:

- **You cannot probe this API to discover routes.** Absence of a `200` is not
  evidence of anything.
- **Never read a `403` as "the endpoint exists and I merely lack permission."**
  It is equally consistent with the endpoint not existing. Confirming a route
  requires a credential that can actually reach it, not a cleverer probe.

Note also that the CLI collapses the upstream body into its own auth error, so
the raw `{"message":"Forbidden"}` above is only visible outside the client.

## Tax documents: not reachable (blocked)

`lofty catalog --group taxdocuments` lists two `Read` endpoints,
`/taxdocuments/v2/all` (`getAllDocuments`) and `/taxdocuments/v2/zip-documents`
(`getZipFile`). Neither is usable from this CLI today, and the reason is
structural rather than a missing feature:

- Both exist only on the **internal** surface, behind the website Cognito
  session. Every combination tried — `GET` and `POST`, anonymous and with the
  SDK Bearer key — returns `403`.
- **The public SDK has no document route at all.** An audit of the published
  `@loftyaicode/sdk` package across versions 0.2.0–0.2.4 found 19 routes, all
  under `properties`, `orders`, `account`, `amm`, and `lp-rewards`. Searching
  the bundle and its type definitions for `tax`, `1099`, `K-1`, `document`,
  `download`, `statement`, `zip`, and `pdf` produces no functional hit — the
  sole `document` match is a browser-environment check. Nothing was published
  and later withdrawn.
- There is no OpenAPI spec and no developer documentation site (neither
  `docs.lofty.ai` nor `developer.lofty.ai` resolves).
- Lofty's own help center describes tax forms as a **dashboard-only** feature:
  log into the website, open the Taxes menu, click Download. No programmatic
  path is advertised.

So a `documents` command group cannot ship until `lofty` can present a website
session credential. Because of the `403` trap above, we cannot even confirm the
two catalog paths are live — only that nothing we can send reaches them.

When that capture eventually happens, two rules apply. Response shapes here are
**entirely unobserved**, so fixtures must come from a real, scrubbed capture
(`tests/fixtures/README.md`) and must never be invented to fit an assumed
shape. And if `getZipFile` returns a pre-signed S3 URL, that URL is a **live
credential** — like document ids and file names, it never lands in a fixture, a
test, or a commit.

## Catalog provenance

`src/catalog.rs` is a **static harvest** of the endpoint registry in the
website's JS bundle, captured in the initial commit. It records each route's
path, name, group, and safety class — but **not its HTTP method** — and the
entries have never been live-verified. The `403` trap makes verification by
probing impossible in principle.

Read `lofty catalog` as a map of what the website's front end refers to: useful
for orientation, not a guarantee that a route exists, is reachable with the
credentials this CLI holds, or accepts the method you assume.

## Follow-ups

- **Bump `cli-common`.** This repo pins `v0.1.2`. Upstream `HEAD` adds the
  `documents/v1` profile (`pk-cli-documents`), which fixes the canonical
  spelling — `documents list` plus `documents download <ID> -o <PATH>` — and
  the `document-list/v1`, `document-download/v1`, and
  `document-download-batch/v1` DTOs. Bump **before** any document support is
  written here, so `lofty` adopts the family shape instead of inventing a
  parallel one.
- **Observe the real shapes.** Reaching `/taxdocuments/v2/*` needs an
  interactive session at a logged-in browser (a captured idToken, or a full
  Cognito login flow in the CLI). Both need the account owner present and are
  out of scope for automated work.
