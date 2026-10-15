# Changelog

## 0.2.1

- Load Supervisor support only when Home Assistant manages apps, so the
  integration also loads on minimal Home Assistant Core installations.

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
