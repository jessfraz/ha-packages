# ha-packages

A local shipment registry for Home Assistant. Add a delivery yourself or through
the API, let its tracking provider check its progress, and correct the status
when you know better. Carrier updates never undo a manual correction.

One Rust process, a SQLite database, and a small Home Assistant adapter. Records
and history stay local. Every shipment needs a tracking number and an inbound
or outbound direction. Its provider must resolve a carrier and accept tracking
before a shipment is saved. New labels do not need a carrier scan.

## Install on Home Assistant

Install the **Shipments app** from the App Store to run the service and
dashboard. Add `https://github.com/jessfraz/ha-packages` to **HACS → Custom
repositories** as an **Integration**, download **Shipments**, and restart Home
Assistant. Confirm the discovered integration in **Settings → Devices &
services** to enable sensors and native actions. The app manages credentials
automatically.

See [installation and configuration](shipments/DOCS.md) for both installation
steps, provider configuration, and standalone Docker instructions. The app
dashboard's **Installation → Run checks** verifies the service, Home Assistant
connection, app/integration version match, authentication, read-only EasyPost
access, and backup coverage. Update both the app and HACS integration to
**0.5.6**, then restart Home Assistant.

The supervised integration adds a stable **Shipments** sidebar panel at
`/shipments`, available to ordinary Home Assistant users. Status corrections,
archiving, installation checks, and credential management still require an
administrator. A standalone installation opens its configured backend URL.

## Home Assistant entities

Existing combined counts and the inbound/outbound counts keep their entity
IDs. Counts use distinct status icons and `measurement` statistics: each value
describes the current registry. The delivered count is not a deliveries-per-week
counter. **Awaiting pickup** keeps the API key `expected` and combines expected,
unknown, and pre-transit states; an unknown carrier state does not establish
that a label exists or that pickup is pending.

**Inbound delivered today** (`sensor.shipments_inbound_delivered_today`) counts
actual carrier delivery timestamps within Home Assistant's configured local
day. It excludes outbound shipments and unknown timestamps, respects daylight
saving, and links to `#view=delivered_today&direction=inbound` in the dashboard.
The dashboard uses the same day boundaries even if your browser is elsewhere.
The sensor stays unavailable while the app and integration versions differ.

Both `sensor.shipments_inbound_delivered_today` and
`sensor.shipments_inbound_out_for_delivery` include an `items` attribute for
dashboard package lists. Their states remain counts. Each item contains `id`,
`title`, `carrier`, `status`, nullable ISO `delivered_at` and
`estimated_delivery`, and a relative `url` opening the shipment in Home
Assistant. Titles retain their stored text. Delivered items sort newest first;
out-for-delivery items sort by ETA. Lists stop at 20, with `items_truncated`
indicating additional matches. Tracking numbers are excluded, and both
attributes are excluded from recorder history.

The **Tracking issues** card opens `#view=tracking_issues`, matching the combined
sync-error count across both directions.

Two native problem entities flag delivery exceptions and synchronization
errors. The read-only **Inbound deliveries** calendar includes active inbound
shipments with a carrier ETA. Date-only estimates appear as all-day entries;
timestamp estimates use a one-hour window. Outbound, archived, and terminal
shipments, and missing or invalid estimates, do not appear. These are carrier
estimates, not delivery appointments.

The native shipment event entity emits `out_for_delivery`, `delivered`, and
`exception` transitions from durable backend events. Attributes include
`event_id`, `shipment_id`, `title`, `carrier`, `direction`, `status`, and `at`.
Initial setup starts at the current event tail rather than replaying history.
The integration saves its checkpoint across reloads and restarts. Delivery is
at least once: use `event_id` to deduplicate automations when a restart occurs
between emission and checkpoint persistence.

## Run standalone

```sh
nix develop
export SHIPMENTS_API_TOKEN="$(openssl rand -hex 32)"
export SHIPMENTS_INTEGRATION_TOKEN="$(openssl rand -hex 32)"
export EASYPOST_API_KEY="your-easypost-api-key"
cargo run
```

Open `http://127.0.0.1:8080` and enter the token. The browser keeps it in memory
for that tab. The default database is `shipments.sqlite3` in the working
directory. Set `SHIPMENTS_DATABASE` to change it.
Use the separate `SHIPMENTS_INTEGRATION_TOKEN` when pairing Home Assistant;
the owner API token is for the direct dashboard and administrative clients.
The tokens must be distinct and each contain 32–256 printable ASCII characters
without whitespace. The integration token is optional when running without
Home Assistant.

`EASYPOST_API_KEY` enables EasyPost verification and background updates.
`AFTERSHIP_API_KEY` enables tracking for **SpeedX**, **Sway / Returnmates**, and **Veho**
through AfterShip. Select those carriers explicitly; they do not fall back to
EasyPost. **DoorDash** uses its original recipient tracking link and requires
no API key. Other carriers use EasyPost, including automatic carrier detection.
Creates and tracking edits require successful provider verification. A missing
key for the selected provider, failed detection, or provider outage saves no
new shipment and leaves an existing tracking identity unchanged. Tracker
registration can consume provider quota or incur fees, and some carriers
require their own accounts and credentials. The
provider key belongs to the server alone.

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
records migrate to inbound. Direction can be edited but cannot be cleared. There
is no hard-delete endpoint; archive unwanted
records.

Create returns `201` only after provider verification and an atomic commit of
the shipment, history and idempotency response. The response has
`sync.registration_state: "registered"` and a canonical, required carrier.
Omitting the input carrier requests auto-detection; failed detection saves
nothing. Unknown and pre-transit statuses are accepted without scans.
Tracking-number edits use the same verification, preserving the original
shipment and history if verification fails.

Verification intent is stored separately from shipments before network I/O.
If a provider response is lost, retry the normal endpoint with the same input
and idempotency key. The app checks for an existing tracker before registering,
including after restart. It returns `503` with `Retry-After` while verification
is unavailable or backing off; a retry never bypasses provider verification.
There is no separate recovery endpoint.

| Method | Path | Operation |
| --- | --- | --- |
| GET / POST | `/v1/shipments` | List or create |
| GET | `/v1/carriers` | List supported carrier enum values |
| GET / PATCH | `/v1/shipments/{id}` | Read or edit |
| GET | `/v1/shipments/{id}/events` | Read history |
| GET | `/v1/events` | Read durable shipment status transitions |
| GET | `/v1/delivery-estimates` | Page active inbound delivery estimates |
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

For DoorDash, select `"carrier":"DoorDash"` and supply
`tracking.recipient_url`, the original HTTPS link from the shipping email:

```json
{
  "title": "Example parcel",
  "direction": "inbound",
  "tracking": {
    "carrier": "DoorDash",
    "number": "YOUR_TRACKING_NUMBER",
    "recipient_url": "https://www.doordash.com/orders/drive?urlCode=ORIGINAL_LINK_CODE"
  }
}
```

Copy the whole original link. Its opaque `urlCode` cannot be derived from the
tracking number. Other hosts, paths, fragments, credentials and additional
query parameters are rejected. The app fetches that fixed recipient route
without authentication or JavaScript, verifies the exact tracking number and
recognized current status, and stores the original link for refreshes. A new
link or tracking-number edit is verified before replacing an existing one.
DoorDash tracking makes only read requests; it creates no remote tracker.
Current status and actual delivery time are supported when provided by the
page. Detailed scan history is unavailable. Missing, changed or unrecognized
page data fails verification; an estimate never becomes an actual delivery.
Treat recipient tracking links as private shipment data.

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
Ingress administrator access also requires a current authenticated integration
check-in listing that active administrator. The list expires after three
minutes; missing or stale check-ins deny privileged actions.

An ingestion agent should look up a tracking or source reference, create or
patch the record, and attach new evidence. The app handles tracking afterward.
An email claiming delivery is evidence, not a human confirmation.

Malformed or empty tracking is rejected at the API before saving. API errors
include `error_code`, a readable `message`, and a request ID. Creation failures
save no shipment; existing tracking failures appear on
the shipment with the same safe error codes:

| Where | Code | Action |
| --- | --- | --- |
| HTTP 400 | `tracking_required` | Supply a tracking number; tracking cannot be cleared. |
| HTTP 400 | `tracking_invalid` | Check the number's format; unsupported carrier values are also rejected by the enum. |
| HTTP 400 / tracking issue | `tracking_unsupported` | The provider could not match the number to an available carrier. Select the carrier from the shipping notice and check its account setup. This does not establish that the number is invalid. |
| HTTP 400 / tracking issue | `tracking_invalid`, `tracking_carrier_mismatch`, `tracking_carrier_ambiguous` | Correct the number or specify the right carrier. |
| HTTP 400 | `tracking_recipient_url_required`, `tracking_recipient_url_invalid`, `tracking_recipient_url_mismatch` | Supply the original DoorDash recipient link matching this tracking number. The number alone is insufficient. |
| HTTP 400 / setup required | `tracking_test_key`, `tracking_carrier_account_required`, `tracking_carrier_credentials_required` | Fix the EasyPost key, select the appropriate carrier choice, or configure its account. |
| HTTP 503 / tracking issue | `provider_http`, `provider_timeout`, `provider_connection`, `provider_invalid_response` | Retry the same admission request after `Retry-After`; saved shipments retry with backoff. These errors do not establish that a number is invalid. |
| HTTP 503 / tracking issue | `recipient_page_unavailable` | The DoorDash page could not verify a recognized current status. Retry after backoff; an access refusal is not a request for carrier credentials. |
| Shipment sync | `registration_ambiguous`, `registration_expired` | Registration needs review; the app will not risk another paid registration automatically. |

Inspect `sync.provider_status`, `sync.provider_error_code`, `sync.attempts`,
and `sync.next_attempt_at` for provider failures and retry timing. Known native
provider codes are retained without storing arbitrary provider error text. Use
GET to read the current state: an
idempotency replay returns the original committed create response, which
reflects the original committed state. Retry an uncertain local
create with its original idempotency key. Do not create a fresh record or key
because a provider response was uncertain.

List and summary queries accept paired `delivered_after` (inclusive) and
`delivered_before` (exclusive) UTC epoch seconds. These filter actual carrier
delivery timestamps, independently of manual status corrections. Use
`direction=inbound` for incoming deliveries. The list also supports
`tracking_issues=true`, filtering stored tracking failures before pagination.

### Retry old failed attempts

Versions before 0.4.0 could create an EasyPost tracker, then lose its response
without saving the shipment or idempotency key. Submit those parcels through
normal creation with a stable idempotency key and the correct canonical
carrier, or omit the carrier when it is uncertain.

Every admission and legacy unregistered shipment first looks for an existing matching EasyPost
tracker, including on its first attempt. A single confirmed match is adopted
without another paid registration. Only a complete, empty lookup permits a
registration request. Failed lookups preserve the verification intent and back
off without adding a shipment; ambiguous results need attention and never fall
through to registration.

Lookup and duplicate protection cover the previous three calendar months for
the selected carrier and tracking number. They do not establish that every
older tracker or a tracker under another carrier is unrelated. Review the
provider inventory for historical attempts outside that window or uncertain
carrier choices. Retry the same local create key and read the saved shipment's
current state rather than generating new keys after an uncertain response.

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
schedule. Missing carrier accounts, credentials, test keys, and rejected provider
credentials stop automatic retries and show **Setup required**. Configure the
provider, then select **Refresh tracking** to retry explicitly. Terminal carrier
states and terminal manual corrections stop polling.

All EasyPost operations share one outbound slot, with at least one second
between request starts. Rate limits pause registration, polling and connection
checks together. New shipments wait for verified tracking before they can be saved. Provider
cooldowns honor bounded `Retry-After` values with exponential backoff. The
shared pause lasts for the process; shipment retries and refresh cooldowns
survive restarts.

Before any registration POST, admission or the worker persists its intent. If that
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

Upgrades remove legacy records missing a carrier or tracking number, together
with their sources, history and create replays. Complete records are retained.
The DoorDash recipient-tracking migration also removes legacy DoorDash records
without their required original recipient link. Re-add those shipments with
the original link if needed.

The app writes JSON logs to stderr, visible in the Home Assistant app log or
container logs. Entries include UTC timestamps, request IDs, safe route
names, outcomes, durations, and error codes. Provider failures include HTTP
status, allowlisted native provider codes, and retry timing when available.
Saved-registration events include the
API request ID and shipment ID; background attempts include their own request
ID and the same local shipment ID. Find the request ID in the `X-Request-ID`
response header or an error response's `request_id` field. Logs exclude
credentials, tracking numbers,
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

Tracking backends implement `ShippingApi` in `src/shipping_api/mod.rs`.
`easypost.rs` owns EasyPost authentication, wire formats, carrier encoding,
tracker validation, shared request pacing and registration recovery. Shared
observations and errors live in `types.rs` and `error.rs`. The worker uses the
trait and stores provider ownership with each tracking identity.

Reconciliation must establish a confirmed match or authoritative absence.
Before a provider write, the implementation reserves its request slot, the
worker verifies the shipment's lease and generation, and the reservation
checks its provider's recovery deadline before sending. Dropping a reservation
cancels it without a write. Providers must share pacing across request-scoped
clients. Never try a different provider automatically after an uncertain
registration outcome.

`aftership.rs` owns AfterShip authentication, carrier slugs and tracker
reconciliation for SpeedX, Sway, and Veho. `doordash.rs` implements read-only
recipient tracking, with a fixed URL allowlist, response and transport bounds,
typed observations and no JavaScript execution. Its HTML hydration parser is
a website adapter, not a stable public API contract; changed or ambiguous data
must fail closed. Each provider shares pacing across its own scoped clients.
Additional carriers require verified coverage before routing them to a
provider. Amazon Logistics coverage remains unconfirmed.

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
