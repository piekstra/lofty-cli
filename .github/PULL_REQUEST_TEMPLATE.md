<!-- Thanks for contributing! Keep PRs focused. -->

## What & why

<!-- What does this change and why? Link any issue: Closes #123 -->

## User-visible changes

<!-- New/changed commands, flags, output. Note any changed `--json` `schema`
     tags — a breaking DTO change should bump its `/vN` suffix. -->

## Checklist

- [ ] `make verify` passes locally (fmt + clippy + tests + smoke)
- [ ] Tests added/updated (fixtures load from disk; no live-API calls in tests)
- [ ] No secrets, keys, real account ids, balances, or addresses in the diff
      (including fixtures — dummies only)
- [ ] Any new mutation confirms and honors `--force` (exit `6` when non-interactive)
- [ ] Docs/README updated if behavior changed
