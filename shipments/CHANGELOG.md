## 0.6.19

- Reuse Amazon website sessions and serialize account checks. Apply longer
  exponential backoff with jitter, honor Retry-After, and bound request timeouts.
- Distinguish confirmed website challenges from upstream service errors using
  fixed, non-sensitive diagnostics. Retain saved sign-in and persistently pause
  automatic checks until an explicit successful Preview.
- Show verification guidance in settings and one Home Assistant notification.
  Keep slow Amazon observations inside their bounded worker lease.

## 0.6.18

- Resolve Amazon item-pop links to uniquely verified indexed tracking pages,
  preserving package identities, split-parcel products and saved sign-in.
- Report fixed deferral counts for unsupported links, missing identity or product
  evidence, unverified tracking pages and bounded request limits.

## 0.6.17

- Decode gzip-compressed Amazon orders and tracking responses before parsing.
  Keep the decoded response limit and saved sign-in protections.

## 0.6.16

- Save verified Amazon sign-in before website requests. Preview failures retain
  the persistent grant rather than requiring another device registration.
- Retry the supported legacy orders route for unrecognized layouts, with safe
  layout/challenge diagnostics and no imports from unknown parcels.
- Persist refresh-token replacements atomically before cookie requests.

## 0.6.15

- Honor Amazon's cookie paths, including omitted and empty paths. Report the exact
  connection step and a safe error reference when sign-in cannot finish, without
  exposing credentials or provider responses. Missing account identity stays
  blocked rather than linking an unverified account.

- Add the existing parcel icon beside the app name, matching FileVault header
  sizing, spacing, alignment and light/dark branding. Preserve shipment cards
  and the rest of the dashboard styling.

## 0.6.14

- Connect one Amazon.com account from Shipments settings, preview verified parcels
  without changing shipments, and explicitly enable automatic imports.
- Share normal tracking cadence and preserve carrier identity, existing records,
  history and manual corrections when Amazon provides an existing tracking number.
- Notify Home Assistant when Amazon needs sign-in again, with a direct reconnect
  path. Keep credentials private and imports disabled until explicitly enabled.
- Amazon.com payload compatibility still needs a user-connected live preview;
  ambiguous parcels and unverified delivery times are left unset.

## 0.6.13

- Rename the carrier-link module to `parcel-links.js` to avoid a content-filter
  false positive that stopped the dashboard before startup. Preserve
  carrier/provider links and existing shipment behavior.

## 0.6.12

- Add direct carrier tracking beside EasyPost and AfterShip links for Amazon
  Logistics/MCF, OnTrac, SpeedX, Sway and Veho. Resolve TBA numbers through
  Amazon's recipient tracker and Sway numbers through its public lookup.
- Open DHL tracking results automatically. Keep original DoorDash recipient
  links, validated provider links and existing delivery date/calendar behavior.

## 0.6.11

- Share civil-date precision between new and legacy EasyPost estimates, preserving
  original ETA evidence and explicit timestamps. Keep calendar and due-today
  selection on the same Home Assistant local-day/window projection.
- Add deduplicated expected-today/out-for-delivery items with accurate statuses;
  retain status-only sensors and delivery notifications. Use HA's timezone for
  web date labels even when the browser is in another timezone.

## 0.6.10

- Preserve explicit carrier delivery-window bounds separately from a point ETA.
  Show genuine aware start/end windows as timed calendar entries; keep date-only
  estimates and lone ETAs all-day without inventing a delivery slot.
- Preserve delivered calendar history and raw boundary offsets. Keep windows
  queryable after their end time so an agenda can retain today’s history.

## 0.6.9

- Remove artificial EasyPost request spacing and queue concurrent work within a
  bounded deadline. Keep real provider cooldowns and avoid persisting local
  contention as an upstream rate limit.
- Keep delivery calendar entries visible for their full local day, including DST
  transitions. Retain delivered shipments as clearly labeled delivery history
  using verified actual dates; keep raw estimates and actual timestamps distinct.
- Reconcile OnTrac OFDL scans with stale in-transit provider status using explicit
  scan codes and chronology. Preserve raw scan evidence, user overrides and later
  terminal outcomes.

## 0.6.8

- Resolve ordinary FedEx input to the supported FedEx Default Wallet route before
  registration. Preserve explicit custom accounts through FedExAccount, and keep
  UPS and other product-specific routes unchanged. Retain exact stored carrier
  history, registration recovery, global deduplication and user decisions.

## 0.6.7

- Correct recipient Amazon MCF / Swiship tracking to AfterShip's `amazon`
  courier (Amazon Shipping + Amazon MCF). Keep exact carrier detection,
  lookup before registration and tracking-number validation. No credential
  or subscription changes are required.

# Changelog

## 0.6.6

- Track source-identified Amazon Multi-Channel Fulfillment parcels with the
  existing AfterShip provider and its distinct `amazon-mcf` courier. Preserve
  EasyPost AmazonShipping/AmazonMws routes and existing shipment history.
  Provider quotas and courier verification still apply.

## 0.6.5

- Let the app's **Show in sidebar** setting own the Shipments sidebar entry.
  Keep the integration's stable route hidden so existing shipment, calendar,
  and notification links continue to work without a second sidebar entry.

## 0.6.4

- Keep date-only carrier estimates on their stated calendar day in Home
  Assistant and the shipment list. Preserve actual timed estimates, including
  explicit carrier times and timezone offsets.
- Update TLS trust dependencies to reject distrusted emSign intermediates.

## 0.6.3

- Keep buttons at least 44 pixels wide and text inputs and selects at least
  44 pixels tall, including short direction tabs and shipment editor fields.

## 0.6.2

- Stack shipment detail facts on narrow screens so labels and values stay
  readable. Keep long values within their cards and enlarge buttons and
  dialog close controls to 44 pixels.

## 0.6.1

- Add optional notification blueprints for delivery updates, persistent
  tracking errors, and a daily inbound delivery summary. Include package
  links, phone selection, direction filters, and quiet handling of restored
  states and empty summaries.
- Add native Home Assistant diagnostic downloads with versions, connection
  health, and cached counts. Exclude credentials, backend URLs, package
  details, source records, and raw errors.

## 0.6.0

- Add native Assist questions and an Assist API tool for arrivals, confirmed
  deliveries, named shipment status, and tracking problems. Share one typed
  query handler, default to inbound, and use Home Assistant local-day bounds.
- Respect Assist exposure and user permissions, keep responses bounded, and
  report unavailable or partial data clearly. Voice queries read synchronized
  records without creating trackers or polling carriers.

## 0.5.13

- Close shipment details and the editor when clicking the backdrop. Keep
  clicks inside the dialog and drags from inside from dismissing it, and
  preserve the list or shipment URL when closing.

## 0.5.12

- Open active inbound shipments by default from the Home Assistant sidebar
  and app home. Preserve explicit direction filters, shipment links, and
  combined tracking-issue views.
- Shorten the README and move detailed API and architecture references into
  linked documentation.

## 0.5.11

- Save the last EasyPost installation check and timestamp in SQLite so app
  restarts and upgrades retain the result. Match it to the configured key and
  endpoint, and replace it when a later check fails.
- Open the exact shipment when clicking a delivery in Home Assistant's native
  calendar or calendar card. Support supervised ingress and standalone app
  links, preserve other calendars' normal dialogs, and handle integration
  reloads and frontend module updates without duplicate click handlers.
- Keep calendar cards independent of Shipments. Document Calendar Card Pro's
  current card-wide action limitation.

## 0.5.10

- Add a parcel icon for the Home Assistant integration and app, with light,
  dark and high-resolution integration variants.
- Show the icon and README in HACS details. Document the HACS 2.0.5 store-tile
  limitation for custom integrations with local brand images.

## 0.5.9

- Make tracking numbers unique across all shipments, including archived records,
  different carriers and both directions. Reject duplicate creates and tracking
  edits before provider requests, with a clear error naming the existing record.
- Reserve each number across providers during verification and saving so
  simultaneous requests cannot register it twice. Preserve idempotent replays
  and allow carrier corrections on the original shipment.
- Preserve existing data on upgrade. Stop atomically if a legacy database has
  duplicate numbers instead of silently deleting records or history.

## 0.5.8

- Clear tracking issues when a shipment is manually marked delivered, returned
  to sender, or cancelled. Stop retries and remove it from the tracking-error
  count and list, including when an in-flight request fails afterward.
- Clear stale issues on existing manually completed shipments during upgrade.
  Preserve carrier history, tracker ownership and registration intent, and
  resume tracking if the correction is removed.
- Show stopped tracking in completed shipment details without registration
  warning badges.

## 0.5.7

- Type-check the Home Assistant integration and its tests with pinned ty in CI,
  alongside Ruff linting and formatting. Keep the local checks documented and
  dependencies locked.
- Use Home Assistant's native entity attributes and cache invalidation for
  shipment counts, package details, and problem indicators. Check missing test
  entities explicitly and match native event and service-response types.

## 0.5.6

- Show a reconnecting screen during Home Assistant startup instead of asking
  for an API token. Retry automatically with bounded requests and backoff,
  preserve deep links, and pause while the page is hidden.
- Route Veho through AfterShip. Migrate existing records automatically while
  preserving history and reconciling existing imports before registration.

## 0.5.5

- Track DoorDash directly from its original recipient link. Require and verify
  that link and tracking number before saving; keep DoorDash out of EasyPost
  and AfterShip. Remove legacy DoorDash imports without recipient links.
- Route SpeedX and Sway through AfterShip, adopt existing imports before new
  registrations, and stop automatic retries for carrier connection problems.
  Preserve prior provider ownership in history when migrating existing rows.
- Preserve actual delivery timestamps even when a provider has no scan history.
  Show provider-specific setup and safe native error codes without logging
  recipient links or tracking numbers.

## 0.5.4

- Add package details to inbound delivered-today and out-for-delivery sensors.
  Keep numeric counts, preserve titles, link to each shipment, and cap lists
  at 20 with an explicit truncation flag. Use the same local-day filters and
  carrier delivery timestamps as the count, with deliveries newest first and
  arrivals sorted by ETA.
- Exclude tracking numbers from sensor attributes and package lists from
  recorder history. Keep failed or inconsistent snapshots unavailable.
- Exercise EasyPost's real test-mode API in CI for registration, tracking,
  replay, restart and tracking-number changes, using documented test fixtures.

## 0.5.3

- Match Maintenance's neutral light and dark styling, compact controls, and
  restrained mono typography. Simplify labels and improve mobile layouts.
- Show the running service version and tracking status in a consistent footer.
- Update SHA-256 dependencies while preserving stored request fingerprints.

## 0.5.2

- Verify tracking before saving a shipment or changing its tracking identity.
  Carrier input may be auto-detected, but every saved shipment has a canonical
  carrier enum and tracking number. Remove incomplete legacy imports with
  their sources, history and create replays.
- Persist verification intent separately, recover existing trackers after
  uncertain responses or restarts, and reserve imported sources before paid
  registration. Failed verification saves no new shipment; normal endpoint
  retries honor durable backoff and idempotency.
- Stop automatic retries for missing carrier accounts, credentials and test
  keys. Show actionable setup errors and allow an explicit refresh after
  configuration. Preserve transient HTTP failures as retryable even when a
  provider returns a misleading native rejection code.
- Add a linkable Tracking issues view and consistent table/detail error
  badges. Add an inbound Delivered today sensor and filtered view using
  actual carrier delivery timestamps and Home Assistant's local day,
  including daylight-saving changes.
- Fix the inbound calendar's missing icon by using a supported delivery icon.
- Keep provider wire formats and recovery policy behind the shipping API
  trait. EasyPost remains the configured backend. Include safe native request
  IDs in redacted error logs, and keep filtered pagination within its cursor
  limit.

## 0.5.1

- Look for an existing EasyPost tracker before every new registration, including
  the first attempt. Adopt one confirmed match instead of registering again;
  only a complete empty lookup permits a new registration. Provider failures
  back off, while ambiguous matches need attention.
- Recheck the shipment after waiting for the outbound request slot so an edit,
  archive, or terminal correction during pacing cancels the registration.
- Reuse the existing direction enum in Home Assistant summary polling.

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
