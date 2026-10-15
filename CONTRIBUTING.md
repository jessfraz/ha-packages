# Contributing

Use `nix develop` for Rust, Python, uv, SQLite, and the workflow validators.
`direnv allow` enables the same shell automatically if you use direnv.

Run the checks before opening a pull request:

```sh
cargo fmt --check
cargo clippy --locked --all --benches --tests --examples --all-features -- -D warnings
cargo test --locked --all-features
cargo deny check advisories licenses sources
uv sync --frozen
uv run --frozen ruff check custom_components tests_ha
uv run --frozen ruff format --check custom_components tests_ha
cargo build --locked --bin shipments
SHIPMENTS_TEST_BINARY="$PWD/target/debug/shipments" uv run --frozen pytest tests_ha
actionlint
shellcheck docker-entrypoint.sh
```

Keep changes focused. Add regression tests for behavior changes and update
the OpenAPI snapshot when the public API intentionally changes. Do not add
real shipments, tracking numbers, credentials, or personal information to
fixtures or issue reports.

To prepare a release, update the Cargo package version and the app version in
`shipments/config.yaml` together, update the changelog, and tag that commit
`v<version>`. The container workflow checks the versions match and publishes
the matching amd64/aarch64 image to GHCR. The first publication also requires
making the GHCR package public so Home Assistant can pull it without a login.
