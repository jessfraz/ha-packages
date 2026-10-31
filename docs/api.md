# API

The Rust API is under `/v1`. The Home Assistant integration forwards it under
`/api/shipments/v1` using Home Assistant authentication. The generated contract
is available at `/openapi.json` and in the [OpenAPI schema](../openapi/api.json).

This example targets a local standalone server. For a Home Assistant app,
enable the optional network port and use the host address, as described under
[Agent access](../shipments/DOCS.md#agent-access).

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
records migrate to inbound. Direction can be edited but cannot be cleared.
There is no hard-delete endpoint; archive unwanted records.

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

| Method | Path | Operation |
| --- | --- | --- |
| GET / POST | `/v1/shipments` | List or create |
| GET | `/v1/carriers` | List supported carrier enum values |
| GET / PATCH | `/v1/shipments/{id}` | Read or edit |
| GET | `/v1/shipments/{id}/events` | Read history |
| GET | `/v1/events` | Read durable shipment status transitions |
| GET | `/v1/delivery-estimates` | Page inbound estimates and verified delivered history |
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
restarts; retrying the same request returns it. A different input with the same
key returns `409`. A normalized tracking number belongs to exactly one record,
including archived shipments, other carriers and either direction. A duplicate
create or tracking edit returns `409 tracking_duplicate` with the existing
shipment's ID before contacting a provider. Open that shipment or restore it
if archived; archiving never releases its number. Imported sources are also
unique. Concurrent admissions reserve the number across providers until saved.

Reads and writes return an `ETag` containing the shipment revision. Every write
to an existing shipment requires that value in `If-Match`. A stale edit returns
`412`; a missing condition returns `428`. For `PATCH`, omitted fields stay as
they are and `null` clears optional fields. Carrier state and audit identity
cannot be written in request bodies. Tracking cannot be removed.

List filters include `tracking_number`, `carrier`, `order_reference`,
`source_namespace` with `source_key`, `status`, `active`, `exceptions`,
`search`, `archived`, `direction`, and `sort`. Summary accepts an optional
`direction` filter; omitting it counts both directions.
`active=true` excludes delivered, returned, and cancelled shipments, using the
manual correction when one exists. Archiving is independent of status. The
dashboard opens in the active view and refreshes while the tab is visible.
Lists and history use `items` and `next_page`. Pass the token as `page_token`
to continue, with an optional `limit` of up to 200.

Carrier values are a closed enum using EasyPost's canonical tracking strings,
including `USPS`, `UPS`, and `FedEx`. `GET /v1/carriers` supplies the same
catalog used by the add-shipment dropdown. Omit carrier for automatic
detection. Older case-insensitive spellings remain readable; responses use
canonical names.

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

Admission resolves ordinary `FedEx` (case-insensitively) to `FedExDefault`,
the EasyPost Wallet route, before verification and registration. Explicit
`FedExDefault` remains unchanged. Use the input alias `FedExAccount` to select
EasyPost's separate `FedEx` route with your configured carrier account. Responses
and saved history retain the concrete provider carrier (`FedEx` for that account
route). The editor preserves that explicit choice when editing existing records.
`GET /v1/carriers` lists concrete provider identities; the input schema additionally
accepts `FedExAccount`.

UPS already supports standalone tracking under `UPS`; `UPSDAP`, DHL Express,
DHL Express Default, FedEx SmartPost and cross-border products remain distinct.
No product is guessed, and unknown carriers fail validation. Missing provider
configuration, unavailable verification and uncertain registration never trigger
a second carrier registration. Existing tracker lookup, idempotency and global
tracking-number ownership apply after input resolution. Existing shipments and
user decisions are not migrated or re-registered by this change.

`GET /v1/delivery-estimates` includes nonarchived inbound delivery estimates
and delivered history. `estimated_delivery_at` is nullable;
`estimated_delivery_window` carries explicit carrier-provided `start`/`end`
bounds separately from a point ETA. A window can exist without a point ETA.
Only paired dates or timezone-aware timestamps are accepted; raw offsets are
preserved. AfterShip supplies carrier bounds through
`courier_estimated_delivery_date.estimated_delivery_date_min/max`; predictive
AI bounds and a lone EasyPost ETA are not carrier time windows. `delivered_at`
contains the verified actual delivery timestamp when available. Delivered
shipments without a verified actual date are omitted rather than dated from
an ETA or an override's creation time. Effective user status remains authoritative;
archived, cancelled and returned shipments stay excluded. Keyset pagination
orders delivered rows by actual date and other rows by window start or estimate.

EasyPost untimed midnight-UTC ETA defaults are civil dates, even when optional
local-date metadata is absent. Explicit carrier times, non-UTC offsets and
fractional times remain instants. `carrier_tracking.estimated_delivery_raw`
retains the original provider ETA. Legacy EasyPost snapshots use the same read
projection until their next normal refresh; no scan history or user decisions
are rewritten. AfterShip timestamps, actual delivery timestamps and paired
windows are never subjected to this provider-specific date default.

The native `inbound_due_today` sensor combines inbound estimates overlapping
Home Assistant's local day with out-for-delivery parcels, deduplicated by UUID.
It exposes bounded `items` with their actual status and an honest truncation
flag. The status-only `inbound_out_for_delivery` sensor and notification
semantics remain unchanged. Calendar and expected-today selection use the same
event bounds, including date ranges, aware windows and DST boundaries.

The native calendar shows genuine aware start/end delivery windows as timed
events. Date-only bounds, default midnight bounds and lone ETAs remain all-day;
no endpoint is invented from a point estimate. Date ranges include the latest
carrier date. Delivered entries are
labeled `Delivered` and use actual delivery dates. Raw carrier timestamps stay
in event descriptions; an estimated time is not an actual arrival or a one-hour
appointment. Calendar range queries retain ended windows when they overlap the
requested range; agendas can retain today's history without changing the
window's actual times. OnTrac's latest unambiguous dated `OFDL` scan refines a
stale `in_transit` status; terminal provider statuses and newer conflicting
scans remain authoritative, as do user overrides. Raw scan fields are preserved.

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
check replaces it; elapsed time does not turn it into a failure. From 0.5.11,
the result survives restarts and upgrades in SQLite, scoped to the configured
key and endpoint.

The dashboard home opens on active inbound shipments. Select **All** or
**Outbound** to change directions. Existing filtered links retain their scope.
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
| HTTP 409 | `tracking_duplicate` | Use the existing shipment ID in the message. Restore it if archived; changing carrier or direction does not allow a second record. |
| HTTP 503 | `tracking_verification_pending` | Another request is verifying this number, or verification is backing off. Wait for `Retry-After` and retry the original request and key. |
| HTTP 400 / tracking issue | `tracking_unsupported` | The provider could not match the number to an available carrier. Select the carrier from the shipping notice and check its account setup. This does not establish that the number is invalid. |
| HTTP 400 / tracking issue | `tracking_invalid`, `tracking_carrier_mismatch`, `tracking_carrier_ambiguous` | Correct the number or specify the right carrier. |
| HTTP 400 | `tracking_recipient_url_required`, `tracking_recipient_url_invalid`, `tracking_recipient_url_mismatch` | Supply the original DoorDash recipient link matching this tracking number. The number alone is insufficient. |
| HTTP 400 / setup required | `tracking_test_key`, `tracking_carrier_account_required`, `tracking_carrier_credentials_required` | Fix the EasyPost key, select the appropriate carrier choice, or configure its account. |
| HTTP 503 / tracking issue | `provider_http`, `provider_timeout`, `provider_connection`, `provider_invalid_response` | Retry the same admission request after `Retry-After`; saved shipments retry with backoff. These errors do not establish that a number is invalid. |
| HTTP 503 / tracking issue | `recipient_page_unavailable` | The DoorDash page could not verify a recognized current status. Retry after backoff; an access refusal is not a request for carrier credentials. |
| Shipment sync | `registration_ambiguous`, `registration_expired` | Registration needs review; the app will not risk another paid registration automatically. |

EasyPost requests share a fair, bounded local queue, without artificial
one-second spacing. A local queue deadline is not an upstream HTTP 429 and
must not acquire provider-rate-limit backoff. Genuine provider rate limits
retain their Retry-After cooldown across client clones. A cancelled or timed-out
wait cannot register a tracker, and the existing admission ownership/lease and
idempotency fences still apply before registration.

Inspect `sync.provider_status`, `sync.provider_error_code`, `sync.attempts`,
and `sync.next_attempt_at` for provider failures and retry timing. Known native
provider codes are retained without storing arbitrary provider error text. Use
GET to read the current state: an idempotency replay returns the original
committed create response. Retry an uncertain local create with its original
idempotency key. Do not create a fresh record or key because a provider
response was uncertain.

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

Every admission and legacy unregistered shipment first looks for an existing
matching EasyPost tracker, including on its first attempt. A single confirmed
match is adopted without another paid registration. Only a complete, empty
lookup permits a registration request. Failed lookups preserve the verification
intent and back off without adding a shipment; ambiguous results need attention
and never fall through to registration.

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

## Amazon account management

`POST /v1/shipments/{id}/amazon` switches one existing inbound AmazonMcf
AfterShip record to verified Amazon-account tracking. It requires owner access
and the record's current `If-Match` revision. Amazon must report that order's
same carrier and tracking number. A mismatch, stale revision, archived record,
or failed check leaves the record unchanged. The action preserves its ID,
metadata, manual correction, sources, and history. It never creates a record
or enables automatic imports. Linked records continue normal tracking while
imports are off. The returned shipment includes its new `ETag`.

All `/v1/amazon` management endpoints require the owner or an authenticated Home
Assistant administrator. Agent and integration credentials cannot manage Amazon.
Safe connection state is also included in the integration-readable `/v1/system`.

- `GET /v1/amazon`: connection state, imports enabled, last success and safe error code.
- `POST /v1/amazon/sign-in`: begin an owner-bound, expiring PKCE sign-in; returns
  the one-time flow ID and the Amazon sign-in URL.
- `POST /v1/amazon/complete`: submit `flow_id` and `landing_url` in the JSON body.
  Only the expected HTTPS Amazon landing route is accepted. Never put the address
  or authorization code in a URL or log.
- `POST /v1/amazon/preview`: read verified candidate parcels and a deferred count.
  This does not create or modify shipments or mark candidates imported.
- `POST /v1/amazon/enable`: explicitly enable automatic imports after preview.
- `POST /v1/amazon/refresh`: queue bounded discovery, respecting cooldown/backoff.
- `DELETE /v1/amazon`: remove local account credentials; retain shipment data.

Connection and preview responses use `Cache-Control: no-store`. No endpoint returns
saved Amazon refresh tokens or website cookies. An expired Amazon sign-in is account
health data, not a 401 rejection of the Home Assistant integration credential.
