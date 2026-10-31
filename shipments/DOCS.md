# Installation

Home Assistant 2026.10 or newer is required. Install both parts:

- The **Shipments app** runs the shipment service, database, and dashboard.
- The **Shipments integration**, installed through HACS, adds sensors and
  native Home Assistant actions.

Use the same repository URL in the App Store and HACS.

## Start the app

Add `https://github.com/jessfraz/ha-packages` in **Settings → Apps → App store →
Repositories**, then install **Shipments**. On older Home Assistant versions,
these settings are named **Add-ons**.

Configure the provider keys you need, then start the app. Shipments and
tracking edits are verified with the selected provider before they are saved;
the app updates their tracking in the background. It creates and saves its
own credentials automatically.

Open the web UI from the app page, or enable **Show in sidebar**. Home
Assistant authenticates the dashboard through ingress. Server credentials
stay out of the browser.

## Install the integration through HACS

If needed, [install HACS](https://www.hacs.xyz/docs/use/download/download/)
first.

1. Open **HACS**, select the **⋮** menu, then **Custom repositories**.
2. Enter `https://github.com/jessfraz/ha-packages`, choose **Integration**, and
   select **Add**.
3. Find **Shipments** (`jessfraz/ha-packages`) and select **Download**.
4. Restart Home Assistant after the download.
5. Open **Settings → Devices & services** and confirm the discovered
   **Shipments** integration.

Shipments includes local brand icons for light and dark themes, including
high-resolution displays. Restart Home Assistant after updating the integration
to load them. HACS 2.0.5 still uses the old brand CDN for its store tile and may
show a placeholder there; the integration icon in Home Assistant and the icon
in the HACS details page are bundled with Shipments. This is an
[upstream HACS limitation](https://github.com/hacs/frontend/pull/937).

The app shares a separate integration credential through authenticated
Supervisor discovery. Pairing does not require pasting a URL or token.
Internal app networking does not require a host port mapping.

The app's **Show in sidebar** setting controls the Shipments sidebar entry.
The integration retains a hidden `/shipments` route for existing shipment,
calendar, and notification links, without adding another sidebar entry.
Ordinary Home Assistant users can open the dashboard; administrator-only
actions still enforce administrator access. The device's **Visit** link uses
this route for supervised apps. A standalone installation uses its configured
backend URL instead.

If there is no discovered card, choose **Add integration → Shipments → Connect
the Home Assistant app**. Start the app if prompted, then select **Submit** to
find it and confirm the connection.

HACS explains [custom repositories](https://www.hacs.xyz/docs/faq/custom_repositories/)
and [integration installation](https://www.hacs.xyz/docs/use/repositories/type/integration/)
in its documentation.

## Check the installation

After a restart, the Home Assistant dashboard waits for its authenticated
connection and retries automatically. Leave it open; no API token is needed.
If it stays on the connecting screen, check the Shipments integration in
**Settings → Devices & services**. The standalone dashboard still requires
its API token.

Open the app dashboard, expand **Installation**, and select **Run checks**.

| Check | What it verifies |
| --- | --- |
| App and database | The service is running and SQLite responds. |
| Home Assistant | The integration has checked in recently. |
| App / integration versions | A recent check-in reports the same app and integration version. |
| Authentication | Home Assistant authenticated with the app. |
| EasyPost | The configured key permits a read-only tracking request. |
| Backup | A recorded backup includes the app and Home Assistant configuration. |

The Home Assistant checks use a recent authenticated integration check-in.
If either needs attention, finish the HACS installation, restart Home
Assistant, and confirm the connection in **Devices & services**. If already
paired, check the integration for a connection or authentication error.

Version mismatch is a separate warning: connectivity and authentication can
pass while the two installed versions differ. Update both the app through the
App Store and the integration through HACS to the **same release**, then restart Home
Assistant. Without a recent integration check-in, the version match is
unverified.

**Run checks** makes a read-only EasyPost request when a key is configured;
requests are rate-limited to once per minute. A successful result stays passed
until a later check actually fails. The result and timestamp are saved in
SQLite and survive app restarts and upgrades. Changing the key or endpoint
requires a new check; credentials are never stored with the diagnostic.
Older versions did not save the result. Run checks once after upgrading to
0.5.11 to record it; later upgrades retain that record. Without an EasyPost
key, EasyPost carriers cannot be verified or saved; DoorDash and configured
AfterShip carriers still
use their own providers.

The backup check shows the recorded backup time. It requires a backup made
after pairing. An older, app-only, or missing backup needs attention; an
unavailable backup inventory is reported as
**Unavailable**. Include both **Shipments** and **Home Assistant** when
creating a backup in **Settings → System → Backups**. The check confirms backup
inventory and coverage; restore testing is a separate step.

## Diagnostics

Open **Settings → Devices & services → Shipments**, select the integration's
menu, then **Download diagnostics**. The native Home Assistant report includes
connection health, the last observed app version, the integration version,
refresh settings, and cached shipment counts. Unavailable or stale data is
identified rather than presented as a fresh zero.

Diagnostics use the integration's existing observations without polling
carriers. They exclude backend URLs, credentials, package titles, tracking
numbers, provider tracker IDs, source records, and raw error messages. Review
any downloaded diagnostic before sharing it.

## Notification blueprints

The [blueprint catalog](../blueprints/README.md) has import links for:

- **Delivery updates:** out for delivery, delivered, and delivery exceptions,
  with inbound/outbound selection and a direct link to the package.
- **Tracking problems:** combined tracking errors that persist for five
  minutes, with a link to the affected shipments.
- **Daily delivery summary:** inbound packages out for delivery and packages
  actually delivered today, at a time you choose.

Choose a Home Assistant Companion app device when creating an automation.
Importing a blueprint does not enable notifications. Blueprints use existing
entities; they do not register trackers, refresh providers, or require an AI
conversation agent.

## Sensors

The integration keeps the existing counts combined across inbound and outbound
shipments, including **Exceptions** and **Sync errors**. It also adds:

| Sensor key | Counts |
| --- | --- |
| `inbound_active` | Active packages you receive. |
| `inbound_due_today` | Incoming parcels expected today or out for delivery, with deduplicated items and original statuses. |
| `inbound_out_for_delivery` | Incoming packages out for delivery. |
| `inbound_delivered_today` | Incoming packages delivered during Home Assistant’s local day, using actual carrier delivery timestamps. |
| `inbound_exceptions` | Incoming packages with a delivery exception. |
| `outbound_active` | Active packages you send. |
| `outbound_exceptions` | Outgoing packages with a delivery exception. |

By default, their entity IDs are `sensor.shipments_` followed by the sensor key,
for example `sensor.shipments_inbound_out_for_delivery`. Use the direction
sensors in dashboard tiles and automations to distinguish incoming deliveries
from outgoing packages or returns. Counts refresh together every minute.

All count sensors have `measurement` state class and distinct status icons.
They are gauges of the current unarchived registry, not cumulative counters.
For example, **Delivered** is not the number delivered this week: archiving a
delivered shipment reduces that count. Keep the existing `expected` sensor
entity ID; its visible name is now **Awaiting pickup**. That API aggregate
includes expected, unknown, and pre-transit shipments. An unknown tracking state
does not prove the carrier has created a label or is waiting for pickup.

**Inbound delivered today** excludes outbound shipments and unknown delivery
timestamps. Its day starts at midnight in Home Assistant's configured timezone,
including daylight-saving changes. The sensor stays unavailable while the app
and integration versions differ. The dashboard's **Delivered today** filter
uses the same window: `#view=delivered_today&direction=inbound`. The linkable
**Tracking issues** card (`#view=tracking_issues`) matches the combined sync-error
count. Failed carrier setup remains visible until explicitly retried after
configuration.

### Package lists in dashboard sensors

`sensor.shipments_inbound_delivered_today` and
`sensor.shipments_inbound_out_for_delivery` keep count states and also expose:

| Attribute | Meaning |
| --- | --- |
| `items` | Up to 20 matching inbound shipments; an empty list when none match. |
| `items_truncated` | `true` when more than 20 shipments match. |

Each item has `id`, `title`, `carrier`, `status`, `delivered_at`,
`estimated_delivery`, and `url`. Dates are ISO strings or `null` when unknown.
Titles retain their stored text. The relative URL opens that shipment through
the hidden Shipments compatibility route, for example
`/shipments/shipments/shipment%3D0193c69f-e5c4-7e93-b6cb-8172dacb617e`.

Today's list uses the count's exact Home Assistant local-day bounds and sorts
by actual delivery time, newest first. The out-for-delivery list sorts by ETA.
Unknown ETAs sort last. Each refresh adds one bounded list request per sensor.
If counts and items cannot form a complete snapshot, the sensors become
unavailable rather than publish mismatched details. The existing mixed-version
rule still applies to the delivered-today sensor.

Tracking numbers are not included in these attributes. Both `items` and
`items_truncated` are excluded from the Home Assistant recorder, so changing
package details does not accumulate in its history database.

The integration also creates native problem entities:

| Default entity ID | On when |
| --- | --- |
| `binary_sensor.shipments_exceptions` | At least one unarchived shipment has a delivery exception. |
| `binary_sensor.shipments_sync_errors` | At least one unarchived shipment has a synchronization error. |

Transport failures make coordinated entities unavailable; they do not turn an
unknown problem state into a healthy one.

## Delivery calendar

`calendar.shipments_inbound_deliveries` is a read-only calendar of inbound
estimated delivery days, explicit carrier windows and verified delivered
history.
A genuine timezone-aware carrier start/end window stays timed, such as 12–2.
A date-only estimate or a lone ETA stays all-day: one timestamp never invents a
delivery slot. Date-only ranges include the latest carrier date; default
midnight bounds do not assert a real time slot. Original boundaries and raw
carrier timestamps remain in event details, including timezone offsets and DST
changes.
Only the provider's explicit carrier bounds qualify; the app cannot recover a
window that its tracking provider does not expose. Delivered entries
are labeled **Delivered** and use the verified actual delivery date, never an
ETA or the time a manual override was saved. A delivered shipment without a
verified actual date cannot be dated truthfully and is omitted until that gap is
resolved. Archived, cancelled and returned shipments remain excluded.

Click a Shipments event in Home Assistant's native calendar or calendar card
to open that exact shipment. The integration uses the hidden Shipments route
on supervised installations, even when **Show in sidebar** is off, and the
configured app URL for standalone setups.
Update the integration, restart Home Assistant, and reload the page to load
the calendar link support.

Dashboard cards such as Calendar Card Pro can use this same standard calendar
entity. Install and configure them separately through HACS; Shipments does not
bundle a calendar card. Calendar Card Pro currently supports card-wide actions
rather than a different destination for each event, so the native calendar
click behavior does not extend to its event rows. To retain genuine windows
after they end today, keep the agenda's start date at today and enable
`show_past_events`; do not flatten timed windows or extend their end times.

## Ask Assist

Expose `sensor.shipments_inbound_active` under **Settings → Voice assistants →
Expose** to enable named shipment answers. Renaming the entity is supported.
This enables access to shipment details, including outbound shipments when
explicitly requested. Removing its exposure disables shipment queries.

With Home Assistant's built-in conversation agent, ask:

- "What packages are arriving today?"
- "Which shipments are coming tomorrow?"
- "What's out for delivery?"
- "What arrived today?"
- "When is my Office supplies arriving?"
- "Where is my shipment Office supplies?"
- "Are there any tracking problems?"

Questions default to inbound shipments. Say **outbound** or **all** to select
other directions, for example "Which outbound shipments were delivered
today?" A title lookup searches unarchived shipment titles. If several match,
Assist asks for a more specific title.
Built-in phrases and spoken responses currently use English.

"Arriving today" includes active carrier estimates for today's local date
and packages currently out for delivery. Estimates are identified as estimates.
"Delivered today" uses actual carrier delivery timestamps, not estimates or
manual corrections. All day boundaries use Home Assistant's configured
timezone, including daylight-saving changes.

AI conversation agents using Home Assistant's **Assist / Device control** API
also receive the `shipments__query` tool. It uses the same validated intent
handler as local Assist, with structured results and links to each shipment.
For sentence triggers to run before an external agent, enable **Prefer
handling commands locally** in that assistant's settings.

Answers name up to five shipments and say when there are more. Each backend
list read is bounded to 200 records; incomplete results are marked explicitly.
Queries read the app's synchronized database and never refresh or register
carrier trackers. Tracking numbers, source records, credentials, and raw
provider errors are excluded from voice and tool responses. An unavailable
backend or mismatched app/integration release produces an unavailable answer,
not a zero count.

## Shipment events

Use the native `event.shipments` entity for `out_for_delivery`, `delivered`,
and `exception` transitions. Its attributes contain:

| Attribute | Meaning |
| --- | --- |
| `event_id` | Stable backend event ID for deduplication. |
| `shipment_id` | Shipment UUID. |
| `title` | Shipment title when the transition occurred. |
| `carrier` | Carrier, or `null` when unknown. |
| `direction` | `inbound` or `outbound`. |
| `status` | The effective shipment status. |
| `at` | Backend transition timestamp. |

Events come from committed backend transitions, rather than changes in summary
counts. On first setup the integration records the current event tail and
does not replay existing history. A per-entry checkpoint persists across
reloads and restarts. Each page is emitted before its checkpoint is saved, so
delivery is at least once: a restart in that interval can repeat an event.
Deduplicate consequential automations using `event_id`.

Default entity IDs can change if another entity already uses the name or you
rename an entity in Home Assistant.

## Settings

| Option | Purpose |
| --- | --- |
| `easypost_api_key` | Enables tracking verification before saving and background tracking updates. |
| `aftership_api_key` | Enables Amazon MCF, SpeedX, Sway / Returnmates, and Veho tracking through AfterShip. |

DoorDash requires its original recipient tracking link rather than an API key.
Other carriers and automatic carrier detection use EasyPost. Provider keys
stay in the app configuration; clients and agents do not receive them.

## Add or edit a shipment

Every shipment needs a tracking number and an explicit **Direction**:
**Inbound** for packages you receive, **Outbound** for packages you send.
Existing records migrate to inbound. The default list shows active inbound
shipments. Select **All** or **Outbound** to change the direction filter.
Leave **Carrier** on **Auto-detect**, or
choose its canonical EasyPost carrier when detection is ambiguous.
Select **Amazon MCF**, **SpeedX**, **Sway / Returnmates**, or **Veho** explicitly to use
AfterShip.
Use **Amazon MCF** only when the original merchant evidence identifies
Multi-Channel Fulfillment or supplies a matching Swiship parcel link. It uses
AfterShip `amazon`, not `amazon-mcf`, `amazon-order`, or the UK Swiship product.
Existing **AmazonShipping** and **AmazonMws** choices retain their EasyPost routes. A
TBA prefix alone does not establish the carrier product. AfterShip account
entitlements and tracking quotas still apply; this does not enable all carriers.
Configure the AfterShip key in the app; these carriers do not fall back to
EasyPost if verification fails. Existing Veho shipments migrate automatically:
the next update reconciles AfterShip imports before registering a tracker.
History is retained; shipments do not need to be recreated.
For FedEx, the **FedEx Default (EasyPost account)** choice uses EasyPost's
Wallet route. **FedEx (your carrier account)** requires separately configured
FedEx credentials. If the provider reports `CREDENTIALS_NOT_FOUND`, correct
that choice or configure the carrier account; retrying does not fix missing
credentials or mean that the number is invalid.

For **DoorDash**, also paste the **DoorDash tracking link** from the merchant's
shipping email. It looks like
`https://www.doordash.com/orders/drive?urlCode=...`. The opaque link code is
different from the tracking number; copying the number alone is insufficient.
The app verifies that the page matches the exact number and reports a
recognized status before saving. The original link remains available in the
shipment details and editor. No DoorDash account, API key, label or paid
tracker registration is needed. Keep the link private.

DoorDash supplies current status and delivery dates through this recipient
page. Detailed scan history is unavailable. The app never runs page scripts
or treats an estimate as an actual delivery. A missing or changed page reports
a clear error and uses normal retry backoff; carrier credentials will not fix
a page-access refusal.

Select **Save shipment** to verify tracking and save the accepted shipment.
Auto-detection must return a supported carrier, which is always stored with the
tracking number. New labels can be accepted before their first scan. Failed
detection or verification shows a clear error and saves no shipment; a failed
tracking edit preserves the original tracking identity.

A tracking number can belong to only one shipment, including archived records
and other carriers or directions. Duplicate creates and tracking edits show the
existing shipment's ID. Open or restore that shipment instead. Spaces and letter
case do not create a different number; carrier corrections on the existing
shipment are allowed.

The app checks for an existing matching tracker before registration, including
old failed creates that were never saved locally. Durable verification intent
survives timeouts and restarts. Retry the same request and idempotency key after
the displayed backoff; only a complete empty lookup permits a new registration.
Missing carrier accounts or credentials show **Setup required** and stop
background retries. Configure the carrier in EasyPost, then select **Refresh
tracking**. A tracking number cannot be removed.

On upgrade, legacy records missing either a carrier or tracking number are
removed with their sources, history and create replays.
The recipient-tracking migration also removes DoorDash records missing the
original tracking link. Re-add those parcels with their original link if needed.
Upgrades to 0.5.9 preserve complete records and history. If duplicate tracking
numbers already exist, upgrade stops without deleting them so they can be
resolved explicitly.

Open a shipment for tracking links, delivery dates, signature information and
carrier scan history. Action buttons and manual corrections follow history.
Marking a shipment delivered, returned to sender, or cancelled clears its
tracking issues and stops automatic checks. Removing the correction resumes
tracking. The upgrade also clears stale issues on existing manually completed
shipments, while preserving carrier history and tracker ownership.
Sources are collapsed at the bottom. Direction, status filters, search, shipment
details and open settings have URL fragments for links and browser navigation.

## Existing Amazon shipments

**Amazon Account** uses your connected consumer Amazon.com account. It is separate
from **Amazon MCF** on AfterShip and **AmazonShipping** on EasyPost. Previously
linked account shipments are corrected in place without losing their registration,
status, sources, or history. Genuine MCF records are unchanged.

With Amazon connected, open an existing inbound Amazon shipment on AfterShip
and select **Use Amazon Account tracking**. Shipments checks that order on Amazon and
requires the exact order and tracking number and a verified Amazon Account carrier
before replacing the legacy MCF label. Its ID, title, sources, corrections, and
history stay intact.

This action does not enable automatic imports or create other shipments. Linked
shipments use normal tracking updates even while automatic imports are off.
Archived records are not changed. A failed check leaves the original record
untouched; a verification challenge pauses Amazon checks and retains sign-in.

To find new shipments, enable **Automatic imports** under **Amazon Account**.
Discovery runs every 30–33 minutes, even if all existing shipments are delivered.
Only verified parcels are added; existing tracking numbers and parcel identities
are deduplicated in place. Provider backoff and verification challenges pause
requests rather than causing rapid retries.

## Agent access

Open **Agent access** in the app dashboard. Enter an **Agent name** and select
**Create credential** for each ingestion agent. Use **Copy token** to save the
token in your password manager, then select **Done**. It is shown only once.
Select **Revoke** beside an active credential to remove its access. If a token
is lost, revoke that credential and create another.

An agent can read, add, and edit shipments, attach evidence, and request
tracking updates. It cannot make manual status corrections, archive records,
run installation checks, or manage credentials. Credential hashes are saved
in SQLite and survive restarts.

Ingestion clients can use the [API error codes and retry
guidance](../docs/api.md#agents-and-human-corrections)
to distinguish invalid input from a provider or configuration failure.

For clients on another machine, enable the app's optional `8080/tcp` network
port and use the Home Assistant host address. All requests to that port
require a bearer token. Keep it on a trusted network, or put TLS in front of
it before remote access. Ingress port 8099 is internal to Home Assistant.

The dashboard shows estimated delivery dates, actual delivery dates, and
available carrier scan history. Status cards filter the list. Signature
requirements can be recorded when known from a shipping notice; missing
carrier information remains unknown. EasyPost signature proof is shown
separately from the requirement.

Healthy installation checks and agent access appear below the shipment list.
Installation moves above it when a check needs attention. The EasyPost row
shows the last diagnostic result and its timestamp; it does not expire into a
failure just because time passed.

## Actions

The integration adds `shipments.add`, `shipments.refresh`,
`shipments.set_override`, `shipments.clear_override`, `shipments.archive`, and
`shipments.unarchive` actions. Use **Developer tools → Actions** to try them.
Adding a shipment returns its ID, revision, and status when a response is
requested. Direction is required: use `inbound` for packages you receive or
`outbound` for packages you send. For example:

```yaml
action: shipments.add
data:
  title: Workshop supplies
  direction: inbound
  merchant: Example shop
  tracking_number: YOUR_TRACKING_NUMBER
response_variable: parcel
```

For `carrier: DoorDash`, also supply `recipient_url` with the original
recipient link from the merchant's shipping email. The action verifies the
same number, carrier and link as the dashboard and API.

Existing shipment actions accept `shipment_id`. The adapter reads the current
version before submitting a change. Advanced callers can pass `if_match` to
require a specific revision. A concurrent edit produces a conflict; review the
latest record before trying again. For retryable creation workflows, supply a
stable `idempotency_key` instead of generating a new shipment on each attempt.

Actions require an authenticated Home Assistant user. Status corrections and
archiving additionally require an administrator. They are explicit human
decisions; unattended ingestion should use the restricted agent API token.
For ingress, the app checks the administrator IDs supplied by a recent
authenticated integration heartbeat. Only active administrators qualify; the
list expires after three minutes and a missing or stale list denies privileged
actions. Keep the integration connected so this authorization can refresh.

## Standalone Docker

The same image runs without Home Assistant. Docker is required. Create an
owner token and a separate integration token, set your EasyPost key, and start
it with a persistent volume:

```sh
export SHIPMENTS_API_TOKEN="$(openssl rand -hex 32)"
export SHIPMENTS_INTEGRATION_TOKEN="$(openssl rand -hex 32)"
export EASYPOST_API_KEY="your-easypost-api-key"
docker run --detach --name shipments --restart unless-stopped \
  --publish 127.0.0.1:8080:8080 \
  --mount type=volume,source=shipments-data,target=/data \
  --env SHIPMENTS_API_TOKEN \
  --env SHIPMENTS_INTEGRATION_TOKEN \
  --env EASYPOST_API_KEY \
  ghcr.io/jessfraz/ha-packages:0.5.12
```

Open `http://127.0.0.1:8080` and enter the owner API token. Save both tokens in
your password manager before closing the shell. They must be distinct and
contain 32–256 printable ASCII characters without whitespace. Tracker
registration can incur EasyPost
charges. The image supports amd64 and arm64 and runs the service as an
unprivileged user.

To use the integration with a standalone server, install it through HACS and
restart Home Assistant. Then choose **Settings → Devices & services → Add
integration → Shipments → Connect a standalone service** and enter the server
URL and integration token. The owner token is not accepted for new standalone
pairing or reauthentication. If you run without Home Assistant, the integration
token is optional. Expose port 8080 on a trusted network address that Home
Assistant can reach. Use TLS for access across untrusted networks.

## Data and backups

The SQLite database is stored in `/data/shipments.sqlite3`. Home Assistant
stops the app before backing up this directory. Back up both the app and Home
Assistant configuration to preserve the database and paired integration.
Treat backups as private: they include shipment details and saved credentials.
They also include original recipient tracking links.
They preserve agent access and revocations at the time of the backup.

Stop the app before copying the database manually so its WAL is checkpointed.
Restore both parts to recover the database, settings, and paired integration.

## Updates

The app version selects a matching prebuilt container image. Release tags
publish images for amd64 and aarch64. Update the app through the App Store and
the integration through HACS. Restart Home Assistant after integration updates.
Back up both parts before upgrading.
The installation checks and Home Assistant Repairs warn when app and
integration versions differ. Update both parts to the same release.

The 0.3.0 upgrade permanently deletes legacy shipments without a tracking
number, including their sources, history, and saved idempotency responses.
Tracked records and their existing history are preserved. Make a backup
before upgrading if you need to keep the removed data.
