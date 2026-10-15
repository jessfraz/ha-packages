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

## Data and backups

The SQLite database is stored in `/data/shipments.sqlite3`. Home Assistant
stops the app before backing up this directory. Treat app backups as private: they
include shipment details and configured credentials.

Stop the app before copying the database manually so its WAL is checkpointed.
Restore the complete app backup to recover both the database and settings.

## Updates

The app version selects a matching prebuilt container image. Release tags
publish images for amd64 and aarch64. Back up the app before upgrading.
