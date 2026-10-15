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

Set `easypost_api_key`, then start the app. Adding a shipment or changing its
tracking identity requires EasyPost verification. The app creates and saves
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

If there is no discovered card, choose **Add integration → Shipments → Connect
the Home Assistant app**. Start the app if prompted, then select **Submit** to
find it and confirm the connection.

HACS explains [custom repositories](https://www.hacs.xyz/docs/faq/custom_repositories/)
and [integration installation](https://www.hacs.xyz/docs/use/repositories/type/integration/)
in its documentation.

## Check the installation

Open the app dashboard, expand **Installation**, and select **Run checks**.

| Check | What it verifies |
| --- | --- |
| App and database | The service is running and SQLite responds. |
| Home Assistant | The integration has checked in recently. |
| Authentication | Home Assistant authenticated with the app. |
| EasyPost | The configured key permits a read-only tracking request. |
| Backup | A recorded backup includes the app and Home Assistant configuration. |

The Home Assistant checks use a recent authenticated integration check-in.
If either needs attention, finish the HACS installation, restart Home
Assistant, and confirm the connection in **Devices & services**. If already
paired, check the integration for a connection or authentication error.

**Run checks** makes a read-only EasyPost request when a key is configured;
results are cached for up to a minute. A key is required to add shipments or
change their tracking details.

The backup check shows the recorded backup time. It requires a backup made
after pairing. An older, app-only, or missing backup needs attention; an
unavailable backup inventory is reported as
**Unavailable**. Include both **Shipments** and **Home Assistant** when
creating a backup in **Settings → System → Backups**. The check confirms backup
inventory and coverage; restore testing is a separate step.

## Settings

| Option | Purpose |
| --- | --- |
| `easypost_api_key` | Required to verify new shipments and changed tracking identities. |

## Add or edit a shipment

Every shipment needs a tracking number. Leave **Carrier** blank for automatic
detection, or supply a carrier when EasyPost cannot identify one unambiguously.
Select **Save shipment** and wait while the app verifies tracking. A newly
created label can be saved before its first carrier scan; unknown and
pre-transit tracking states are accepted.

An invalid number, unsupported carrier, or ambiguous detection must be fixed
before saving. If EasyPost is unavailable or its key is missing, the change
is not saved. Your draft stays in the form so you can retry. Existing records
can still be read, corrected, and edited without changing tracking. A tracking
number cannot be removed from an existing shipment.

The list shows each carrier and tracking number. Open a shipment to see its
carrier status, last successful check, next scheduled check, and EasyPost
health before its sources. Manual corrections are shown separately. Tracking
links open the supported carrier's site or the EasyPost page when available.

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

Ingestion clients can use the [API error codes and retry guidance](../README.md#agents-and-human-corrections)
to distinguish invalid input from a provider or configuration failure.

For clients on another machine, enable the app's optional `8080/tcp` network
port and use the Home Assistant host address. All requests to that port
require a bearer token. Keep it on a trusted network, or put TLS in front of
it before remote access. Ingress port 8099 is internal to Home Assistant.

## Actions

The integration adds `shipments.add`, `shipments.refresh`,
`shipments.set_override`, `shipments.clear_override`, `shipments.archive`, and
`shipments.unarchive` actions. Use **Developer tools → Actions** to try them.
Adding a shipment returns its ID, revision, and status when a response is
requested. For example:

```yaml
action: shipments.add
data:
  title: Workshop supplies
  merchant: Example shop
  tracking_number: YOUR_TRACKING_NUMBER
response_variable: parcel
```

Existing shipment actions accept `shipment_id`. The adapter reads the current
version before submitting a change. Advanced callers can pass `if_match` to
require a specific revision. A concurrent edit produces a conflict; review the
latest record before trying again. For retryable creation workflows, supply a
stable `idempotency_key` instead of generating a new shipment on each attempt.

Actions require an authenticated Home Assistant user. Status corrections and
archiving additionally require an administrator. They are explicit human
decisions; unattended ingestion should use the restricted agent API token.

## Standalone Docker

The same image runs without Home Assistant. Docker is required. Create an
owner token, set your EasyPost key, and start it with a persistent volume:

```sh
export SHIPMENTS_API_TOKEN="$(openssl rand -hex 32)"
export EASYPOST_API_KEY="your-easypost-api-key"
docker run --detach --name shipments --restart unless-stopped \
  --publish 127.0.0.1:8080:8080 \
  --mount type=volume,source=shipments-data,target=/data \
  --env SHIPMENTS_API_TOKEN \
  --env EASYPOST_API_KEY \
  ghcr.io/jessfraz/ha-packages:0.3.0
```

Open `http://127.0.0.1:8080` and enter the token. Save it in your password
manager before closing the shell. Tracker registration can incur EasyPost
charges. The image supports amd64 and arm64 and runs the service as an
unprivileged user.

To use the integration with a standalone server, install it through HACS and
restart Home Assistant. Then choose **Settings → Devices & services → Add
integration → Shipments → Connect a standalone service** and enter the server
URL and owner token. Expose port 8080 on a trusted network address that Home
Assistant can reach. Use TLS for access across untrusted networks.

## Data and backups

The SQLite database is stored in `/data/shipments.sqlite3`. Home Assistant
stops the app before backing up this directory. Back up both the app and Home
Assistant configuration to preserve the database and paired integration.
Treat backups as private: they include shipment details and saved credentials.
They preserve agent access and revocations at the time of the backup.

Stop the app before copying the database manually so its WAL is checkpointed.
Restore both parts to recover the database, settings, and paired integration.

## Updates

The app version selects a matching prebuilt container image. Release tags
publish images for amd64 and aarch64. Update the app through the App Store and
the integration through HACS. Restart Home Assistant after integration updates.
Back up both parts before upgrading.

The 0.3.0 upgrade permanently deletes legacy shipments without a tracking
number, including their sources, history, and saved idempotency responses.
Tracked records and their existing history are preserved. Make a backup
before upgrading if you need to keep the removed data.
