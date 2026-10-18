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

Set `easypost_api_key`, then start the app. Shipments and tracking edits are
saved locally, then registered with EasyPost in the background. The app creates and saves
its own credentials automatically.

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

The app shares a separate integration credential through authenticated
Supervisor discovery. Pairing does not require pasting a URL or token.
Internal app networking does not require a host port mapping.

The integration also adds a **Shipments** sidebar panel at the stable
`/shipments` path. Ordinary Home Assistant users can open the dashboard;
administrator-only actions still enforce administrator access. The device's
**Visit** link opens this panel for supervised apps. A standalone installation
uses its configured backend URL instead.

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
App Store and the integration through HACS to **0.5.6**, then restart Home
Assistant. Without a recent integration check-in, the version match is
unverified.

**Run checks** makes a read-only EasyPost request when a key is configured;
requests are rate-limited to once per minute. A successful result stays passed
until a later check actually fails. Without an EasyPost key, EasyPost carriers
cannot be verified or saved; DoorDash and configured AfterShip carriers still
use their own providers.

The backup check shows the recorded backup time. It requires a backup made
after pairing. An older, app-only, or missing backup needs attention; an
unavailable backup inventory is reported as
**Unavailable**. Include both **Shipments** and **Home Assistant** when
creating a backup in **Settings → System → Backups**. The check confirms backup
inventory and coverage; restore testing is a separate step.

## Sensors

The integration keeps the existing counts combined across inbound and outbound
shipments, including **Exceptions** and **Sync errors**. It also adds:

| Sensor key | Counts |
| --- | --- |
| `inbound_active` | Active packages you receive. |
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
the Shipments panel, for example
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

`calendar.shipments_inbound_deliveries` is a read-only calendar of carrier
estimates for active inbound shipments. Date-only ETAs create all-day entries;
ETAs with a time zone create one-hour estimated windows. Missing or invalid
dates, outbound shipments, archived records, and terminal shipments do not
appear. Estimates can change and are not guaranteed delivery times.

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
| `aftership_api_key` | Enables SpeedX, Sway / Returnmates, and Veho tracking through AfterShip. |

DoorDash requires its original recipient tracking link rather than an API key.
Other carriers and automatic carrier detection use EasyPost. Provider keys
stay in the app configuration; clients and agents do not receive them.

## Add or edit a shipment

Every shipment needs a tracking number and an explicit **Direction**:
**Inbound** for packages you receive, **Outbound** for packages you send.
Existing records migrate to inbound. The default list shows both directions
together; use the optional direction filters to narrow it.
Leave **Carrier** on **Auto-detect**, or
choose its canonical EasyPost carrier when detection is ambiguous.
Select **SpeedX**, **Sway / Returnmates**, or **Veho** explicitly to use
AfterShip.
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
guidance](../README.md#agents-and-human-corrections)
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
  ghcr.io/jessfraz/ha-packages:0.5.2
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
