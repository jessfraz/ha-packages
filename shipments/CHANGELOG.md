# Changelog

## 0.5.0

- Keep combined and per-direction count entity IDs, add measurement statistics
  and distinct status icons, and label the expected/unknown/pre-transit count
  **Awaiting pickup** without claiming unknown tracking has a known label.
- Add native delivery-exception and synchronization-problem entities, an
  inbound ETA calendar, and durable out-for-delivery, delivered, and exception
  events. Resume from a saved checkpoint after restart, skip initial history,
  and expose stable event IDs for deduplicating at-least-once delivery.
- Open the supervised dashboard at `/shipments` for ordinary Home Assistant
  users while enforcing administrator access from authenticated integration
  heartbeats. Standalone device links use the configured backend URL.
- Warn when the app and HACS integration versions differ. Update both parts
  and restart Home Assistant after installing the integration update.
- Require a separate integration credential for standalone Home Assistant
  pairing and reauthentication; keep the owner credential for direct access.

## 0.4.0

- Preserve EasyPost carrier-credential errors, including native
  `CREDENTIALS_NOT_FOUND` on HTTP 404, instead of hiding them as unavailable
  tracking. Distinguish FedEx account choices and keep the provider status and
  safe native code in shipment state and logs. Add regressions for credential
  failures and successful FedEx Default registration.

- Save shipments and tracking edits before contacting EasyPost. Pending
  registration survives outages and restarts; uncertain outcomes reconcile
  existing trackers before retrying. Show actionable registration states,
  provider status and safe errors without losing local records.
- Require inbound or outbound direction for new shipments, migrate existing
  records to inbound, and show both directions together with optional filters
  and matching colored badges.
- Keep shipment, filter and settings views in shareable URL fragments with
  browser navigation. Show carrier history before actions and corrections,
  with sources collapsed at the bottom.

- Show expected and delivered dates, signature requirements, and carrier scan
  history. Active views sort by expected delivery; delivered views show newest
  deliveries first, with stable backend pagination and unknown dates last.
- Use status colors, clickable count filters, clearer shipment details, and
  carrier location map links. Move healthy installation checks and agent
  access below the list, and remove the dashboard slogans.
- Use a typed carrier enum and the same canonical EasyPost catalog in the API
  and carrier dropdown, with automatic detection still optional.
- Keep the last EasyPost installation diagnostic and its check time instead
  of turning a successful result into a failure after five minutes.
- Write redacted JSON registration, sync, and request logs with UTC timestamps,
  request IDs, safe routes, durations, and error codes. Credentials, tracking
  numbers, request bodies, raw URLs, and provider messages stay out of logs.
- Share EasyPost concurrency and rate-limit protection across tracking checks,
  registration, and connection checks. Respect provider cooldowns while
  allowing local creates and edits to save.
- Coalesce manual refreshes with a persistent one-minute cooldown, including
  failed checks on terminal shipments.
- Spread polling with per-shipment jitter and slow unchanged unknown and
  pre-transit labels after two days and a week. Status changes restore the
  normal schedule; delivered and archived shipments still stop polling.

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
