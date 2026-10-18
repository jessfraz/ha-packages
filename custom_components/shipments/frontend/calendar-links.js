// HA's native calendar has no event URL field. Intercept only our own events
// before its detail dialog opens; titles and descriptions never select a URL.
const INSTALL_EVENT = "shipments-calendar-navigation-install";
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const PANEL = "/shipments/shipments/";

function shipmentDestination(event) {
  const detail = event.detail;
  if (detail?.dialogTag !== "dialog-calendar-event-detail") return null;
  const { calendarId, entry } = detail.dialogParams ?? {};
  if (typeof calendarId !== "string" || !calendarId.startsWith("calendar.")) return null;
  if (typeof entry?.uid !== "string" || !UUID.test(entry.uid)) return null;
  // The native calendar element owns hass and is on the composed event path,
  // including when embedded in a dashboard. No shadow-root traversal is needed.
  const owner = event.composedPath().find(node => node?.hass?.states);
  const state = owner?.hass.states[calendarId];
  if (!state || state.state === "unavailable" || state.state === "unknown") return null;
  if (state.attributes?.shipments_calendar !== true) return null;
  const base = state.attributes.shipments_app_url;
  if (base === PANEL) return PANEL + encodeURIComponent(`shipment=${entry.uid}`);
  if (typeof base !== "string") return null;
  try {
    const url = new URL(base);
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password
      || url.pathname !== "/" || url.search || url.hash) return null;
    url.hash = `shipment=${entry.uid}`;
    return url.href;
  } catch {
    return null;
  }
}

export function installCalendarNavigation(target, navigate) {
  // Extra modules can be reloaded under a new versioned URL in a live page.
  // Retire the previous module's listeners before taking ownership of clicks.
  target.dispatchEvent(new Event(INSTALL_EVENT));
  const controller = new AbortController();
  target.addEventListener(INSTALL_EVENT, () => controller.abort(), {
    signal: controller.signal,
  });
  target.addEventListener("show-dialog", event => {
    const destination = shipmentDestination(event);
    if (destination === null) return;
    // Capture precedes HA's bubbling dialog manager and its history mutation.
    event.stopPropagation();
    navigate(destination);
  }, { capture: true, signal: controller.signal });
  // Keep the listener across pagehide: a BFCache restore resumes this document.
  // Loaded/unloaded entity state is checked afresh on every click.
  return () => controller.abort();
}

if (typeof window !== "undefined") {
  installCalendarNavigation(window, url => window.location.assign(url));
}
