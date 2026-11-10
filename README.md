# Packages

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/jessfraz/ha-packages/main/custom_components/packages/brand/dark_icon@2x.png">
  <img src="https://raw.githubusercontent.com/jessfraz/ha-packages/main/custom_components/packages/brand/icon@2x.png" width="64" height="64" alt="Packages parcel icon">
</picture>

Track incoming and outgoing packages in Home Assistant. Add packages through
the dashboard or API, follow carrier updates, and correct a status yourself
when needed. Carrier updates never overwrite a manual correction.

The app is a Rust service with a SQLite database and a web dashboard. A Python
integration adds Home Assistant sensors, a delivery calendar, events, and
actions. Shipment records and history are stored locally. Tracking requests
go to the configured provider.

## Install

Requires Home Assistant 2026.11 or newer. Install both the app and integration
using this repository URL:

```text
https://github.com/jessfraz/ha-packages
```

1. Add the repository under **Settings → Apps → App store → Repositories**.
   Install **Packages**, configure your provider API keys, and start the app.
2. Add the same repository to **HACS → Custom repositories** as an
   **Integration**. Download **Packages** and restart Home Assistant.
3. Confirm the discovered connection under **Settings → Devices & services**.

Enable **Show in sidebar** on the Packages app page, then open **Packages**
in the sidebar. Pairing and dashboard authentication use Home Assistant;
there is no token to create or copy during setup.

See the [installation guide](packages/DOCS.md) for configuration, pairing,
updates, and backups. Keep the app and integration on the same release.

The [JSON CLI](docs/cli.md) uses the same API types as the server and supports
native and Home Assistant connections. Install it with `nix build .#ha-packages`.

## Tracking

Every shipment needs a tracking number and an `inbound` or `outbound`
direction. Select a carrier or let the provider detect it. The app verifies
tracking before saving and stores the resolved carrier. A new label can be
tracked before its first scan.

| Provider | Used for |
| --- | --- |
| EasyPost | Most carriers, including automatic carrier detection. |
| Amazon account | Verified parcels from your connected Amazon.com account. |
| AfterShip | Amazon MCF, SpeedX, Sway / Returnmates, and Veho. |
| DoorDash | Parcels with an original DoorDash Drive recipient tracking link. No API key required. |

### Amazon account imports

Amazon Account discovers verified consumer-account parcels from recent Amazon.com
orders. Existing Amazon MCF tracking remains separate.

Administrators can connect one Amazon.com retail account from the dashboard's
**Amazon** settings. Open the generated Amazon sign-in link, complete any
verification on Amazon, and paste the final landing address back into Packages.
The flow uses Amazon's device sign-in, so Amazon may show an Alexa sign-in page.
Verified account identity and a successful token grant are saved before website
requests. Cookie, layout and preview failures retain that sign-in; retry Preview
without registering another device. A replacement refresh token, when returned,
is saved atomically before cookie requests. The private app-data file survives
app and Home Assistant restarts; routine refresh does not require signing in.


If connecting fails, the error names the failed step and includes a request ID.
The matching `amazon_connection` app log records a fixed error category, upstream
HTTP status when known, and elapsed time. Share that request ID rather than the
landing address, authorization code, tokens, or Amazon page contents. A submitted
sign-in attempt cannot be retried with the previous final address.
Packages never receives your password or verification code.

Use **Preview packages** to inspect the result before choosing **Enable imports**.
Preview does not change shipment records. Only parcels with verified item mapping,
a supported carrier and a real tracking number are imported. Split parcels stay
separate; a number already in Packages links to its existing record.

Amazon parcels use the same tracking schedule as other packages. Discovery checks
recent order pages in small paced batches, then checks again every 30–33 minutes.
New work continues even when none of the saved parcels are active. If Amazon rejects the saved
sign-in, Home Assistant raises a native Repair linking to **Reconnect Amazon**.
Discovery failures and unresolved source records also appear in Settings → Repairs,
with app logs and integration diagnostics. Repairs survive connection outages and
reloads until a fresh healthy discovery report or an intentional disable confirms
recovery. The backend retains known coverage failures until a full clean scan.
The integration keeps polling even when its entities are disabled. Missing or
expired health reports raise an unavailable Repair, with a three-minute initial
startup grace. Rejected backend credentials use Home Assistant's reauthentication
Repair; independent Amazon sign-in or discovery failures remain separate.
Network failures and changed pages retain saved data and retry separately.
Disconnect stops account access and retains shipment records and history.

The saved refresh token can create an Amazon website session; it is not scoped
only to packages. It stays in an owner-only `amazon-account.json` file beside the
database, and is included in app backups. It is not encrypted separately at rest.
Cookies stay in memory. To revoke the Amazon device itself, use Amazon's device
management; local disconnect removes the app's saved credential.

Carrier access depends on your provider account. Some carriers require their
own credentials, and registering a tracker can incur provider fees. See
[carrier configuration](packages/DOCS.md#add-or-edit-a-shipment) before adding
packages.

The dashboard shows status, expected and actual delivery dates, tracking
links, signature information when known, and carrier history. It opens on
active inbound packages. Select **All** or **Outbound** to see other
directions, open a shipment for its full history, or archive it when finished.
Tracking numbers are unique, including across archived packages.

## Home Assistant

The integration provides:

- Shipment counts, including separate inbound and outbound counts for active
  packages and exceptions.
- An inbound **Delivered today** sensor using actual carrier delivery times
  and Home Assistant's time zone. It and **Inbound out for delivery** include
  package details for dashboard lists.
- A calendar of inbound delivery estimates. Click an event in the native
  calendar to open that shipment in the app.
- [Assist queries](packages/DOCS.md#ask-assist) for arrivals, confirmed
  deliveries, package status, and tracking problems. Local Assist and AI
  assistants use the same shipment data.
- Tracking-error indicators, shipment events for automations, and actions to
  refresh tracking, correct a status, or archive a shipment.
- Optional [notification blueprints](blueprints/README.md) for delivery updates,
  persistent tracking problems, and a daily delivery summary.

Download native diagnostics from the Packages integration's menu in
**Settings → Devices & services** when troubleshooting a connection. The
report includes versions and health information, without package details or
credentials.

See the [entity reference](packages/DOCS.md#sensors),
[calendar](packages/DOCS.md#delivery-calendar), and
[actions](packages/DOCS.md#actions). Dashboard cards can use these standard
entities independently of the app.

## API

The JSON API is available at `/v1`. Create a named ingestion credential under
**Agent access** in the dashboard for an importer or agent. These credentials
can read and edit packages; manual corrections, archiving, and credential
management require owner access.

For a local standalone server, with that credential in `PACKAGES_AGENT_TOKEN`:

```sh
curl --fail-with-body http://127.0.0.1:8080/v1/packages \
  -H "Authorization: Bearer $PACKAGES_AGENT_TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: example-delivery-1' \
  -d '{"title":"New desk","direction":"inbound","tracking":{"number":"YOUR_TRACKING_NUMBER"}}'
```

Replace `YOUR_TRACKING_NUMBER` with a real tracking number. Creates require an
idempotency key; edits require the current `ETag` in `If-Match`. Provider
failures do not save an unverified shipment. Retry an uncertain create with
the same input and key so the app can reconcile an existing tracker.

For a Home Assistant app, enable the optional network port and substitute the
host address. See [external API access](packages/DOCS.md#agent-access).

- [API reference](docs/api.md): endpoints, filters, authentication, and errors.
- [OpenAPI schema](openapi/api.json), also served at `/openapi.json`.
- [Architecture](docs/architecture.md): provider implementations, persistence,
  retries, and registration recovery.

## Run standalone

The service also runs without Home Assistant. For a local source build:

```sh
nix develop
export PACKAGES_API_TOKEN="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
export EASYPOST_API_KEY="your-easypost-api-key"
cargo run --locked
```

Open `http://127.0.0.1:8080` and enter the generated token. The default database
is `packages.sqlite3`; set `PACKAGES_DATABASE` to use another path. Use
`AFTERSHIP_API_KEY` for AfterShip carriers. Provider keys stay on the server.

Prebuilt containers are available for amd64 and arm64. See
[standalone Docker](packages/DOCS.md#standalone-docker) for deployment and
connecting a standalone service to Home Assistant.

## Development

Use `nix develop` for the Rust and Python toolchains. Tests run against local
HTTP servers, SQLite, and Home Assistant itself; the default suite needs no
provider API keys. CI also runs an EasyPost sandbox test and builds and tests
both container architectures.

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, checks, and release steps,
and the [changelog](packages/CHANGELOG.md) for release history.

## License

[MIT](LICENSE).

Amazon account authentication includes Apache-2.0-derived portions from alexapy;
see [NOTICE](NOTICE) and [LICENSE-Apache-2.0](LICENSE-Apache-2.0). The remaining
project source retains its MIT license.
