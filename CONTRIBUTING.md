# Contributing to lofty-cli

Thanks for your interest! Contributions — bug reports, fixes, new commands,
docs — are welcome under the project's [MIT license](LICENSE): by opening a pull
request you agree your contribution is licensed under those same terms.

## Ground rules (this is a trading client — safety first)

- **Never commit secrets or personal data.** The API key lives only in the OS
  keychain; nothing credential-like belongs in the repo, tests, or fixtures.
- **Fixtures: no account/personal data.** `account/` fixtures use dummy ids and
  amounts (`01SAMPLE…`) — never a real account capture. `public/` and internal
  market-data fixtures may contain real *public* marketplace data (property ids,
  prices), but nothing account-specific. Any new fixture must scrub account ids,
  balances, and amounts to obvious dummies.
- **Read-only by default.** Any command that mutates (order create/cancel, AMM
  swap) must prompt for confirmation and require `--force` to proceed
  non-interactively (exit `6` otherwise). Don't add a mutation that can fire
  without one of those.
- **Secrets never on argv or in logs.** Read them from the keychain or stdin.

## Dev loop

```console
$ make verify     # fmt-check + clippy -D warnings + tests + smoke — the CI gate
$ make test       # unit + integration (fully offline; no network, no creds)
$ cargo run -- <command>
```

`make verify` is exactly what CI runs; a green local run predicts a green CI run.

## Pull requests

1. Branch from `main`; keep the change focused.
2. Add or update tests. Contract/shape tests load fixtures from disk — extend
   them rather than hitting the live API in tests.
3. Run `make verify` and make sure it's green.
4. Fill in the PR template. Describe user-visible behavior and any new/changed
   `--json` `schema` tags (these are a compatibility surface — bump the `/vN`
   suffix on a breaking DTO change).

## Coding notes

- Rust, `clap` derive, `edition = 2021`, MSRV in `Cargo.toml` (`rust-version`).
- Shared CLI plumbing (auth, http, config, self-update) comes from the public
  [`cli-common`](https://github.com/piekstra/cli-common) `pk-cli-*` crates —
  keep this repo focused on Lofty-specific commands and DTOs.
- Every command supports `--json`; the human and JSON paths must stay in sync.

## Reporting bugs / requesting features

Use the [issue templates](https://github.com/piekstra/lofty-cli/issues/new/choose).
For anything involving an account or a key, **redact** ids, balances, and the
key itself before pasting output.
