# Architecture

The Rust service owns shipment state, provider access, and tracking updates.
SQLite stores shipments, history, credentials, and registration intent. The
Python integration exposes that state in Home Assistant.

## Tracking providers

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
provider. Consumer-account Amazon parcels use Amazon Account rather than the
MCF provider route.

## Storage and upgrades

SQLite transactions commit the record, history snapshot, and idempotency
response together. Duplicate checks hold the SQLite writer lock. History
preserves previous values as well as who changed them.

Upgrades remove legacy records missing a carrier or tracking number, together
with their sources, history and create replays. Complete records are retained.
The DoorDash recipient-tracking migration also removes legacy DoorDash records
without their required original recipient link. Re-add those shipments with
the original link if needed. Back up before upgrading.

Tracking numbers are globally unique from 0.5.9. This migration preserves all
records and history. If a legacy database contains duplicate numbers, upgrade
stops atomically instead of choosing a record to delete. Resolve those records
before retrying the upgrade.

## Polling

The worker claims due records using persisted leases, makes its request outside
the transaction, and checks the lease and tracking generation before saving.
An old response cannot overwrite a corrected tracking number or archived
record. Failed requests retain the local record and retry with backoff from
one minute to six hours. Ordinary polling is every 30 minutes, or five minutes
when out for delivery, with up to 10% added per-shipment jitter. Unknown and
pre-transit labels that keep the same status slow to two-hour checks after two
days and six-hour checks after a week. A status change restores its normal
schedule. Missing carrier accounts, credentials, test keys, and rejected
provider credentials stop automatic retries and show **Setup required**.
Configure the provider, then select **Refresh tracking** to retry explicitly.
Terminal carrier states and terminal manual corrections stop polling.

All EasyPost operations share one outbound slot, with at least one second
between request starts. Rate limits pause registration, polling and connection
checks together. New shipments wait for verified tracking before they can be
saved. Provider cooldowns honor bounded `Retry-After` values with exponential
backoff. The shared pause lasts for the process; shipment retries and refresh
cooldowns survive restarts.

Manual refreshes coalesce and wait at least one minute after the last provider
attempt, respecting longer pending error backoff and earlier scheduled checks.
An explicit terminal refresh allows one further check. Repeated failed terminal
checks also respect the cooldown.

## Registration recovery

Before any registration POST, admission or the worker persists its intent. If
that outcome is uncertain, including a crash after EasyPost may have created
a tracker, the next attempt first looks up matching trackers and reuses the
existing one. An incomplete or ambiguous lookup never triggers another POST.
EasyPost documents duplicate reuse for the same user, number and carrier
within three months; the app stops uncertain registrations beyond that window
rather than assuming indefinite billing protection. See [EasyPost's tracker
contract](https://docs.easypost.com/docs/trackers). SQLite leases and tracking
generation fences still prevent stale responses from overwriting edits.

## Assist

The integration's `voice.py` validates bounded database projections and builds
typed answers for shipment queries. `intent.py` owns the native intent schema;
`llm.py` contributes Home Assistant's standard `IntentTool` for that same
handler. Native sentence triggers are registered through Home Assistant's
public conversation API and removed with the config entry.

The registered inbound-active sensor acts as the Assist exposure gateway.
Queries resolve it by unique ID and check exposure and caller read permission
before reading, then recheck after the response to handle unloads or revoked
access. Satellite requests without a user still require exposure. Only bounded
safe fields reach speech or AI tools; provider access remains with the Rust
worker. A voice query never requests a tracking refresh.

## Logs and backups

The app writes JSON logs to stderr, visible in the Home Assistant app log or
container logs. Entries include UTC timestamps, request IDs, safe route
names, outcomes, durations, and error codes. Provider failures include HTTP
status, allowlisted native provider codes, and retry timing when available.
Saved-registration events include the API request ID and shipment ID;
background attempts include their own request ID and the same local shipment
ID. Find the request ID in the `X-Request-ID` response header or an error
response's `request_id` field. Logs exclude credentials, tracking numbers,
raw URLs, request bodies, and provider messages. Successful dashboard reads
stay quiet.

Home Assistant uses cold backups: the app stops while its persistent `/data`
directory is copied. For standalone operation, stop the service before copying
the database, or use SQLite's online backup facility. Do not copy only the main
database file while a writer is running in WAL mode. Backups preserve saved
agent access and revocations at the time of the backup.

## Amazon account discovery

`src/amazon` owns the Amazon.com retail device sign-in, private credential file,
cookie session and bounded website adapters. `AmazonClient` implements the same
`ShippingApi` contract as other tracking providers. An account-scoped opaque
tracker ID binds the saved package page to the authenticated consumer account.
Amazon-handled deliveries use the distinct **Amazon Account** tracking product;
USPS, UPS, and other named couriers retain their own carrier identities. Amazon
Account is not Amazon MCF or Amazon Shipping and never uses their provider routes.

`AmazonDiscoveryWorker` runs in the existing provider-worker lifecycle and uses
the shared base polling interval, jitter and retry calculation. Each batch reads
at most three listing pages and five unresolved parcel groups, retaining an
account-scoped bounded sweep of recent orders. Mobile `chunkState` continuation
uses only its validated numeric start index, never a continuation token. Pending
batches continue no sooner than one minute later; a completed sweep returns to
the normal discovery interval. Verified progress survives a later transient
parcel failure, and failed leaves rotate behind unvisited work after backoff.
The normal `SyncWorker` fetches a known parcel's saved page at the
shared status-based cadence. Account listing is not repeated for each saved parcel.

Imports start disabled. The owner-only preview endpoint performs reads without
writing shipment data. Enabling imports schedules discovery. Real parcel identity
and product mapping are required; missing tracking numbers never become synthetic
barcodes. Generated titles are bounded display text; their length does not
invalidate verified parcel identity. Verified source identity and shipment
updates commit atomically. Global
tracking-number uniqueness deduplicates an existing parcel, including archived
records, without replacing user metadata or an existing last-mile provider.
Different tracking numbers are not merged without explicit parcel evidence.

Completed discovery sweeps are scheduled every 30 minutes with up to three minutes
of stable jitter. Discovery continues when all saved parcels are delivered; terminal
parcels themselves stop polling. Failures honor the shared retry/backoff policy.

Account replacement invalidates in-flight work. Discovery holds the account
generation through each short database commit; normal sync uses the provider's
observation guard as well as its shipment lease/generation fences. No network call
holds a SQLite transaction. Metadata-only edits preserve tracking-provider ownership.

The authenticated system report exposes only safe account connection state.
Home Assistant creates one persistent reconnect notification on confirmed Amazon
authentication rejection, retains it through outages, and clears it after reconnect
or explicit disconnect. Amazon expiry does not invalidate Home Assistant pairing.
The notification deep link opens settings without carrying codes or credentials.

The adapter follows pinned ha-amazon/alexapy source with MIT and Apache-2.0
attribution in NOTICE. Website layouts are deliberately narrow; unsupported or
changed pages are deferred rather than guessed into shipment identities.

Amazon promise wording is parsed at ingestion into typed civil dates and a typed
observation timestamp. A delivered month/day uses the most recent plausible
year; estimates choose the nearest plausible year, including overdue dates and
December/January boundaries. Relative dates use the provider's IANA timezone.
Explicit ranges remain ranges. Calendar dates do not imply midnight delivery
scans, and unknown wording does not become a fabricated date. The dashboard
formats these backend dates without interpreting provider prose.
