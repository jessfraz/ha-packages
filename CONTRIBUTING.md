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
