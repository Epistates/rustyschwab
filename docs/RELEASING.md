# Releasing

The checklist for a release. [CONTRIBUTING.md](../CONTRIBUTING.md#releases) has the short version.

A release is a `vX.Y.Z` tag on a `main` commit. The tag push starts `release.yml` (binaries, SBOM, provenance and a draft GitHub Release) and `publish-python.yml` (wheels and sdist, published to PyPI after approval). `publish.yml` publishes the crates when you run it for the tag. Both publish jobs refuse a tagged commit that has not passed CI and `release.yml`.

## Repository settings

These live in GitHub and the registries, not in the repository, so no review sees them change. Check them before a release, and after any change of maintainers.

- **Environments.**
  - `crates-io` and `pypi`: required reviewers (the maintainers), and deployments limited to tags matching `v*`.
  - `live-schwab`: deployments limited to the `main` branch. It holds `SCHWAB_APP_KEY`, `SCHWAB_APP_SECRET` and `SCHWAB_TOKENS_JSON` for the live conformance job in `qualification.yml`. Keep no repository-level copies of them: a repository secret reaches a run from any branch, so anyone who can push a branch could print it.
- **Tag ruleset.** Only maintainers may create, update or delete tags matching `v*`, since a tag starts the release workflows.
- **Trusted publishers.** On crates.io, each crate (`rustyschwab-types`, `rustyschwab`, `rustyschwab-mcp`, `rustyschwab-console`, `rustyschwab-dash`) trusts repository `Epistates/rustyschwab`, workflow `publish.yml`, environment `crates-io`. On PyPI, `rustyschwab` trusts workflow `publish-python.yml`, environment `pypi`.
- **Branch protection** on `main`, with the checks [CONTRIBUTING.md](../CONTRIBUTING.md#before-opening-a-pull-request) lists.

## Before tagging

1. **Live evidence.** Run the [live smoke](LIVE_SMOKE.md) and the read-only conformance sweep with `record`. Commit the smoke report and any fixture diff, and update the live verification table in [SUPPORT.md](SUPPORT.md#live-verification-status). Do not release a version whose README claims more than that table shows.
2. **Checks.** CI is green on `main` for the commit you will tag, on Linux, macOS and Windows, with the Python matrix. Run the Python checks in `crates/schwab-py` locally too if you changed the bindings (`cargo test`, `maturin develop`, `pytest`, `mypy --strict python/rustyschwab`).
3. **Versions.** Set the new version in one commit:
   - `version` in the workspace `Cargo.toml`;
   - every internal `version = "…"` on a path dependency in `crates/*/Cargo.toml` (`rustyschwab-types`, `rustyschwab`, `rustyschwab-mcp`, `rustyschwab-console`);
   - `version` in `crates/schwab-py/Cargo.toml`, and its `rustyschwab` dependency.

   Then `cargo update -w` and, in `crates/schwab-py`, `cargo update -p rustyschwab`, so both lockfiles agree. CI and the publish workflows refuse a bindings version that differs from the SDK's, and a tag that differs from either.
4. **Changelog.** Rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD` and open a new empty `## [Unreleased]`. Every **Breaking** entry needs its migration notes. `cargo-semver-checks` runs only on pull requests, so for a release built from direct pushes to `main`, run `cargo semver-checks` against the previous tag yourself.
5. **README.** Update the status line and the install lines (`rustyschwab = "X.Y"`).
6. **Dry runs.** `cargo package -p rustyschwab-types -p rustyschwab -p rustyschwab-mcp -p rustyschwab-console -p rustyschwab-dash --locked`, and run `publish-python.yml` by hand from `main` to build and test the wheels and sdist without publishing.

## Tag and publish

1. Tag the release commit and push the tag: `git tag -a vX.Y.Z -m vX.Y.Z && git push origin vX.Y.Z`.
2. Wait for `release.yml` to pass. It runs the workspace tests on the tag, checks that the tag matches the crate version and that the binary list matches the packages, and drafts the GitHub Release.
3. **PyPI.** `publish-python.yml` builds the wheels and sdist, runs the test suite against the Linux wheel, and waits for approval in the `pypi` environment. Approve it once `release.yml` has passed; the job checks that it did, and that CI passed on the same commit, before it uploads.
4. **crates.io.** Run `publish.yml` for the tag and approve it in the `crates-io` environment. It makes the same check, packages all five crates, then publishes them in dependency order. A release that adds a crate publishes that crate the first time with a short-lived API token, since trusted publishing cannot create a crate: from a clean checkout of the tag, `cargo publish -p <crate> --locked`, then add the crate's trusted publisher and revoke the token.
5. Review and publish the drafted GitHub Release. Verify an artifact with `gh attestation verify <file> --repo Epistates/rustyschwab`.

## After publishing

- Install from the registries in a clean environment (`cargo install rustyschwab-mcp`, `pip install rustyschwab`) and run the README quickstarts.
- Check the docs.rs builds of all five crates.
- Watch for a stable TurboMCP 4.x; `rustyschwab-mcp` pins `=4.0.0-alpha.5` until then (see [SUPPORT.md](SUPPORT.md#support-tiers)).
