# Shipments

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/jessfraz/ha-packages/main/custom_components/shipments/brand/dark_icon@2x.png">
  <img src="https://raw.githubusercontent.com/jessfraz/ha-packages/main/custom_components/shipments/brand/icon@2x.png" width="64" height="64" alt="Shipments parcel icon">
</picture>

Track incoming and outgoing packages in Home Assistant. Add shipments through
the dashboard or API, follow carrier updates, and correct a status yourself
when needed. Carrier updates never overwrite a manual correction.

The app is a Rust service with a SQLite database and a web dashboard. A Python
integration adds Home Assistant sensors, a delivery calendar, events, and
actions. Shipment records and history are stored locally. Tracking requests
go to the configured provider.

## Install

Requires Home Assistant 2026.10 or newer. Install both the app and integration
using this repository URL:

```text
https://github.com/jessfraz/ha-packages
```

1. Add the repository under **Settings → Apps → App store → Repositories**.
   Install **Shipments**, configure your provider API keys, and start the app.
2. Add the same repository to **HACS → Custom repositories** as an
   **Integration**. Download **Shipments** and restart Home Assistant.
3. Confirm the discovered connection under **Settings → Devices & services**.

Enable **Show in sidebar** on the Shipments app page, then open **Shipments**
in the sidebar. Pairing and dashboard authentication use Home Assistant;
there is no token to create or copy during setup.

See the [installation guide](shipments/DOCS.md) for configuration, pairing,
updates, and backups. Keep the app and integration on the same release.

## Tracking

Every shipment needs a tracking number and an `inbound` or `outbound`
direction. Select a carrier or let the provider detect it. The app verifies
tracking before saving and stores the resolved carrier. A new label can be
tracked before its first scan.

| Provider | Used for |
| --- | --- |
| EasyPost | Most carriers, including automatic carrier detection. |
| AfterShip | Amazon MCF / Logistics, SpeedX, Sway / Returnmates, and Veho. |
| DoorDash | Parcels with an original DoorDash Drive recipient tracking link. No API key required. |

Carrier access depends on your provider account. Some carriers require their
own credentials, and registering a tracker can incur provider fees. See
[carrier configuration](shipments/DOCS.md#add-or-edit-a-shipment) before adding
shipments.

The dashboard shows status, expected and actual delivery dates, tracking
links, signature information when known, and carrier history. It opens on
active inbound shipments. Select **All** or **Outbound** to see other
directions, open a shipment for its full history, or archive it when finished.
Tracking numbers are unique, including across archived shipments.

## Home Assistant

The integration provides:

- Shipment counts, including separate inbound and outbound counts for active
  shipments and exceptions.
- An inbound **Delivered today** sensor using actual carrier delivery times
  and Home Assistant's time zone. It and **Inbound out for delivery** include
  package details for dashboard lists.
- A calendar of inbound delivery estimates. Click an event in the native
  calendar to open that shipment in the app.
- [Assist queries](shipments/DOCS.md#ask-assist) for arrivals, confirmed
  deliveries, package status, and tracking problems. Local Assist and AI
  assistants use the same shipment data.
- Tracking-error indicators, shipment events for automations, and actions to
  refresh tracking, correct a status, or archive a shipment.
- Optional [notification blueprints](blueprints/README.md) for delivery updates,
  persistent tracking problems, and a daily delivery summary.

Download native diagnostics from the Shipments integration's menu in
**Settings → Devices & services** when troubleshooting a connection. The
report includes versions and health information, without package details or
credentials.

See the [entity reference](shipments/DOCS.md#sensors),
[calendar](shipments/DOCS.md#delivery-calendar), and
[actions](shipments/DOCS.md#actions). Dashboard cards can use these standard
entities independently of the app.

## API

The JSON API is available at `/v1`. Create a named ingestion credential under
**Agent access** in the dashboard for an importer or agent. These credentials
can read and edit shipments; manual corrections, archiving, and credential
management require owner access.

For a local standalone server, with that credential in `SHIPMENTS_AGENT_TOKEN`:

```sh
curl --fail-with-body http://127.0.0.1:8080/v1/shipments \
  -H "Authorization: Bearer $SHIPMENTS_AGENT_TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: example-delivery-1' \
  -d '{"title":"New desk","direction":"inbound","tracking":{"number":"YOUR_TRACKING_NUMBER"}}'
```

Replace `YOUR_TRACKING_NUMBER` with a real tracking number. Creates require an
idempotency key; edits require the current `ETag` in `If-Match`. Provider
failures do not save an unverified shipment. Retry an uncertain create with
the same input and key so the app can reconcile an existing tracker.

For a Home Assistant app, enable the optional network port and substitute the
host address. See [external API access](shipments/DOCS.md#agent-access).

- [Rust CLI](docs/cli.md): `ha-shipments`, JSON I/O, native/HA authentication,
  and Nix packages.
- [API reference](docs/api.md): endpoints, filters, authentication, and errors.
- [OpenAPI schema](openapi/api.json), also served at `/openapi.json`.
- [Architecture](docs/architecture.md): provider implementations, persistence,
  retries, and registration recovery.

## Run standalone

The service also runs without Home Assistant. For a local source build:

```sh
nix develop
export SHIPMENTS_API_TOKEN="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
export EASYPOST_API_KEY="your-easypost-api-key"
cargo run --locked
```

Open `http://127.0.0.1:8080` and enter the generated token. The default database
is `shipments.sqlite3`; set `SHIPMENTS_DATABASE` to use another path. Use
`AFTERSHIP_API_KEY` for AfterShip carriers. Provider keys stay on the server.

Prebuilt containers are available for amd64 and arm64. See
[standalone Docker](shipments/DOCS.md#standalone-docker) for deployment and
connecting a standalone service to Home Assistant.

## Development

Use `nix develop` for the Rust and Python toolchains. Tests run against local
HTTP servers, SQLite, and Home Assistant itself; the default suite needs no
provider API keys. CI also runs an EasyPost sandbox test and builds and tests
both container architectures.

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, checks, and release steps,
and the [changelog](shipments/CHANGELOG.md) for release history.

## License

[MIT](LICENSE).
