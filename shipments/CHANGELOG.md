# Changelog

## 0.3.0

- Require a tracking number and EasyPost acceptance before creating a shipment
  or changing its tracking identity. Carrier detection remains optional, and
  unknown or pre-transit labels do not need a first carrier scan.
- Keep unsaved drafts when verification fails and show verification progress
  while saving. Provider failures leave the registry unchanged.
- Show carrier and tracking details in the list, with provider health and
  trusted carrier and EasyPost tracking links in shipment details.
- Permanently remove legacy shipments without tracking numbers during the
  upgrade, including their sources, history, and idempotency responses.
  Tracked shipments and their history are preserved. Back up before upgrading.

## 0.2.2

- Remove the manual agent token from app settings and environment options.
- Create named agent credentials from the dashboard, with one-time token
  display and individual revocation. Credential hashes persist in SQLite.
- Keep agent access separate from Home Assistant pairing and attribute imports
  to the individual agent. Revocation applies without restarting the app.

## 0.2.1

- Load Supervisor support only when Home Assistant manages apps, so the
  integration also loads on minimal Home Assistant Core installations.
- Preserve completed EasyPost check results when several callers run
  installation checks at the same time.

## 0.2.0

- Pair the app and integration through Home Assistant discovery. Credentials
  are generated and stored privately; setup no longer asks for an owner token
  or internal hostname.
- Give the integration a separate credential for shipment actions. Installation
  checks and EasyPost connection checks remain owner-only.
- Document HACS installation and show guidance when the other component is
  missing.
- Add installation checks for the app, database, integration, authentication,
  read-only EasyPost access, and backup coverage.
- Update SQLx, UUID, nixpkgs, and the checkout action.

## 0.1.1

- Default to active deliveries, with delivered, all, and archived views.
- Refresh the visible dashboard automatically and inspect history snapshots.
- Add ordinary Home Assistant action fields and return shipment identifiers.
- Build and smoke-test containers on native amd64 and arm64 runners.

## 0.1.0

- Local shipment registry backed by SQLite.
- Home Assistant ingress dashboard and bearer-authenticated API.
- Optional EasyPost tracking and a separate agent token.
