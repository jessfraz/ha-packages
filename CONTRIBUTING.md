# Contributing

Use `nix develop` for Rust, Python, uv, SQLite, and the workflow validators.
`direnv allow` enables the same shell automatically if you use direnv.

Run the checks before opening a pull request:

```sh
cargo fmt --check
cargo clippy --locked --all --benches --tests --examples --all-features -- -D warnings
cargo test --locked --all-features
node --test tests_web/*.mjs
cargo deny check advisories licenses sources
uv sync --frozen
uv run --frozen ruff check custom_components tests_ha
uv run --frozen ruff format --check custom_components tests_ha
uv run --frozen ty check --error-on-warning
cargo build --locked --bin shipments
SHIPMENTS_TEST_BINARY="$PWD/target/debug/shipments" uv run --frozen pytest tests_ha
actionlint
shellcheck docker-entrypoint.sh scripts/container-smoke.sh
```

Keep changes focused. Add regression tests for behavior changes and update
the OpenAPI snapshot when the public API intentionally changes. Do not add
real shipments, tracking numbers, credentials, or personal information to
fixtures or issue reports.

To prepare a release, update the version in `Cargo.toml`, `pyproject.toml`,
`custom_components/shipments/manifest.json`, and `shipments/config.yaml`.
Refresh both lockfiles and the OpenAPI snapshot, then update the changelog.
Merge through the required checks and tag that commit `v<version>`.

The container workflow checks the versions agree, builds on native amd64 and
arm64 runners, and checks authentication and durable state after restart.
Both architectures must pass before the matching multiarch image is tagged
in GHCR. With Docker running locally, the same check is available as
`scripts/container-smoke.sh IMAGE ARCH`, where `ARCH` is `amd64` or `arm64`.

The first publication also requires making the GHCR package public so Home
Assistant can pull it without a login.

## API contract

Regenerate OpenAPI after intentional API changes:

```sh
EXPECTORATE=overwrite cargo test --locked --test api generated_openapi_matches_the_committed_contract
```

## Tests

Tests start real HTTP servers and SQLite databases, including a local EasyPost
fixture. The Home Assistant suite uses Home Assistant itself and includes a
contract test against the compiled Rust binary. CI builds that binary before
running the Python suite. The default suite needs no API keys and makes no
external provider requests.

The separate **EasyPost sandbox** CI workflow uses the repository's
`EASYPOST_TEST_API_KEY` Actions secret. It runs against EasyPost's test API on
main, trusted same-repository pull requests and manual dispatches. Fork and
Dependabot pull requests run the deterministic suite without provider secrets.
The key is available only to the live test step; an absent or non-test key
fails before any provider request.

Run the same test locally with an EasyPost test key in the environment:

```sh
cargo test --locked --test easypost_live -- --ignored --test-threads=1
```

It exercises the real HTTP API, SQLite restart/replay, auto-detected USPS,
explicit UPS and FedEx Default, unscanned and delivered states, carrier scan
history, accepted tracking edits and rejected creates/edits. Only synthetic
`EZ…` test codes are submitted. These canned test responses validate our API
integration; they do not establish live carrier-account coverage. Actual
HAOS installation and production carrier coverage need separate verification.
