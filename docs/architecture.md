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
provider. Amazon Logistics coverage remains unconfirmed.

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
