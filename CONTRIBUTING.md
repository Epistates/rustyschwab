# Contributing

Thanks for helping. This file covers the mechanics; read
[docs/ARCHITECTURE_OVERVIEW.md](docs/ARCHITECTURE_OVERVIEW.md) before
changing anything non-trivial, and its "Where to look" table to find the
right module.

## Reporting issues

Search existing issues first. Include what you ran, what you expected,
what happened, the crate version or commit, and your OS and Rust version.
Redact account hashes, tokens and app keys from logs. Report security
problems privately rather than in a public issue.

## Development setup

Rust 1.89 or newer (edition 2024). No other toolchain is needed; SQLite is
bundled.

```bash
git clone <repository-url>
cd rustyschwab
cargo build --workspace --all-targets --all-features
cargo test --workspace --all-targets --all-features
```

Examples are workspace packages, so run them with `-p`:

```bash
cp .env.example .env   # then fill in SCHWAB_APP_KEY / SCHWAB_APP_SECRET
cargo run -p oauth-flow
cargo run -p comprehensive
cargo run -p streaming-examples --bin streaming_quotes
```

## Before opening a pull request

Run what CI runs (`.github/workflows/ci.yml`):

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo doc --workspace --no-deps --locked
cargo build --workspace --all-targets --locked
cargo test --workspace --all-targets --all-features --locked
cargo test --workspace --doc --all-features --locked
cargo check -p schwab-rs --no-default-features --locked
```

The two `cargo test` lines are both needed: `--all-targets` skips doctests.
CI additionally tests on the MSRV, runs `cargo audit`, compares the public
API against the base branch with `cargo-semver-checks`, and runs the stream
soak harness. Intentional public-API breaks need a CHANGELOG entry with
migration notes.

Then:

1. Add or update tests. Wire-shape changes need a fixture or `wiremock`
   test; decoding changes should keep
   `crates/schwab-types/tests/properties.rs` (no panic on arbitrary input)
   passing. If you have Schwab credentials, also run the read-only live
   sweep so the typed bindings are checked against the real API:

   ```bash
   SCHWAB_LIVE_CONFORMANCE=1 cargo test -p schwab-rs --test live_conformance -- --nocapture
   ```

   It only issues `GET`s. `SCHWAB_LIVE_CONFORMANCE=record` refreshes
   `crates/schwab-rs/tests/fixtures/conformance/`; commit the fixture diff
   with the change that explains it.
2. Add a line under `## [Unreleased]` in `CHANGELOG.md`
   (Keep a Changelog format).
3. Update `.env.example` and the README "Environment Variables" section if
   you add or rename a `SCHWAB_*` variable, and
   `crates/schwab-mcp/tests/tool_inventory.rs` if you add an MCP tool.
4. Keep commits focused; one concern per commit.

## Style

`rustfmt` and `clippy -D warnings` are the style guide. Beyond that:

- Prefer typed fields over `String`/`Value`; if Schwab's shape is
  uncertain, say so in a comment and keep the tolerant type on the response
  side only.
- Errors go through `schwab_rs::Error`; add a variant rather than stringly
  errors, and set `ErrorKind`/`is_retryable` correctly.
- Every metric is declared in `metrics.rs` with its label set; do not emit
  ad-hoc names.
- Anything that touches submissions must keep the never-replay rule and the
  journal `begin_once`/`finish` pairing.
- Public items carry doc comments; doc examples must compile and pass
  `cargo test --doc`.

## Commit messages

Conventional commit style: `type(scope): summary`, where type is one of
`feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `perf`, `ci`. The body
explains why, not what.

## Releases

1. Bump `version` in the workspace `Cargo.toml`.
2. Move the `[Unreleased]` section in `CHANGELOG.md` to a dated version.
3. Tag `vX.Y.Z` and push the tag. `release.yml` builds the binaries, an
   SBOM and a provenance attestation.
4. Publish `schwab-types`, then `schwab-rs`, then `schwab-mcp`.

See [docs/SUPPORT.md](docs/SUPPORT.md) for the compatibility policy.
