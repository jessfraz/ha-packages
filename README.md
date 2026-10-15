# ha-packages

A local shipment registry for Home Assistant. Add a delivery yourself or through
the API, let EasyPost check its progress, and correct the status when you know
better. Carrier updates never undo a manual correction.

One Rust process, a SQLite database, and a small Home Assistant adapter. Records
and history stay local. EasyPost is optional; adding and editing shipments
works when it is disabled or unavailable.

## Run

```sh
nix develop
export SHIPMENTS_API_TOKEN="$(openssl rand -hex 32)"
cargo run
```

Open `http://127.0.0.1:8080` and enter the token. The browser keeps it in memory
for that tab. The default database is `shipments.sqlite3` in the working
directory. Set `SHIPMENTS_DATABASE` to change it.

Set `EASYPOST_API_KEY` to enable tracking. Standalone tracker registration can
incur EasyPost charges, and some carriers require their own credentials. The
service makes no carrier requests until tracking is configured and a shipment
has a tracking number. The provider key belongs to the server alone.

For Docker and Home Assistant installation, see
[the app instructions](shipments/DOCS.md). The app and the Python integration
are separate installation artifacts in this repository.

## API

The Rust API is under `/v1`. The Home Assistant integration forwards it under
`/api/shipments/v1` using Home Assistant authentication. The generated contract
is available at `/openapi.json` and checked in at [openapi/api.json](openapi/api.json).

```sh
curl --fail-with-body http://127.0.0.1:8080/v1/shipments \
  -H "Authorization: Bearer $SHIPMENTS_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: example-delivery-1' \
  -d '{"title":"New desk","merchant":"Example shop"}'
```

Each record represents one parcel or expected delivery. Several parcels can
share an order reference. Tracking, merchant, order reference, and source are
optional. There is no hard-delete endpoint; archive unwanted records.

| Method | Path | Operation |
| --- | --- | --- |
| GET / POST | `/v1/shipments` | List or create |
| GET / PATCH | `/v1/shipments/{id}` | Read or edit |
| GET | `/v1/shipments/{id}/events` | Read history |
| POST | `/v1/shipments/{id}/sources` | Attach evidence |
| PUT / DELETE | `/v1/shipments/{id}/status-override` | Set or clear a correction |
| PUT / DELETE | `/v1/shipments/{id}/archive` | Archive or restore |
| POST | `/v1/shipments/{id}/refresh` | Queue a tracking check |
| GET | `/v1/summary` | Read counts |
| GET | `/v1/system` | Read synchronization health |

Creates require `Idempotency-Key`. The committed response survives restarts;
retrying the same request returns it. A different input with the same key
returns `409`. An active tracking identity or source cannot belong to a second
record. Archive an old shipment before reusing its tracking number.

Reads and writes return an `ETag` containing the shipment revision. Every write
to an existing shipment requires that value in `If-Match`. A stale edit returns
`412`; a missing condition returns `428`. For `PATCH`, omitted fields stay as
they are and `null` clears optional fields. Carrier state and audit identity
cannot be written in request bodies.

List filters include `tracking_number`, `carrier`, `order_reference`,
`source_namespace` with `source_key`, `status`, `search`, and `archived`.
Lists and history use `items` and `next_page`. Pass the token as `page_token`
to continue, with an optional `limit` of up to 200.

## Agents and human corrections

Set a separate `SHIPMENTS_AGENT_TOKEN` for ingestion clients. It can read, add,
edit, attach evidence, and request tracking updates. It cannot set or clear
manual corrections, archive, or restore. Human actions use the owner token or
an authenticated Home Assistant administrator. The adapter derives the actor
from Home Assistant authentication.

An ingestion agent should look up a tracking or source reference, create or
patch the record, and attach new evidence. The app handles tracking afterward.
An email claiming delivery is evidence, not a human confirmation.

Home Assistant tokens grant access beyond this integration. Give agents the
restricted shipment credential through a constrained tool boundary rather than
giving them broad Home Assistant credentials. Use HTTPS or a trusted local
transport when carrying bearer credentials beyond loopback.

## Persistence and synchronization

SQLite transactions commit the record, history snapshot, and idempotency
response together. Duplicate checks hold the SQLite writer lock. History
preserves previous values as well as who changed them.

The worker claims due records using persisted leases, makes its request outside
the transaction, and checks the lease and tracking generation before saving.
An old response cannot overwrite a corrected tracking number or archived
record. Failed requests retain the local record and retry with backoff from
one minute to six hours. Ordinary polling is every 30 minutes, or five minutes
when out for delivery. Terminal carrier states and terminal manual corrections
stop polling. An explicit refresh allows one further check and respects a
pending error backoff.

Home Assistant uses cold backups: the app stops while its persistent `/data`
directory is copied. For standalone operation, stop the service before copying
the database, or use SQLite's online backup facility. Do not copy only the main
database file while a writer is running in WAL mode.

## Development

```sh
nix develop
cargo fmt --check
cargo clippy --locked --all --benches --tests --examples --all-features -- -D warnings
cargo test --locked --all-features
uv sync --frozen
uv run --frozen ruff check custom_components tests_ha
uv run --frozen ruff format --check custom_components tests_ha
SHIPMENTS_TEST_BINARY="$PWD/target/debug/shipments" uv run --frozen pytest tests_ha
```

Regenerate OpenAPI after intentional API changes:

```sh
EXPECTORATE=overwrite cargo test --test api generated_openapi
```

Tests start real HTTP servers and SQLite databases, including a local EasyPost
fixture. The Home Assistant suite uses Home Assistant itself and includes a
contract test against the compiled Rust binary. CI builds that binary before
running the Python suite. Tests do not register real trackers or require API
keys. Actual HAOS installation and live carrier coverage still need testing
on the target system.
