# ha-shipments

`ha-shipments` is a JSON client for the existing Shipments API. It does not
collect mail, infer shipment metadata, synchronize a local registry, bootstrap
credentials, or run carrier workflows. The service owns validation, permissions,
tracking verification, registration, and duplicate protection.

## Install and configure

```sh
nix build .#ha-shipments
nix run .#ha-shipments -- --help
# Or, from the development shell:
cargo run --locked --bin ha-shipments -- --help
```

The flake also exposes `packages.shipments` and `apps.shipments`. Both default
outputs remain the server; `cargo run` still runs the server. All existing flake
systems are supported: aarch64-darwin, aarch64-linux, and x86_64-linux.

Choose one transport. URLs must be HTTP(S) origins, such as
`http://127.0.0.1:8080`, without paths, credentials, queries, or fragments.
Credentials are read only from the environment, never accepted as CLI flags.
No login or credential discovery runs.

| Transport | URL | Credential | API prefix |
| --- | --- | --- | --- |
| Native | `SHIPMENTS_URL` | `SHIPMENTS_AGENT_TOKEN` or `SHIPMENTS_API_TOKEN`, not both | `/v1` |
| Home Assistant | `HOME_ASSISTANT_URL` | `HOME_ASSISTANT_TOKEN` | `/api/shipments/v1` |

With both URLs configured, pass `--transport native` or
`--transport home-assistant`. Explicit selection uses only the selected
transport's credential. Native ingestion credentials do not become HA tokens;
HA tokens do not authenticate directly to the standalone app. Owner operations
still require owner access, and HA archive/status corrections require an HA
administrator. No actor or ingress authentication headers are fabricated.

Use HTTPS beyond trusted local transport. Home Assistant tokens grant access
beyond Shipments; prefer restricted ingestion credentials for narrow clients.
Provider keys belong on the server, not in this client.

## JSON and conditions

Body commands read one JSON document from stdin by default. `--input FILE`
reads a local file; `--input -` explicitly selects stdin. Inputs are decoded
using the server's owned request types. Optional fields stay optional. Unknown
write fields, such as actor or carrier status, fail without a request. PATCH
preserves the API distinction between omitted fields and explicit `null`.
Input is limited to 1 MiB and successful responses to 8 MiB.

Successful stdout is one JSON envelope, followed by a newline:

```json
{"status":200,"etag":"\"7\"","data":{"id":"…","revision":7}}
```

`data` contains the typed API response, not the abbreviated illustration above.
`etag` is null for operations without an ETag. Agent revocation returns status
204 and null data. Newly created agent tokens are sensitive stdout data, shown
only once by the API; save securely, not in logs.

Creates require an explicit `--idempotency-key`. The CLI never generates a key.
Every existing-shipment write requires `--if-match '"7"'`, the exact quoted
revision from the prior response's `etag`. It never fetches or substitutes a
new revision to make a write succeed. `412` means stale revision and `428`
means a missing server precondition. Read the current record before deciding
whether an edit is still appropriate.

```sh
export SHIPMENTS_URL=http://127.0.0.1:8080
# Supply SHIPMENTS_AGENT_TOKEN securely through the calling environment.
ha-shipments list --limit 20
ha-shipments create --idempotency-key example-parcel-1 --input parcel.json
ha-shipments get 00000000-0000-4000-8000-000000000001
printf '%s\n' '{"merchant":null}' | ha-shipments patch \
  00000000-0000-4000-8000-000000000001 --if-match '"7"'
```

`parcel.json` uses the normal create contract, for example:

```json
{"title":"Example parcel","direction":"inbound","tracking":{"number":"YOUR_TRACKING_NUMBER"}}
```

The app verifies tracking before saving. Carrier omission requests detection;
`FedExAccount` retains its explicit input meaning. See [API](api.md) for carrier
choices, DoorDash links, source references, and admission guarantees.

## Commands

| Command | API operation / JSON input |
| --- | --- |
| `list [--input FILE] [--limit N] [--page-token TOKEN]` | GET shipments; optional API `Filters` JSON |
| `get ID` | GET shipment |
| `create --idempotency-key KEY [--input FILE]` | POST shipment; `CreateShipment` |
| `patch ID --if-match ETAG [--input FILE]` | PATCH shipment; `UpdateShipment` |
| `add-source ID --if-match ETAG [--input FILE]` | POST sources; `Source` |
| `set-override ID --if-match ETAG [--input FILE]` | PUT status-override; `SetOverride` |
| `clear-override ID --if-match ETAG` | DELETE status-override |
| `archive ID --if-match ETAG` | PUT archive |
| `restore ID --if-match ETAG` | DELETE archive |
| `refresh ID --if-match ETAG` | POST refresh |
| `history ID [--limit N] [--page-token TOKEN]` | GET shipment events |
| `delivery-estimates [--limit N] [--page-token TOKEN]` | GET delivery-estimates |
| `summary [--input FILE]` | GET summary; optional direction/delivered_after/delivered_before JSON |
| `carriers` | GET carriers |
| `system` | GET system |
| `events [--after N] [--limit N]` | GET durable state events, native only |
| `installation` | GET installation, native owner only |
| `check-installation` | POST installation/check, native owner only |
| `agents` | GET agents, native owner only |
| `create-agent [--input FILE]` | POST agents; `CreateAgent`, native owner only |
| `revoke-agent ID` | DELETE agent, native owner only |

The HA proxy does not expose durable state events, installation checks, or
agent management. Those commands fail locally in HA mode with no request.
Integration pairing/presence and browser ingress are not client operations.

List filters are the fields documented in [API](api.md), including boolean
false values and delivered-date bounds. For example:

```sh
printf '%s\n' '{"direction":"inbound","active":true,"sort":"expected"}' |
  ha-shipments list --input - --limit 50
```

Pagination always makes **one request**. List/history/estimate responses retain
`items` and `next_page` inside `data`; supply the opaque token explicitly on the
next invocation. Do not combine list filters with a page token, which already
encodes the filter scope. Durable state events retain `next_after` and
`has_more`; omission of `--after` starts at the current tail, as on the API.
No draining, polling, persistence, cursor advancement, or client-side dedup runs.

## Failures and uncertain writes

Input and HTTP failures produce no stdout, a nonzero exit, and one JSON stderr
diagnostic. An output I/O failure can leave partial stdout.
HTTP failures retain status, known normalized API `error_code` when present,
and numeric `Retry-After` as `retry_after_seconds`. Unknown codes, messages,
response bodies, URL/Location headers, input values, and tokens are never
printed to stderr. Usage errors exit 2; other failures exit 1.

Redirects are refused, including same-origin redirects. Neither redirects nor
HTTP failures trigger retries. The transport has a 10-second connection timeout
and a 120-second total request timeout. Connection, timeout, response-decoding,
or output failure does not prove a write was rejected. Check current server
state; retry an uncertain create only with the original input and idempotency
key. The CLI never switches authentication, rotates keys, retries, or bypasses
tracking verification. Standard HTTP proxy environment configuration applies.
