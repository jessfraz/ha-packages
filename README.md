# ha-packages

A local shipment registry for Home Assistant. Add a delivery yourself or through
the API, let EasyPost check its progress, and correct the status when you know
better. Carrier updates never undo a manual correction.

One Rust process, a SQLite database, and a small Home Assistant adapter. Records
and history stay local. Every shipment needs a tracking number and an inbound
or outbound direction. Shipments are saved locally before EasyPost registers
them in the background. New labels do not need a carrier scan.

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

`EASYPOST_API_KEY` enables background registration and tracking updates.
Creates and tracking edits are saved even when the key is missing or EasyPost
is unavailable; registration remains pending until tracking can run. Tracker registration can incur EasyPost
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
  -d '{"title":"New desk","direction":"inbound","tracking":{"number":"YOUR_TRACKING_NUMBER"}}'
```

Replace `YOUR_TRACKING_NUMBER` with the carrier's tracking number. Each record
represents one parcel or expected delivery. Several parcels can share an order
reference. A tracking number and `direction` (`inbound` or `outbound`) are
required; merchant, order reference, source, and carrier are optional. Existing
records migrate to inbound. Direction can be edited but cannot be cleared. There is no hard-delete endpoint; archive unwanted
records.

Create returns `201` after committing the shipment, history and idempotency
response together. `sync.registration_state` starts as `pending`; a worker
registers the number and sets it to `registered` after provider acceptance.
Unknown and pre-transit statuses are accepted without scans. An explicit
unsupported-number response becomes `rejected` with a safe `sync.error_code`
and readable `sync.last_error`; the record remains available to correct.
Tracking-number edits follow the same save-first process and clear the old
carrier history. Pending registration is never presented as verified tracking.

| Method | Path | Operation |
| --- | --- | --- |
| GET / POST | `/v1/shipments` | List or create |
| GET | `/v1/carriers` | List supported EasyPost carrier values |
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
`source_namespace` with `source_key`, `status`, `active`, `exceptions`, `search`,
`archived`, `direction`, and `sort`. Summary accepts an optional `direction`
filter; omitting it counts both directions.
`active=true` excludes delivered, returned, and cancelled shipments, using the
manual correction when one exists. Archiving is independent of status. The
dashboard opens in the active view and refreshes while the tab is visible.
Lists and history use `items` and `next_page`. Pass the token as `page_token`
to continue, with an optional `limit` of up to 200.

Carrier values are a closed enum using EasyPost's canonical tracking strings,
including `USPS`, `UPS`, and `FedEx`. `GET /v1/carriers` supplies the same catalog
used by the add-shipment dropdown. Omit carrier for automatic detection. Older
case-insensitive spellings remain readable; responses use canonical names.

EasyPost treats `FedEx` and `FedExDefault` as separate carrier choices.
`FedEx` requires your own configured carrier credentials; the EasyPost Wallet
choice is `FedExDefault`. A valid number can fail with HTTP 404 and native
`CREDENTIALS_NOT_FOUND` when the requested choice has no credentials. This is
an account/carrier setup error, not evidence that the tracking number is bad.
Choose the appropriate canonical carrier or configure its account. The app
keeps the requested carrier; it never switches to a different billable carrier
after an uncertain result.

Use `sort=expected` for earliest estimated delivery first, or `sort=delivered`
for newest actual delivery first. Unknown dates come last. The date and ID
cursor preserves ordering across pages; omitting `sort` keeps ID ordering.
Manual delivery corrections supply a delivery timestamp when no carrier
scan reports one. An ETA is never treated as an actual delivery date.

Shipment responses include `carrier_tracking` with carrier scan events,
estimated and actual delivery dates, and available signature proof. History
merges scans across updates and survives restarts; tracking replacement clears
old carrier data. A maximum of 512 scans is retained, with `events_truncated`
reported when that limit is reached.

`signature_required` is optional known shipment metadata: `true`, `false`, or
unknown (`null` or omitted). Carrier signature proof is separate from a future
requirement; EasyPost's standard tracker does not report a required-signature
flag. Agents can record a requirement from shipping notices, and humans can
set it in the editor. Replacing tracking clears it unless the same edit
supplies the requirement for the new shipment.

The dashboard's status cards filter the entire list, including search and
pagination. Active shipments show expected dates; delivered shipments show
actual delivery dates. Installation checks and agent access sit below the
list, with installation promoted above it only when a check needs attention.
A passed EasyPost diagnostic retains its result and timestamp until another
check replaces it; elapsed time does not turn it into a failure.

The default dashboard shows inbound and outbound shipments together, with
distinct badges. Direction filters are optional.
Dashboard fragments preserve direction, status view, search, open shipment,
and expanded settings. Copy the address to return to the same view; browser
Back and Forward work too. Shipment details put carrier history before the
action buttons and manual corrections. Sources are collapsed at the bottom.
Carrier locations have explicit map links; the dashboard does not geocode
shipment locations or load third-party maps automatically.

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

Malformed or empty tracking is rejected at the API before saving. API errors
include `error_code`, a readable `message`, and a request ID. Registration
failures are stored on the shipment instead of losing the create request:

| Where | Code | Action |
| --- | --- | --- |
| HTTP 400 | `tracking_required` | Supply a tracking number; tracking cannot be cleared. |
| HTTP 400 | `tracking_invalid` | Check the number's format; unsupported carrier values are also rejected by the enum. |
| Shipment sync | `tracking_unsupported`, `tracking_invalid`, `tracking_carrier_mismatch`, `tracking_carrier_ambiguous` | Correct the number or specify the right carrier. |
| Shipment sync | `tracking_test_key`, `tracking_carrier_account_required`, `tracking_carrier_credentials_required` | Fix the EasyPost key, select the appropriate carrier choice, or configure its account. |
| Shipment sync | `provider_http`, `provider_timeout`, `provider_connection`, `provider_invalid_response` | The app retries with backoff; these do not establish that a number is invalid. |
| Shipment sync | `registration_ambiguous`, `registration_expired` | Registration needs review; the app will not risk another paid registration automatically. |

Inspect `sync.provider_status`, `sync.provider_error_code`, `sync.attempts`,
and `sync.next_attempt_at` for provider failures and retry timing. Known native
provider codes are retained without storing arbitrary provider error text. Use GET to read the current state: an
idempotency replay returns the original committed create response, which may
still say pending even after registration succeeds. Retry an uncertain local
create with its original idempotency key. Do not create a fresh record or key
just because registration is pending.

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
between request starts. Rate limits pause registration, polling and connection
checks together. New local records can still be saved during a pause. Provider
cooldowns honor bounded `Retry-After` values with exponential backoff. The
shared pause lasts for the process; shipment retries and refresh cooldowns
survive restarts.

Before its first registration POST, the worker persists its intent. If that
outcome is uncertain, including a crash after EasyPost may have created a
tracker, the next attempt first looks up matching trackers and reuses the
existing one. An incomplete or ambiguous lookup never triggers another POST.
EasyPost documents duplicate reuse for the same user, number and carrier
within three months; the app stops uncertain registrations beyond that window
rather than assuming indefinite billing protection. See [EasyPost's tracker
contract](https://docs.easypost.com/docs/trackers). SQLite leases and tracking
generation fences still prevent stale responses from overwriting edits.

Manual refreshes coalesce and wait at least one minute after the last provider
attempt, respecting longer pending error backoff and earlier scheduled checks.
An explicit terminal refresh allows one further check. Repeated failed terminal
checks also respect the cooldown.

The app writes JSON logs to stderr, visible in the Home Assistant app log or
container logs. Entries include UTC timestamps, request IDs, safe route
names, outcomes, durations, and error codes. Provider failures include HTTP
status, allowlisted native provider codes, and retry timing when available.
Saved-registration events include the
API request ID and shipment ID; background attempts include their own request
ID and the same local shipment ID. Find the request ID in the `X-Request-ID`
response header or an error response's `request_id` field. Logs exclude credentials, tracking numbers,
raw URLs, request bodies, and provider messages. Successful dashboard reads
stay quiet.

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
