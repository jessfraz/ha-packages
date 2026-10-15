# Installation

Add `https://github.com/jessfraz/ha-packages` in **Settings → Apps → App store →
Repositories**, then install **Shipments**. On older Home Assistant versions,
these settings are named **Add-ons**.

Set `api_token` to a random secret of at least 32 characters before starting
the app.
Use the same token when configuring the Shipments integration. Keep it in
your password manager.

Open the web UI from the app page, or enable **Show in sidebar**. Home
Assistant authenticates the dashboard through ingress. Its API token is
never sent to the browser.

## Options

| Option | Required | Purpose |
| --- | --- | --- |
| `api_token` | Yes | Owner token for the integration and full API. |
| `agent_token` | No | Separate token for agents that submit shipments. |
| `easypost_api_key` | No | Enables carrier tracking through EasyPost. |

Tracking without an EasyPost key keeps manual shipment records. The agent
token has restricted permissions; use the owner token for administration.

## Integration

The integration requires Home Assistant 2026.10 or newer.

Install `custom_components/shipments` from this repository into Home
Assistant's `custom_components` directory and restart Home Assistant. Add
**Shipments** from **Settings → Devices & services → Add integration**.

Use `http://<app-hostname>:8080` as the service URL. The hostname is shown on
the app's information page. Internal app networking does not require a host
port mapping.

For clients on another machine, enable the app's optional `8080/tcp` network
port and use the Home Assistant host address. All requests to that port
require a bearer token. Keep it on a trusted network, or put TLS in front of
it before remote access. Ingress port 8099 is internal to Home Assistant.

### Actions

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

The same image runs without Home Assistant. Docker is required. Create a token
and start it with a persistent volume:

```sh
export SHIPMENTS_API_TOKEN="$(openssl rand -hex 32)"
docker run --detach --name shipments --restart unless-stopped \
  --publish 127.0.0.1:8080:8080 \
  --mount type=volume,source=shipments-data,target=/data \
  --env SHIPMENTS_API_TOKEN \
  ghcr.io/jessfraz/ha-packages:0.1.1
```

Open `http://127.0.0.1:8080` and enter the token. Save it in your password
manager before closing the shell. To enable tracking, also pass
`--env EASYPOST_API_KEY` with a key set in your shell. The image supports amd64
and arm64 and runs the service as an unprivileged user.

To use the integration with a standalone server, expose port 8080 on a trusted
network address and configure that address as the integration's service URL.
Use TLS for access across untrusted networks.

## Data and backups

The SQLite database is stored in `/data/shipments.sqlite3`. Home Assistant
stops the app before backing up this directory. Treat app backups as private: they
include shipment details and configured credentials.

Stop the app before copying the database manually so its WAL is checkpointed.
Restore the complete app backup to recover both the database and settings.

## Updates

The app version selects a matching prebuilt container image. Release tags
publish images for amd64 and aarch64. Back up the app before upgrading.
