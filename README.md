# ha-packages

A local shipment registry for Home Assistant. Add a delivery yourself or through
the API, let EasyPost check its progress, and correct the status when you know
better. Carrier updates never undo a manual correction.

One Rust process, a SQLite database, and a small Home Assistant adapter. Records
and history stay local. Every shipment needs a tracking number, verified by
EasyPost before saving. Carrier detection is automatic when left blank. New
labels can be saved before the carrier's first scan.

## Install on Home Assistant

Install the **Shipments app** from the App Store to run the service and
dashboard. Add `https://github.com/jessfraz/ha-packages` to **HACS → Custom
repositories** as an **Integration**, download **Shipments**, and restart Home
Assistant. Confirm the discovered integration in **Settings → Devices &
services** to enable sensors and native actions. The app manages credentials
automatically.

See [installation and configuration](shipments/DOCS.md) for both installation
steps, EasyPost configuration, and standalone Docker instructions. The app
dashboard's **Installation → Run checks** verifies the service, Home Assistant
connection, authentication, read-only EasyPost access, and backup coverage.

## Run standalone

```sh
nix develop
export SHIPMENTS_API_TOKEN="$(openssl rand -hex 32)"
export EASYPOST_API_KEY="your-easypost-api-key"
cargo run
```

Open `http://127.0.0.1:8080` and enter the token. The browser keeps it in memory
for that tab. The default database is `shipments.sqlite3` in the working
directory. Set `SHIPMENTS_DATABASE` to change it.

`EASYPOST_API_KEY` is required to add a shipment or change its tracking identity.
Existing records can still be read, corrected, or edited without changing
tracking when EasyPost is unavailable. Tracker registration can incur EasyPost
charges, and some carriers require their own credentials. The provider key
belongs to the server alone.

## API

The Rust API is under `/v1`. The Home Assistant integration forwards it under
`/api/shipments/v1` using Home Assistant authentication. The generated contract
is available at `/openapi.json` and checked in at [openapi/api.json](openapi/api.json).

```sh
curl --fail-with-body http://127.0.0.1:8080/v1/shipments \
  -H "Authorization: Bearer $SHIPMENTS_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: example-delivery-1' \
  -d '{"title":"New desk","tracking":{"number":"YOUR_TRACKING_NUMBER"}}'
```

Replace `YOUR_TRACKING_NUMBER` with the carrier's tracking number. Each record
represents one parcel or expected delivery. Several parcels can share an order
reference. A tracking number is required; merchant, order reference, source,
and carrier are optional. There is no hard-delete endpoint; archive unwanted
records.

EasyPost must accept the tracking identity before a create or tracking change
is saved. Unknown and pre-transit states are accepted without a carrier scan.
Explicitly invalid numbers, unsupported carriers, and ambiguous carrier
detection are rejected. Choose a carrier if detection is ambiguous. A provider
outage or missing key returns `503` with no saved change; retry when available.

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
| GET | `/v1/installation` | Read installation status with an owner credential |
| POST | `/v1/installation/check` | Run the read-only provider check with an owner credential |
| GET / POST | `/v1/agents` | List or create ingestion credentials with owner access |
| DELETE | `/v1/agents/{id}` | Revoke an ingestion credential with owner access |

Shipment creates require `Idempotency-Key`. The committed response survives
restarts; retrying the same request returns it. A different input with the same key
returns `409`. An active tracking identity or source cannot belong to a second
record. Archive an old shipment before reusing its tracking number.

Reads and writes return an `ETag` containing the shipment revision. Every write
to an existing shipment requires that value in `If-Match`. A stale edit returns
`412`; a missing condition returns `428`. For `PATCH`, omitted fields stay as
they are and `null` clears optional fields. Carrier state and audit identity
cannot be written in request bodies. Tracking cannot be removed.

List filters include `tracking_number`, `carrier`, `order_reference`,
`source_namespace` with `source_key`, `status`, `active`, `search`, and `archived`.
`active=true` excludes delivered, returned, and cancelled shipments, using the
manual correction when one exists. Archiving is independent of status. The
dashboard opens in the active view and refreshes while the tab is visible.
Lists and history use `items` and `next_page`. Pass the token as `page_token`
to continue, with an optional `limit` of up to 200.

## Agents and human corrections

Open **Agent access** in the owner dashboard to create a named ingestion
credential. Copy the token when it appears and save it in your password
manager; it is shown only once. Each credential can be revoked individually.
The service stores its hash in SQLite, and access survives restarts.

Agents can read, add, edit, attach evidence, and request tracking updates.
Manual corrections, archiving, installation checks, and credential management
require owner access.
Home Assistant status corrections and archiving require an authenticated
administrator. The adapter derives the actor from Home Assistant authentication.

An ingestion agent should look up a tracking or source reference, create or
patch the record, and attach new evidence. The app handles tracking afterward.
An email claiming delivery is evidence, not a human confirmation.

Tracking verification errors include an `error_code` and readable `message`.
Use the code to distinguish bad input from a failure worth retrying:

| HTTP | `error_code` | Action |
| --- | --- | --- |
| 400 | `tracking_required` | Supply a tracking number; tracking cannot be cleared. |
| 400 | `tracking_unsupported`, `tracking_invalid` | Check the number and supported carrier. |
| 400 | `tracking_carrier_mismatch`, `tracking_carrier_ambiguous` | Specify the correct carrier. |
| 400 | `tracking_test_key` | Use a production EasyPost key for real shipments. |
| 503 | `tracking_provider_unconfigured`, `tracking_provider_credentials` | Configure or fix the EasyPost key before retrying. |
| 503 | `tracking_carrier_account_required` | Configure the carrier account in EasyPost before retrying. |
| 503 | `tracking_verification_rate_limited` | Wait before retrying; honor `Retry-After` when present. |
| 503 | `tracking_verification_unavailable` | Retry with backoff; the number has not been rejected as invalid. |

These failures leave the registry unchanged. Retry a create with its original
idempotency key after fixing configuration or waiting for provider recovery.

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
when out for delivery, with up to 10% added per-shipment jitter. Unknown and
pre-transit labels that keep the same status slow to two-hour checks after two
days and six-hour checks after a week. A status change restores its normal
schedule. Terminal carrier states and terminal manual corrections stop polling.

All EasyPost operations share one outbound slot, with at least one second
between request starts. A rate-limit response pauses polling, new tracking
verification, and connection checks together, honoring `Retry-After` with a
minimum one-minute exponential cooldown. During a pause or an in-flight
request, tracking verification returns `tracking_verification_rate_limited`
with `Retry-After`; no changes are saved. The shared provider pause lasts for
the current process; shipment retries and refresh cooldowns survive restarts.

Manual refreshes coalesce and wait at least one minute after the last provider
attempt, respecting longer pending error backoff and earlier scheduled checks.
An explicit terminal refresh allows one further check. Repeated failed terminal
checks also respect the cooldown.

Home Assistant uses cold backups: the app stops while its persistent `/data`
directory is copied. For standalone operation, stop the service before copying
the database, or use SQLite's online backup facility. Do not copy only the main
database file while a writer is running in WAL mode. Backups preserve saved
agent access and revocations at the time of the backup.

Upgrading to 0.3.0 permanently removes legacy shipments without a tracking
number, including their sources, history, and saved idempotency responses.
Tracked records and their existing history are preserved. Back up before
upgrading.

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
