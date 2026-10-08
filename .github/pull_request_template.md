## What and why

<!-- What changes, and the problem it solves. -->

## Checklist

- [ ] `CONTRIBUTING.md`'s pre-PR commands pass locally
- [ ] Tests cover the change (wire-shape changes have a fixture or `wiremock` test)
- [ ] `CHANGELOG.md` has an `[Unreleased]` entry; a public-API break has migration notes
- [ ] Anything touching submissions keeps the never-replay rule and `begin_once`/`finish` pairing
- [ ] No account numbers, hashes, tokens or keys in code, fixtures or logs
