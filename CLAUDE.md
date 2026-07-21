# CLAUDE.md

The canonical agent guide for this repo is **[AGENTS.md](AGENTS.md)** — read it
first. It covers build/test/lint, layout, conventions, and the safety rules.

Claude Code specifics:

- **Gate on `make verify`.** Don't report a change as done until it's green
  (fmt + clippy `-D warnings` + tests + smoke). Tests are fully offline.
- **Never run mutating commands to "test" them.** `lofty orders create/cancel`
  and `lofty amm swap` move real money on a real account. Reason about them;
  don't execute them. If you must, they require explicit `--force`.
- **Secrets:** the API key is in the OS keychain (`piekstra.lofty`). Never print
  it, put it on argv, or write it to a file.
- **"Deployed" means released + installed.** A CLI change isn't live until a
  release is cut (tag `v*` → the release workflow) and the binary is installed
  or `self-update`d on the target machine.
- **Public repo, real money.** No secrets, real account ids, balances, or PII in
  any diff — including test fixtures (dummies only).
