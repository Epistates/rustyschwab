# Releasing

The checklist for a release, starting with 0.2.0, the first one published to crates.io and PyPI. [CONTRIBUTING.md](../CONTRIBUTING.md#releases) has the short version for later releases.

## Before tagging

1. **Live evidence.** Run the [live smoke](LIVE_SMOKE.md) and the read-only conformance sweep with `record`. Commit the smoke report and any fixture diff, and update the live verification table in [SUPPORT.md](SUPPORT.md#live-verification-status). Do not release a version whose README claims more than that table shows.
2. **Checks.** Everything in [CONTRIBUTING.md](../CONTRIBUTING.md#before-opening-a-pull-request) passes on `main`, plus the Python checks in `crates/schwab-py` (`cargo test`, `pytest`, `mypy --strict python/rustyschwab`). CI is green on Linux, macOS and Windows.
3. **Versions.** `version` in the workspace `Cargo.toml` and in `crates/schwab-py/Cargo.toml` agree; the publish workflows refuse to run otherwise.
4. **Changelog.** Rename `## [Unreleased — 0.2.0 candidate]` to `## [0.2.0] - YYYY-MM-DD` and open a new empty `## [Unreleased]`.
5. **README.** Replace the install-from-repository lines with registry installs (`rustyschwab = "0.2"`, `pip install rustyschwab`, `cargo install rustyschwab-mcp`) and update the status line.
6. **Dry runs.** `cargo package -p rustyschwab-types -p rustyschwab -p rustyschwab-mcp -p rustyschwab-console -p rustyschwab-dash --locked`, and run `publish-python.yml` by hand to build and inspect the wheels and sdist without publishing.

## First publish (0.2.0 only)

crates.io trusted publishing cannot create a crate, so the first version of each crate is published with an API token ([crates.io docs](https://crates.io/docs/trusted-publishing)). `rustyschwab`, `rustyschwab-types` and `rustyschwab-mcp` were unclaimed on crates.io and `rustyschwab` on PyPI as of 2026-10-17.

1. Tag and push: `git tag -a v0.2.0 -m v0.2.0 && git push origin v0.2.0`. `release.yml` builds the binaries, SBOM and provenance and drafts a GitHub Release; `publish-python.yml` builds the wheels and waits for approval in the `pypi` environment.
2. **PyPI:** before approving, add a *pending* trusted publisher for the `rustyschwab` project on PyPI (owner `Epistates`, repository `rustyschwab`, workflow `publish-python.yml`, environment `pypi`). Then approve the job.
3. **crates.io:** from a clean checkout of the tag, with a scoped, short-lived API token that can publish new crates:

   ```bash
   cargo publish -p rustyschwab-types --locked
   cargo publish -p rustyschwab --locked
   cargo publish -p rustyschwab-mcp --locked
   ```

   Then, on crates.io, add a GitHub trusted publisher to each crate (repository `Epistates/rustyschwab`, workflow `publish.yml`, environment `crates-io`) and revoke the token.
4. Review and publish the drafted GitHub Release. Verify an artifact with `gh attestation verify <file> --repo Epistates/rustyschwab`.

## Later releases

Bump the versions, move the changelog section, tag and push. Approve `publish-python.yml` in the `pypi` environment, run `publish.yml` for the tag and approve it in the `crates-io` environment, then publish the GitHub Release.

A release that adds a crate (`rustyschwab-console` and `rustyschwab-dash` arrive after 0.3.0) publishes it the first time with an API token, as above, since trusted publishing cannot create a crate; `cargo publish --workspace --locked` publishes every crate in dependency order. Add the trusted publisher to the new crates afterwards.

## After publishing

- Install from the registries in a clean environment and run the README quickstarts.
- Check the docs.rs builds for all three crates.
- Watch for a stable TurboMCP 4.x; `rustyschwab-mcp` pins `=4.0.0-alpha.5` until then (see [SUPPORT.md](SUPPORT.md#support-tiers)).
