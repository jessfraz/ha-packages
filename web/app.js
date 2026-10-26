import { waitForConnection } from "./connection.js";
import { calendarDate } from "./dates.js";

const byId = (id) => document.getElementById(id);
const labels = {
  expected: "Awaiting pickup",
  unknown: "Unknown",
  pre_transit: "Label created",
  in_transit: "In transit",
  out_for_delivery: "Out for delivery",
  available_for_pickup: "Ready for pickup",
  delivered: "Delivered",
  return_to_sender: "Returning",
  failure: "Delivery exception",
  cancelled: "Cancelled",
  error: "Tracking error",
};
const providerNames = { easypost: "EasyPost", aftership: "AfterShip", doordash: "DoorDash" };

let token = "";
let selected = null;
let editing = null;
let nextPage = null;
let eventsPage = null;
let createKey = null;
let submittedCreatePayload = null;
let listGeneration = 0;
let detailGeneration = 0;
let editorGeneration = 0;
let loadingMore = false;
let loadingEvents = false;
let savingEditor = false;
let appliedSearch = "";
let shownDeliveryWindow = null;
let activeDirection = "inbound";
let routeGeneration = 0;
let applyingRoute = false;
let connected = false;
let initialRoutePending = true;
const homeAssistant = document.body.dataset.homeAssistant === "true";
let connectionController = null;
let homeAssistantPrefix = null;
let homeAssistantPath = null;
let listController = null;
let refreshTimer = null;
let showInstallation = false;
let enabledProviders = new Set();
let installationInitialized = false;
let installationNeedsAttention = false;
let agentCredentials = [];
let agentBusy = false;
let agentSession = 0;
let agentTokenGeneration = 0;
let issuedAgentId = null;

class ApiError extends Error {
  constructor(message, status) {
    super(message);
    this.status = status;
  }
}

function editError(error) {
  if (error.status === 412) {
    return "This shipment changed. Your draft is still here. Close and reopen the shipment to review its latest version before retrying.";
  }
  return error.message;
}

function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  if (className) node.className = className;
  return node;
}

function statusBadge(status) {
  const known = Object.hasOwn(labels, status) ? status : "unknown";
  return element("span", labels[known], `badge status-${known}`);
}

function signatureLabel(shipment) {
  if (shipment.carrier_tracking?.signed_by) return "Signed for";
  if (shipment.signature_required === true) return "Signature required";
  if (shipment.signature_required === false) return "No signature required";
  return "Signature not reported";
}

function configurationIssue(sync) {
  if (sync.registration_state !== "configuration_required") return null;
  if (["tracking_carrier_account_required", "tracking_carrier_credentials_required"].includes(sync.error_code))
    return {
      label: "Carrier setup required",
      instruction: "Your shipment is saved. Configure this carrier account in the tracking provider, then choose Refresh tracking. Automatic retries are paused.",
    };
  return {
    label: "Provider setup required",
    instruction: "Your shipment is saved. Update the tracking provider API key in the app configuration, then choose Refresh tracking. Automatic retries are paused.",
  };
}

function manuallyCompleted(shipment) {
  return ["delivered", "return_to_sender", "cancelled"].includes(shipment.status_override?.status);
}

function shipmentBadges(shipment, className) {
  const badges = element("div", undefined, className);
  const direction = shipment.direction === "outbound" ? "outbound" : "inbound";
  badges.append(element("span", direction === "inbound" ? "Inbound" : "Outbound", `badge direction-${direction}`));
  badges.append(statusBadge(shipment.status));
  if (shipment.sync.last_error) {
    const configuration = configurationIssue(shipment.sync);
    const retrying = !configuration && shipment.sync.next_attempt_at != null;
    const issue = element("span", configuration?.label || (retrying ? "Tracking retry scheduled" : "Tracking needs attention"), `badge tracking-issue${retrying ? " retrying" : ""}`);
    issue.title = shipment.sync.last_error;
    badges.append(issue);
  } else if (!manuallyCompleted(shipment) && shipment.sync.registration_state === "pending")
    badges.append(element("span", "Registration pending", "badge registration-pending"));
  else if (!manuallyCompleted(shipment) && shipment.sync.registration_state === "rejected")
    badges.append(element("span", "Registration needs attention", "badge registration-rejected"));
  if (shipment.signature_required === true || shipment.carrier_tracking?.signed_by)
    badges.append(element("span", signatureLabel(shipment), "badge signature-badge"));
  return badges;
}

function deliveredDate(shipment) {
  return shipment.carrier_tracking?.delivered_at ||
    (shipment.status_override?.status === "delivered" ? shipment.status_override.set_at : null);
}

// Fragments keep ingress paths intact and contain no credentials. Back/forward
// restores the same filters, shipment and disclosures as a copied URL.
function route() {
  return new URLSearchParams(location.hash.slice(1));
}

function writeRoute(changes, replace = false) {
  if (applyingRoute) return;
  const params = route();
  for (const [key, value] of Object.entries(changes)) {
    if (value == null || value === "") params.delete(key);
    else params.set(key, value);
  }
  if (!params.has("direction")) params.set("direction", activeDirection);
  if (!params.has("view")) params.set("view", byId("view").value);
  const hash = params.toString();
  if (hash === location.hash.slice(1)) return;
  history[replace || homeAssistantPrefix ? "replaceState" : "pushState"](null, "", hash ? `#${hash}` : location.pathname + location.search);
  if (homeAssistantPrefix) {
    const path = hash ? `/shipments/${encodeURIComponent(hash)}` : "/";
    if (path !== homeAssistantPath) {
      homeAssistantPath = path;
      window.parent.postMessage({ type: "home-assistant/navigate", path: homeAssistantPrefix + path, options: { replace } }, location.origin);
    }
  }
}

function userNavigation() {
  routeGeneration += 1;
  applyingRoute = false;
  if (!selected) detailGeneration += 1;
}

function saveListRoute() {
  userNavigation();
  writeRoute({ direction: activeDirection, view: byId("view").value, q: appliedSearch });
}

async function applyRoute(initial = false) {
  const generation = ++routeGeneration;
  applyingRoute = false;
  const params = route();
  const direction = params.size === 0 ? "inbound"
    : ["inbound", "outbound"].includes(params.get("direction")) ? params.get("direction") : "all";
  const requestedView = params.get("view") || "active";
  const view = [...byId("view").options].some(option => option.value === requestedView) ? requestedView : "active";
  if (params.size === 0) writeRoute({ direction, view }, true);
  const search = params.get("q") || "";
  const changed = direction !== activeDirection || view !== byId("view").value || search !== appliedSearch;
  activeDirection = direction;
  byId("view").value = view;
  appliedSearch = search;
  byId("query").value = search;
  applyingRoute = true;
  try {
    if (showInstallation) {
      if (params.has("installation")) byId("installation").open = params.get("installation") === "open";
      byId("agent-access").open = params.get("agents") === "open";
    }
    if (initial || changed) await load();
    if (generation !== routeGeneration) return;
    if (showInstallation) {
      byId("installation").open = params.has("installation") ? params.get("installation") === "open" : installationNeedsAttention;
      byId("agent-access").open = params.get("agents") === "open";
    }
    const id = params.get("shipment");
    const validId = id && /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(id);
    const edit = params.get("edit");
    if (edit === "new") {
      if (!byId("editor").open || editing) openEditor();
    } else if (validId) {
      if (selected?.id !== id || !byId("detail").open) {
        if (!(byId("editor").open && editing?.id === id && edit === "true")) {
          closeEditor();
          await show(id);
        }
      }
      if (generation !== routeGeneration) return;
      if (edit === "true" && selected) openEditor(selected);
    } else {
      closeEditor();
      closeDetail();
    }
    const section = params.get("section");
    for (const sectionId of ["correction", "activity", "source-details", "connection"]) {
      const disclosure = byId(sectionId);
      if (disclosure) disclosure.open = section === sectionId ||
        (!section && sectionId === "connection" && Boolean(selected?.sync.last_error));
    }
    if (section && byId("detail").open && ["history", "correction", "activity", "source-details", "connection"].includes(section))
      byId(section)?.scrollIntoView({ block: "start" });
  } finally {
    if (generation === routeGeneration) applyingRoute = false;
  }
}

function notice(text, error = false) {
  byId("notice").textContent = text;
  byId("notice").className = error ? "error" : "";
}

async function api(path, options = {}) {
  const headers = { "X-Shipments-Request": "1", ...options.headers };
  if (token) headers.Authorization = `Bearer ${token}`;
  if (options.body !== undefined) headers["Content-Type"] = "application/json";
  const response = await fetch(new URL(path, document.baseURI), {
    ...options,
    headers,
    signal: options.method && options.method !== "GET" ? options.signal
      : options.signal ? AbortSignal.any([options.signal, AbortSignal.timeout(15_000)])
        : AbortSignal.timeout(15_000),
  });
  if (response.status === 204) return null;
  const body = await response.json();
  if (!response.ok) {
    throw new ApiError(
      body.message || `Request failed (${response.status})`,
      response.status,
    );
  }
  return body;
}

function guarded(action) {
  return async (event) => {
    if (event) event.preventDefault();
    try {
      await action(event);
    } catch (error) {
      notice(error.message, true);
    }
  };
}

function renderServiceStatus(system, checked) {
  byId("health").hidden = system.provider_enabled;
  byId("health").textContent = system.provider_enabled
    ? ""
    : "Configure a tracking provider before adding shipments or changing tracking. Existing records remain available.";
  const status = [
    system.provider_enabled
      ? `${system.pending_syncs} tracking checks scheduled · ${system.sync_errors} tracking issues`
      : "Tracking provider not configured",
  ];
  if (checked)
    status.push(`Last checked ${new Date().toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}`);
  byId("app-version").textContent = system.version;
  byId("service-status").textContent = status.join(" · ");
  byId("app-footer").hidden = false;
}

async function connect(signal) {
  const [system, carriers] = await Promise.all([api("v1/system", { signal }), api("v1/carriers", { signal })]);
  signal?.throwIfAborted();
  const carrierField = byId("edit-form").elements.namedItem("carrier");
  carrierField.replaceChildren(element("option", "Auto-detect"));
  carrierField.firstElementChild.value = "";
  const carrierLabels = {
    FedEx: "FedEx (EasyPost account)",
    FedExAccount: "FedEx (your carrier account)",
    FedExDefault: "FedEx Default (EasyPost account)",
    DHLExpress: "DHL Express (your carrier account)",
    DHLExpressDefault: "DHL Express Default (EasyPost account)",
    AmazonMcf: "Amazon MCF / Logistics (AfterShip)",
    SpeedX: "SpeedX (AfterShip)",
    Sway: "Sway / Returnmates (AfterShip)",
    Veho: "Veho (AfterShip)",
    DoorDash: "DoorDash (original tracking link)",
  };
  for (const carrier of [...carriers, "FedExAccount"]) {
    const option = element("option", carrierLabels[carrier] || carrier);
    option.value = carrier;
    carrierField.append(option);
  }
  enabledProviders = new Set(system.enabled_providers);
  byId("login").hidden = true;
  byId("startup-connection").hidden = true;
  byId("dashboard").hidden = false;
  byId("add").hidden = false;
  setOwnerAccess(system.credential_role === "owner", true);
  renderServiceStatus(system, false);
  connected = true;
}

async function openDashboard() {
  try {
    await applyRoute(true);
    initialRoutePending = false;
  } catch (error) {
    notice(`Could not load shipments. ${error.message} The list will retry automatically.`, true);
  } finally {
    scheduleRefresh();
  }
}

async function startConnection() {
  connectionController?.abort();
  if (document.hidden) return;
  const controller = new AbortController();
  connectionController = controller;
  if (homeAssistant) {
    byId("login").hidden = true;
    byId("startup-connection").hidden = false;
    byId("connection-status").textContent = "Waiting for Home Assistant to connect. Your shipments will open automatically.";
  }
  try {
    await waitForConnection(connect, {
      homeAssistant,
      signal: controller.signal,
      onWaiting(error) {
        byId("connection-status").textContent = error.status === 401
          ? "Waiting for Home Assistant to authenticate this view. Retrying automatically…"
          : error.status === 403
            ? "Home Assistant has not granted access to this view. Check your access to Shipments; we’ll keep retrying."
            : "Shipments is reconnecting after startup. Retrying automatically…";
      },
    });
    if (!controller.signal.aborted) await openDashboard();
  } catch (error) {
    if (controller.signal.aborted) return;
    byId("login").hidden = false;
    if (![401, 403].includes(error.status)) notice(error.message, true);
  }
}

function setOwnerAccess(owner, reset = false) {
  if (showInstallation === owner && !reset) return;
  showInstallation = owner;
  // Role and connection changes invalidate pending credential responses and
  // clear any one-time token before privileged controls can disappear.
  agentSession += 1;
  agentCredentials = [];
  clearAgentToken();
  byId("agent-access").hidden = !showInstallation;
  byId("installation").hidden = !showInstallation;
  byId("agent-list").replaceChildren();
  byId("agent-status").textContent = "";
  byId("agent-error").textContent = "";
  byId("create-agent").textContent = "Create credential";
  setAgentBusy(false);
  installationInitialized = false;
  installationNeedsAttention = false;
  byId("installation-checks").replaceChildren();
  byId("installation-summary").textContent = owner ? "Checking…" : "";
  byId("backup-date").textContent = "";
  if (!owner) {
    byId("agent-access").open = false;
    byId("installation").open = false;
  } else if (byId("agent-access").open) {
    loadAgents();
  }
  byId("correction").hidden = !owner;
  for (const button of byId("actions").querySelectorAll("[data-owner-only]"))
    button.hidden = !owner;
}

async function refreshInstallation(generation, signal) {
  const session = agentSession;
  try {
    const report = await api("v1/installation", { signal });
    if (signal.aborted || generation !== listGeneration || session !== agentSession || !showInstallation) return;
    renderInstallation(report);
  } catch (error) {
    if (signal.aborted || generation !== listGeneration || session !== agentSession) return;
    if ([401, 403].includes(error.status)) {
      setOwnerAccess(false);
      return;
    }
    installationNeedsAttention = true;
    byId("installation-summary").textContent = "Checks unavailable";
    const row = element("li", undefined, "check-attention");
    row.append(element("strong", "Installation checks unavailable"), element("p", error.message));
    byId("installation-checks").replaceChildren(row);
  }
}

async function load(append = false) {
  if (append && (!nextPage || loadingMore)) return;
  listController?.abort();
  const controller = new AbortController();
  listController = controller;
  byId("refresh").disabled = true;
  byId("refresh").textContent = "Refreshing…";
  byId("list").setAttribute("aria-busy", "true");
  const generation = append ? listGeneration : ++listGeneration;
  const view = byId("view").value;
  const search = appliedSearch;
  const query = new URLSearchParams({ limit: "50" });
  if (append) {
    loadingMore = true;
    byId("more").disabled = true;
    query.set("page_token", nextPage);
  } else {
    nextPage = null;
    loadingMore = false;
    byId("more").hidden = true;
    byId("more").disabled = false;
    if (activeDirection !== "all") query.set("direction", activeDirection);
    query.set("archived", String(view === "archived"));
    query.set("sort", ["delivered", "delivered_today"].includes(view) ? "delivered" : "expected");
    if (view === "active") query.set("active", "true");
    if (view === "delivered") query.set("status", "delivered");
    if (view === "out_for_delivery") query.set("status", "out_for_delivery");
    if (view === "exceptions") query.set("exceptions", "true");
    if (view === "tracking_issues") query.set("tracking_issues", "true");
    if (search) query.set("search", search);
  }

  try {
    const systemRequest = api("v1/system", { signal: controller.signal }).then(system => {
      if (!controller.signal.aborted && generation === listGeneration)
        setOwnerAccess(system.credential_role === "owner");
      return system;
    });
    // Home Assistant supplies its own day window, including timezone and DST.
    // A remote browser must not substitute a different local calendar day.
    const deliveryDay = view === "delivered_today" ? (await systemRequest).delivery_day : null;
    if (controller.signal.aborted || generation !== listGeneration) return;
    const deliveryWindow = deliveryDay ? `${deliveryDay.delivered_after}:${deliveryDay.delivered_before}` : null;
    if (append && view === "delivered_today" && deliveryWindow !== shownDeliveryWindow)
      return load();
    if (deliveryDay && !append) {
      query.set("delivered_after", deliveryDay.delivered_after);
      query.set("delivered_before", deliveryDay.delivered_before);
    }
    const [summary, page, system] = await Promise.all([
      api(activeDirection === "all" ? "v1/summary" : `v1/summary?direction=${activeDirection}`, { signal: controller.signal }),
      view === "delivered_today" && !deliveryDay ? { items: [], next_page: null }
        : api(`v1/shipments?${query}`, { signal: controller.signal }),
      systemRequest,
    ]);
    // A search or archive-filter change owns its whole result, including any
    // pagination requests already in flight for the previous list.
    if (generation !== listGeneration) return;
    shownDeliveryWindow = deliveryWindow;
    byId("view-note").hidden = !deliveryDay;
    byId("view-note").textContent = deliveryDay
      ? `Delivered today, ${new Date(`${deliveryDay.date}T12:00:00Z`).toLocaleDateString([], { timeZone: "UTC", month: "short", day: "numeric", year: "numeric" })} · ${deliveryDay.time_zone}`
      : "";
    enabledProviders = new Set(system.enabled_providers);
    if (showInstallation) refreshInstallation(generation, controller.signal);
    renderServiceStatus(system, true);
    for (const button of byId("directions").querySelectorAll("button"))
      button.setAttribute("aria-pressed", String(button.dataset.direction === activeDirection));
    const stats = byId("summary");
    const focusedStat = document.activeElement?.dataset?.stat;
    stats.replaceChildren();
    for (const [key, label] of [
      ["active", "Active"],
      ["out_for_delivery", "Out for delivery"],
      ["delivered", "Delivered"],
      ["exceptions", "Exceptions"],
      ["tracking_issues", "Tracking issues"],
    ]) {
      const count = key === "tracking_issues" ? system.sync_errors : summary[key];
      const card = element("button", undefined, `stat stat-${key}${key === "tracking_issues" && count > 0 ? " has-issues" : ""}`);
      card.type = "button";
      card.dataset.stat = key;
      card.setAttribute("aria-pressed", String(view === key));
      card.setAttribute("aria-label", key === "tracking_issues"
        ? `${count} shipments with tracking issues, all directions`
        : `${count} ${label.toLowerCase()} shipments`);
      card.append(element("strong", count), element("span", label));
      if (key === "tracking_issues") card.append(element("small", "All directions"));
      card.addEventListener("click", guarded(() => {
        byId("view").value = key;
        if (key === "tracking_issues") {
          activeDirection = "all";
          appliedSearch = "";
          byId("query").value = "";
        }
        saveListRoute();
        return load();
      }));
      stats.append(card);
      if (focusedStat === key) card.focus({ preventScroll: true });
    }

    const list = byId("list");
    if (!append) list.replaceChildren();
    for (const shipment of page.items) {
      const row = element("button", undefined, `shipment status-${shipment.status}`);
      const copy = element("div");
      const description = [
        shipment.merchant,
        shipment.order_reference,
      ]
        .filter(Boolean)
        .join(" · ");
      copy.append(element("h2", shipment.title));
      if (description) copy.append(element("p", description));
      copy.append(element("p", `${shipment.tracking.carrier || "Carrier not identified"} · ${shipment.tracking.number}`, "shipment-tracking"));
      const delivered = shipment.status === "delivered" || view === "delivered_today";
      const deliveryDate = delivered
        ? deliveredDate(shipment)
        : shipment.carrier_tracking?.estimated_delivery_at;
      copy.append(element("p", deliveryDate
        ? `${delivered ? "Delivered" : "Expected"} ${calendarDate(deliveryDate)}`
        : delivered ? "Delivery date not reported" : "Delivery date not reported yet", "shipment-date"));
      const badges = shipmentBadges(shipment, "shipment-badges");
      row.append(copy, badges);
      row.addEventListener(
        "click",
        guarded(async () => {
          userNavigation();
          const generation = await show(shipment.id, false, shipment.sync.last_error ? "connection" : null);
          if (shipment.sync.last_error && currentDetail(shipment.id, generation)) {
            byId("connection").open = true;
            byId("connection").scrollIntoView({ block: "start" });
          }
        }),
      );
      list.append(row);
    }
    if (!list.children.length) {
      const empty = element("div", undefined, "empty");
      const messages = {
        active: [
          "No active shipments.",
          "Add an expected delivery, or switch to All to review completed shipments.",
        ],
        delivered: [
          "No delivered shipments yet.",
          "Received a parcel? Open it from All and mark it delivered.",
        ],
        delivered_today: deliveryDay
          ? ["Nothing delivered today.", "Deliveries appear here when the carrier reports a delivery or you confirm one."]
          : ["Today's deliveries are unavailable.", "Connect Home Assistant to use its timezone and delivery-day window."],
        out_for_delivery: ["Nothing out for delivery.", "Shipments appear here when the carrier starts the delivery run."],
        exceptions: ["No delivery exceptions.", "Carrier-reported exceptions and returns appear here."],
        tracking_issues: ["No tracking issues.", "Tracking errors appear here. New registrations in progress are not errors."],
        all: [
          "No shipments yet.",
          "Use Add shipment when you have a tracking number. New labels do not need a carrier scan.",
        ],
        archived: [
          "No archived shipments.",
          "Archived shipments are kept here and can be restored from their details.",
        ],
      };
      const [heading, description] = search && !(view === "delivered_today" && !deliveryDay)
        ? [
            "No shipments match your search.",
            "Try a different title, merchant, or order reference in this view.",
          ]
        : messages[view];
      empty.append(element("h2", heading), element("p", description));
      if (search) {
        const clear = element("button", "Clear search", "quiet");
        clear.addEventListener(
          "click",
          guarded(() => {
            appliedSearch = "";
            byId("query").value = "";
            saveListRoute();
            return load();
          }),
        );
        empty.append(clear);
      }
      list.append(empty);
    }
    nextPage = page.next_page;
    byId("more").hidden = !nextPage;
  } catch (error) {
    if (generation === listGeneration && error.name !== "AbortError")
      throw error;
  } finally {
    if (generation === listGeneration) {
      loadingMore = false;
      byId("more").disabled = false;
      byId("refresh").disabled = false;
      byId("refresh").textContent = "Refresh";
      byId("list").setAttribute("aria-busy", "false");
      listController = null;
    }
  }
}

function renderInstallation(report) {
  const list = byId("installation-checks");
  list.replaceChildren();
  let attention = 0;
  for (const [key, label] of [
    ["backend", "App and database"],
    ["integration", "Home Assistant"],
    ["version", "App / integration versions"],
    ["authentication", "Authentication"],
    ["easypost", "EasyPost"],
    ["backup", "Backup"],
  ]) {
    const check = report[key];
    const needsAttention = ["attention", "unavailable"].includes(check.status);
    if (needsAttention) attention += 1;
    const row = element("li", undefined, needsAttention ? "check-attention" : "");
    const state = {
      pass: "Passed",
      attention: "Needs attention",
      unavailable: "Unavailable",
      not_applicable: "Optional",
    }[check.status];
    row.append(
      element("strong", label),
      element("span", state, "check-state"),
      element("p", check.message),
    );
    if (check.checked_at) {
      row.append(element("p", `Last checked: ${new Date(check.checked_at * 1000).toLocaleString()}.`, "muted"));
    }
    list.append(row);
  }
  byId("backup-date").textContent = report.last_backup
    ? `Last covering backup: ${new Date(report.last_backup * 1000).toLocaleString()}.`
    : "";
  byId("installation-summary").textContent = attention
    ? `${attention} need attention`
    : "Checks passed";
  const installation = byId("installation");
  installationNeedsAttention = attention > 0;
  if (attention > 0) byId("dashboard").insertBefore(installation, byId("summary"));
  else byId("management").insertBefore(installation, byId("agent-access"));
  installation.hidden = false;
  if (!installationInitialized) {
    const params = route();
    byId("installation").open = params.has("installation")
      ? params.get("installation") === "open" : attention > 0;
    installationInitialized = true;
  }
}

byId("check-installation").addEventListener("click", guarded(async () => {
  const session = agentSession;
  const button = byId("check-installation");
  button.disabled = true;
  button.textContent = "Checking…";
  try {
    const report = await api("v1/installation/check", { method: "POST" });
    if (session === agentSession && showInstallation) renderInstallation(report);
  } catch (error) {
    if (session !== agentSession) return;
    if ([401, 403].includes(error.status)) setOwnerAccess(false);
    else throw error;
  } finally {
    button.disabled = false;
    button.textContent = "Run checks";
  }
}));

function clearAgentToken() {
  agentTokenGeneration += 1;
  issuedAgentId = null;
  byId("agent-token").value = "";
  byId("agent-token").type = "password";
  byId("agent-secret").hidden = true;
  byId("agent-copy-status").textContent = "";
  byId("show-agent-token").textContent = "Show token";
  byId("show-agent-token").setAttribute("aria-pressed", "false");
}

function setAgentBusy(busy) {
  agentBusy = busy;
  byId("create-agent").disabled = busy;
  byId("agent-name").disabled = busy;
  byId("refresh-agents").disabled = busy;
  byId("agent-list").setAttribute("aria-busy", String(busy));
  for (const button of byId("agent-list").querySelectorAll("button")) {
    button.disabled = busy;
  }
}

function renderAgents() {
  const list = byId("agent-list");
  list.replaceChildren();
  if (!agentCredentials.length) {
    list.append(
      element("li", "No agent credentials. Create one when you are ready to connect an agent.", "muted"),
    );
    return;
  }
  for (const credential of agentCredentials) {
    const row = element("li", undefined, "agent-credential");
    const description = element("div");
    const created = element(
      "time",
      `Created ${new Date(credential.created_at).toLocaleString()}`,
    );
    created.dateTime = credential.created_at;
    description.append(element("strong", credential.name), created);
    const revoke = element("button", "Revoke", "quiet");
    revoke.type = "button";
    revoke.disabled = agentBusy;
    revoke.setAttribute("aria-label", `Revoke access for ${credential.name}`);
    revoke.addEventListener("click", () => revokeAgent(credential, revoke));
    row.append(description, revoke);
    list.append(row);
  }
}

async function loadAgents() {
  if (!showInstallation || agentBusy) return;
  const session = agentSession;
  setAgentBusy(true);
  byId("agent-error").textContent = "";
  byId("agent-status").textContent = "Loading agent credentials…";
  try {
    const page = await api("v1/agents");
    if (session !== agentSession) return;
    agentCredentials = page.items;
    if (
      issuedAgentId &&
      !agentCredentials.some((item) => item.id === issuedAgentId)
    ) {
      clearAgentToken();
      byId("agent-status").textContent = "The displayed credential was revoked. Its token has been cleared.";
    } else {
      byId("agent-status").textContent = "";
    }
    renderAgents();
  } catch (error) {
    if (session === agentSession) {
      if ([401, 403].includes(error.status)) {
        setOwnerAccess(false);
        return;
      }
      byId("agent-status").textContent = "";
      byId("agent-error").textContent = `Could not load credentials. ${error.message} Use Refresh credentials to retry.`;
    }
  } finally {
    if (session === agentSession) setAgentBusy(false);
  }
}

async function revokeAgent(credential, button) {
  if (!showInstallation || agentBusy) return;
  const session = agentSession;
  setAgentBusy(true);
  button.textContent = "Revoking…";
  byId("agent-error").textContent = "";
  byId("agent-status").textContent = `Revoking access for ${credential.name}…`;
  try {
    await api(`v1/agents/${credential.id}`, { method: "DELETE" });
    if (session !== agentSession) return;
    agentCredentials = agentCredentials.filter((item) => item.id !== credential.id);
    if (issuedAgentId === credential.id) clearAgentToken();
    renderAgents();
    byId("agent-status").textContent = `Access revoked for ${credential.name}.`;
  } catch (error) {
    if (session === agentSession) {
      byId("agent-status").textContent = "";
      byId("agent-error").textContent = `Could not confirm revocation for ${credential.name}. ${error.message} Refresh credentials to check before retrying.`;
    }
  } finally {
    if (session === agentSession) {
      button.textContent = "Revoke";
      setAgentBusy(false);
    }
  }
}

byId("agent-access").addEventListener("toggle", () => {
  if (byId("agent-access").open) {
    loadAgents();
  } else {
    const hadToken = Boolean(issuedAgentId);
    clearAgentToken();
    if (hadToken) byId("agent-status").textContent = "Token cleared from this tab. If you did not save it, revoke that credential and create a new one.";
  }
});

byId("refresh-agents").addEventListener("click", loadAgents);
byId("agent-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (!showInstallation || agentBusy) return;
  const name = byId("agent-name").value.trim();
  if (!name) {
    byId("agent-error").textContent = "Enter a name for this agent.";
    byId("agent-name").focus();
    return;
  }
  const session = agentSession;
  const hadToken = Boolean(issuedAgentId);
  clearAgentToken();
  const generation = agentTokenGeneration;
  setAgentBusy(true);
  byId("create-agent").textContent = "Creating…";
  byId("agent-error").textContent = "";
  byId("agent-status").textContent = "Creating agent credential…";
  try {
    const created = await api("v1/agents", {
      method: "POST",
      body: JSON.stringify({ name }),
    });
    if (session !== agentSession) return;
    agentCredentials.unshift(created.credential);
    renderAgents();
    byId("agent-form").reset();
    if (generation === agentTokenGeneration && byId("agent-access").open) {
      issuedAgentId = created.credential.id;
      byId("agent-secret-title").textContent = `Save the token for ${created.credential.name}`;
      byId("agent-token").value = created.token;
      byId("agent-secret").hidden = false;
      byId("agent-status").textContent = "Credential created. Copy the token now; it cannot be shown again.";
      byId("agent-token").focus();
    } else {
      byId("agent-status").textContent = `${created.credential.name} was created, but its one-time token was dismissed. Revoke it and create a new credential.`;
    }
  } catch (error) {
    if (session === agentSession) {
      byId("agent-status").textContent = "";
      byId("agent-error").textContent = `Could not confirm credential creation. ${error.message} Refresh credentials before retrying; revoke any new credential whose token you did not receive.${hadToken ? " The previous token was cleared. If you did not save it, revoke that credential and create another." : ""}`;
    }
  } finally {
    if (session === agentSession) {
      byId("create-agent").textContent = "Create credential";
      setAgentBusy(false);
    }
  }
});

byId("copy-agent-token").addEventListener("click", async () => {
  const input = byId("agent-token");
  if (!input.value) return;
  const generation = agentTokenGeneration;
  const button = byId("copy-agent-token");
  button.disabled = true;
  try {
    await navigator.clipboard.writeText(input.value);
    if (generation === agentTokenGeneration) byId("agent-copy-status").textContent = "Token copied.";
  } catch {
    if (generation !== agentTokenGeneration) return;
    input.type = "text";
    byId("show-agent-token").textContent = "Hide token";
    byId("show-agent-token").setAttribute("aria-pressed", "true");
    input.focus();
    input.select();
    byId("agent-copy-status").textContent = "Automatic copying is unavailable here. The token is selected; use your device’s Copy command.";
  } finally {
    button.disabled = false;
  }
});

byId("show-agent-token").addEventListener("click", () => {
  const input = byId("agent-token");
  const show = input.type === "password";
  input.type = show ? "text" : "password";
  byId("show-agent-token").textContent = show ? "Hide token" : "Show token";
  byId("show-agent-token").setAttribute("aria-pressed", String(show));
});

byId("dismiss-agent-token").addEventListener("click", () => {
  clearAgentToken();
  byId("agent-status").textContent = "Token dismissed. If you did not save it, revoke that credential and create a new one.";
  byId("agent-name").focus();
});
window.addEventListener("pagehide", clearAgentToken);

async function refreshDashboard() {
  if (listController || document.hidden || byId("dashboard").hidden) return;
  if (initialRoutePending) {
    await applyRoute(true);
    initialRoutePending = false;
    notice("");
    return;
  }
  await load();
  if (selected && byId("detail").open && !byId("editor").open &&
      !byId("override-form").elements.namedItem("reason").value.trim())
    await show(selected.id, true);
  notice("");
}

function scheduleRefresh() {
  clearTimeout(refreshTimer);
  if (document.hidden || byId("dashboard").hidden) return;
  refreshTimer = setTimeout(async () => {
    try {
      await refreshDashboard();
    } catch (error) {
      notice(
        `Could not refresh shipments. ${error.message} Use Refresh to retry.`,
        true,
      );
    } finally {
      scheduleRefresh();
    }
  }, 30_000);
}

function closeDetail(updateUrl = true) {
  if (updateUrl) writeRoute({ shipment: null, edit: null, section: null });
  detailGeneration += 1;
  selected = null;
  eventsPage = null;
  loadingEvents = false;
  if (byId("detail").open) byId("detail").close();
}

function closeEditor(updateUrl = true) {
  const reopen = updateUrl && !applyingRoute ? editing?.id : null;
  if (updateUrl) writeRoute({ edit: null });
  editorGeneration += 1;
  editing = null;
  createKey = null;
  submittedCreatePayload = null;
  setEditorSaving(false);
  if (byId("editor").open) byId("editor").close();
  if (reopen) guarded(() => show(reopen))();
}

function openEditor(shipment = null) {
  closeDetail(false);
  writeRoute({ edit: shipment ? "true" : "new", shipment: shipment?.id || null });
  editorGeneration += 1;
  editing = shipment;
  createKey = shipment ? null : crypto.randomUUID();
  submittedCreatePayload = null;
  setEditorSaving(false);
  const form = byId("edit-form");
  form.reset();
  byId("edit-error").textContent = "";
  byId("edit-title").textContent = shipment ? "Edit shipment" : "Add shipment";
  if (shipment) {
    for (const [key, value] of Object.entries({
      title: shipment.title,
      direction: shipment.direction,
      merchant: shipment.merchant,
      order_reference: shipment.order_reference,
      carrier: shipment.tracking?.carrier === "FedEx" ? "FedExAccount" : shipment.tracking?.carrier,
      number: shipment.tracking?.number,
      recipient_url: shipment.tracking?.recipient_url,
      signature_required: shipment.signature_required == null ? "" : String(shipment.signature_required),
    })) {
      form.elements.namedItem(key).value = value || "";
    }
  }
  updateRecipientField();
  byId("editor").showModal();
}

function updateRecipientField() {
  const form = byId("edit-form");
  const required = form.elements.namedItem("carrier").value === "DoorDash";
  const field = form.elements.namedItem("recipient_url");
  byId("recipient-url-field").hidden = !required;
  field.required = required;
  field.disabled = !required;
}

function setEditorSaving(saving) {
  savingEditor = saving;
  byId("shipment-fields").disabled = saving;
  byId("save").disabled = saving;
  byId("save").textContent = saving ? "Saving…" : "Save shipment";
  byId("edit-status").textContent = saving ? "Verifying tracking and saving your shipment." : "";
}

function currentDetail(id, generation) {
  return (
    generation === detailGeneration &&
    selected?.id === id &&
    byId("detail").open
  );
}

function carrierTrackingUrl(tracking) {
  if (tracking.carrier === "DoorDash") return recipientTrackingUrl(tracking.recipient_url);
  const carriers = new Map([
    ["usps", ["https://tools.usps.com/go/TrackConfirmAction", "tLabels"]],
    ["ups", ["https://www.ups.com/track", "tracknum"]],
    ["fedex", ["https://www.fedex.com/fedextrack/", "trknbr"]],
    ["fedexdefault", ["https://www.fedex.com/fedextrack/", "trknbr"]],
    ["fedexsmartpost", ["https://www.fedex.com/fedextrack/", "trknbr"]],
    ["dhl", ["https://www.dhl.com/us-en/home/tracking.html", "tracking-id"]],
    ["dhlexpress", ["https://www.dhl.com/us-en/home/tracking.html", "tracking-id"]],
    ["dhlexpressdefault", ["https://www.dhl.com/us-en/home/tracking.html", "tracking-id"]],
  ]);
  const template = carriers.get(tracking.carrier?.toLowerCase());
  if (!template) return null;
  const url = new URL(template[0]);
  url.searchParams.set(template[1], tracking.number);
  return url;
}

function recipientTrackingUrl(value) {
  if (!value) return null;
  try {
    const url = new URL(value);
    const codes = url.searchParams.getAll("urlCode");
    if (url.protocol === "https:" && url.host === "www.doordash.com" &&
        url.pathname === "/orders/drive" && !url.username && !url.password && !url.hash &&
        [...url.searchParams].length === 1 && codes.length === 1 && /^[A-Za-z0-9_-]{1,128}$/.test(codes[0]))
      return url;
  } catch {
    // Server validation owns admission; unsafe links are never rendered.
  }
  return null;
}

function providerTrackingUrl(provider, value) {
  if (!value) return null;
  if (provider === "doordash") return recipientTrackingUrl(value);
  const hosts = provider === "easypost" ? ["track.easypost.com"]
    : provider === "aftership" ? ["www.aftership.com", "track.aftership.com"] : [];
  try {
    const url = new URL(value);
    if (url.protocol === "https:" && hosts.includes(url.host) &&
        !url.username && !url.password) return url;
  } catch {
    // An absent or invalid public tracking URL does not block shipment details.
  }
  return null;
}

function dateLabel(value, fallback = "Not reported") {
  if (value == null || value === "") return fallback;
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? fallback : date.toLocaleString([], {
    month: "short", day: "numeric", hour: "numeric", minute: "2-digit",
  });
}


function renderCarrierHistory(tracking, provider) {
  const list = byId("carrier-events");
  list.replaceChildren();
  const events = [...(tracking?.events || [])].sort((left, right) =>
    (Date.parse(right.datetime) || 0) - (Date.parse(left.datetime) || 0));
  byId("carrier-history-note").textContent = tracking?.events_truncated
    ? "Showing the most recent 512 carrier events. Earlier events exceeded the history limit."
    : events.length ? "" : provider === "doordash"
      ? "DoorDash recipient tracking provides status and delivery dates here; scan history is unavailable."
      : "The carrier has not reported any scan events yet.";
  for (const event of events) {
    const item = element("li", undefined, `carrier-event status-${event.status || "unknown"}`);
    const heading = element("div", undefined, "event-heading");
    heading.append(statusBadge(event.status));
    const time = element("time", dateLabel(event.datetime, "Time not reported"));
    if (event.datetime) time.dateTime = event.datetime;
    heading.append(time);
    item.append(heading);
    if (event.message) item.append(element("p", event.message, "event-message"));
    if (event.description && event.description !== event.message)
      item.append(element("p", event.description, "event-description"));
    const location = event.tracking_location;
    const place = location ? [location.city, location.state, location.zip, location.country]
      .filter(Boolean).join(", ") : "";
    const details = [place, event.source, event.status_detail?.replaceAll("_", " "), event.carrier_code]
      .filter(Boolean).join(" · ");
    if (details) item.append(element("p", details, "event-meta"));
    if (location && (location.city || location.state)) {
      const query = [location.city, location.state, location.country].filter(Boolean).join(", ");
      const map = element("a", "View location on map ↗", "event-map");
      const url = new URL("https://www.openstreetmap.org/search");
      url.searchParams.set("query", query);
      map.href = url.href;
      map.target = "_blank";
      map.rel = "noopener noreferrer";
      item.append(map);
    }
    list.append(item);
  }
}

async function show(id, preserveView = false, section = route().get("section")) {
  writeRoute({ shipment: id, edit: null, section });
  const generation = ++detailGeneration;
  if (!preserveView) selected = null;
  let shipment;
  try {
    shipment = await api(`v1/shipments/${id}`);
  } catch (error) {
    if (generation === detailGeneration) throw error;
    return null;
  }
  if (generation !== detailGeneration) return null;
  if (preserveView && byId("override-form").elements.namedItem("reason").value.trim()) return generation;
  const view = preserveView && selected?.id === id ? {
    scroll: byId("detail").scrollTop,
    status: byId("override-form").elements.namedItem("status").value,
    disclosures: [...byId("detail").querySelectorAll("details[id]")].map(node => [node.id, node.open]),
  } : null;
  selected = shipment;
  byId("detail-title").textContent = shipment.title;
  const content = byId("detail-content");
  content.replaceChildren();
  const hero = element("div", undefined, "detail-hero");
  const badges = shipmentBadges(shipment, "detail-badges");
  hero.append(badges);
  const tracking = element("p", undefined, "detail-tracking");
  tracking.append(element("strong", shipment.tracking.carrier || "Carrier not identified"),
    element("span", shipment.tracking.number));
  hero.append(tracking);
  const links = element("div", undefined, "tracking-links");
  const linked = new Set();
  for (const [label, url] of [
    [shipment.sync.provider === "doordash" ? "DoorDash recipient tracking" : "Carrier tracking", carrierTrackingUrl(shipment.tracking)],
    [`${providerNames[shipment.sync.provider] || "Provider"} tracking`, providerTrackingUrl(shipment.sync.provider, shipment.sync.tracking_url)],
  ]) {
    if (!url || linked.has(url.href)) continue;
    linked.add(url.href);
    const link = element("a", `${label} ↗`);
    link.href = url.href;
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    links.append(link);
  }
  if (links.children.length) hero.append(links);
  content.append(hero);

  const overview = element("div", undefined, "detail-overview");
  const fact = (label, value) => {
    const card = element("div", undefined, "detail-fact");
    card.append(element("span", label), element("strong", value));
    overview.append(card);
  };
  if (shipment.status === "delivered") fact("Delivered", dateLabel(deliveredDate(shipment)));
  else fact("Expected delivery", shipment.carrier_tracking?.estimated_delivery_at
    ? calendarDate(shipment.carrier_tracking.estimated_delivery_at) : "Not reported yet");
  fact("Signature", signatureLabel(shipment));
  if (shipment.carrier_tracking?.signed_by) fact("Signed by", shipment.carrier_tracking.signed_by);
  if (shipment.merchant) fact("Merchant", shipment.merchant);
  if (shipment.order_reference) fact("Order reference", shipment.order_reference);
  content.append(overview);

  if (shipment.status_override) {
    const correction = element("div", undefined, "correction-notice");
    correction.append(element("strong", "Manually corrected"),
      element("p", shipment.status_override.reason),
      element("span", `Carrier reports: ${labels[shipment.carrier_status] || "Not reported"}`));
    content.append(correction);
  }

  const provider = element("details", undefined, "detail-disclosure");
  provider.id = "connection";
  const configuration = configurationIssue(shipment.sync);
  const configured = enabledProviders.has(shipment.sync.provider);
  const retrying = !configuration && shipment.sync.last_error && shipment.sync.next_attempt_at != null;
  const providerState = manuallyCompleted(shipment) ? "Stopped after manual completion" : configuration?.label || (shipment.sync.last_error ? retrying ? "Retry scheduled" : "Needs attention"
    : !configured ? "Not configured" : shipment.sync.last_success_at ? "Connected" : "Waiting for first check");
  provider.append(element("summary", `Tracking connection · ${providerState}`));
  if (shipment.sync.last_error) {
    const issue = element("div", undefined, `tracking-problem${retrying ? " retrying" : ""}`);
    issue.append(element("strong", configuration?.label || (retrying ? "Tracking will retry automatically" : "Tracking needs attention")),
      element("p", shipment.sync.last_error));
    const next = shipment.sync.next_attempt_at;
    issue.append(element("p", configuration
      ? configuration.instruction
      : !configured
      ? "Your shipment is saved. Configure the tracking provider to resume updates."
      : retrying
        ? next * 1000 > Date.now() ? `Your shipment is saved. Next retry ${dateLabel(next * 1000)}.`
          : "Your shipment is saved. Waiting for the next retry."
        : "Your shipment is saved. Review the carrier and provider support before retrying.", "retry-timing"));
    provider.append(issue);
  }
  const fields = element("dl", undefined, "shipment-details");
  const detail = (label, value) => fields.append(element("dt", label), element("dd", value));
  detail("Provider", providerNames[shipment.sync.provider] || "Tracking provider");
  detail("Registration", shipment.sync.registration_state === "registered" ? "Registered" : manuallyCompleted(shipment) ? "Not registered; tracking stopped" : shipment.sync.registration_state === "configuration_required" ? "Configuration required; saved locally" : shipment.sync.registration_state === "rejected" ? "Needs attention; review provider error" : "Pending registration; saved locally");
  if (shipment.sync.error_code) detail("Error code", shipment.sync.error_code);
  if (shipment.sync.provider_error_code) detail("Provider error code", shipment.sync.provider_error_code);
  if (shipment.sync.provider_status) detail("Provider response", `HTTP ${shipment.sync.provider_status}`);
  detail("Last successful sync", dateLabel(shipment.sync.last_success_at, "Not checked"));
  detail("Next check", shipment.sync.next_attempt_at == null ? "Not scheduled"
    : dateLabel(shipment.sync.next_attempt_at * 1000));
  if (shipment.sync.tracker_id) detail("Tracker ID", shipment.sync.tracker_id);
  provider.append(fields);
  provider.open = Boolean(shipment.sync.last_error);
  content.append(provider);
  linkDisclosure(provider);

  byId("sources").replaceChildren();
  if (shipment.sources.length) {
    const sources = element("details", undefined, "detail-disclosure");
    sources.id = "source-details";
    sources.append(element("summary", `Sources (${shipment.sources.length})`));
    for (const source of shipment.sources)
      sources.append(element("p", `${source.namespace} / ${source.key}`, "detail-item"));
    byId("sources").append(sources);
    linkDisclosure(sources);
  }
  renderCarrierHistory(shipment.carrier_tracking, shipment.sync.provider);

  const actions = byId("actions");
  actions.replaceChildren();
  const button = (label, action, ownerOnly = false) => {
    const node = element("button", label, "quiet");
    if (ownerOnly) node.dataset.ownerOnly = "true";
    node.hidden = ownerOnly && !showInstallation;
    node.addEventListener("click", guarded(() => { userNavigation(); return action(); }));
    actions.append(node);
  };
  button("Edit", () => openEditor(shipment));
  if (shipment.tracking && !shipment.archived)
    button("Refresh tracking", () => mutate("refresh", "POST"));
  if (shipment.status_override)
    button("Clear correction", () => mutate("status-override", "DELETE"), true);
  button(shipment.archived ? "Restore" : "Archive", () =>
    mutate("archive", shipment.archived ? "DELETE" : "PUT"), true,
  );
  if (!preserveView) {
    for (const id of ["correction", "activity"]) byId(id).open = route().get("section") === id;
  }
  byId("correction").hidden = !showInstallation;
  byId("override-form").reset();
  byId("detail-error").textContent = "";
  byId("events").replaceChildren();
  eventsPage = null;
  loadingEvents = false;
  byId("more-events").hidden = true;
  byId("more-events").disabled = false;
  if (!byId("detail").open) byId("detail").showModal();
  await loadEvents();
  if (view && currentDetail(id, generation)) {
    for (const [sectionId, open] of view.disclosures) {
      if (byId(sectionId)) byId(sectionId).open = open;
    }
    byId("override-form").elements.namedItem("status").value = view.status;
    byId("detail").scrollTop = view.scroll;
  }
  return generation;
}

async function loadEvents() {
  if (!selected || loadingEvents || !byId("detail").open) return;
  const id = selected.id;
  const generation = detailGeneration;
  const query = new URLSearchParams({ limit: "50" });
  if (eventsPage) query.set("page_token", eventsPage);
  loadingEvents = true;
  byId("more-events").disabled = true;
  try {
    const page = await api(`v1/shipments/${id}/events?${query}`);
    if (!currentDetail(id, generation)) return;
    for (const event of page.items) {
      const item = element("li", `${event.detail} · ${event.actor_id}`);
      const time = element("time", new Date(event.at).toLocaleString());
      time.dateTime = event.at;
      item.append(time);
      if (event.snapshot) {
        const snapshot = event.snapshot;
        const details = element("details", undefined, "history-snapshot");
        const fields = element("dl");
        const values = [
          ["Title", snapshot.title],
          ["Status", labels[snapshot.status]],
          [
            "Tracking",
            snapshot.tracking
              ? `${snapshot.tracking.carrier || "Auto-detect carrier"} · ${snapshot.tracking.number}`
              : "Not set",
          ],
          ["Merchant", snapshot.merchant || "Not set"],
          ["Order reference", snapshot.order_reference || "Not set"],
          [
            "Carrier status",
            snapshot.carrier_status
              ? labels[snapshot.carrier_status]
              : "Not checked",
          ],
          [
            "Manual correction",
            snapshot.status_override
              ? `${labels[snapshot.status_override.status]} · ${snapshot.status_override.reason} (${snapshot.status_override.actor_id})`
              : "None",
          ],
          ["Archived", snapshot.archived ? "Yes" : "No"],
          ["Revision", String(snapshot.revision)],
        ];
        for (const [label, value] of values) {
          fields.append(element("dt", label), element("dd", value));
        }
        details.append(element("summary", "Record at this point"), fields);
        item.append(details);
      }
      byId("events").append(item);
    }
    eventsPage = page.next_page;
    byId("more-events").textContent = "More history";
    byId("more-events").hidden = !eventsPage;
  } catch (error) {
    if (currentDetail(id, generation)) {
      byId("detail-error").textContent = error.message;
      byId("more-events").textContent = "Retry history";
      byId("more-events").hidden = false;
    }
  } finally {
    if (currentDetail(id, generation)) {
      loadingEvents = false;
      byId("more-events").disabled = false;
    }
  }
}

async function mutate(path, method, body) {
  if (!selected || !byId("detail").open) return;
  const { id, revision } = selected;
  const generation = detailGeneration;
  try {
    await api(`v1/shipments/${id}/${path}`, {
      method,
      headers: { "If-Match": `"${revision}"` },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    const refreshedGeneration = currentDetail(id, generation)
      ? await show(id)
      : null;
    await load();
    if (currentDetail(id, refreshedGeneration)) notice("Saved.");
  } catch (error) {
    if (currentDetail(id, generation))
      byId("detail-error").textContent = editError(error);
  }
}

byId("add").addEventListener("click", () => { userNavigation(); openEditor(); });
byId("search").addEventListener(
  "submit",
  guarded(() => {
    appliedSearch = byId("query").value.trim();
    saveListRoute();
    return load();
  }),
);
byId("view").addEventListener(
  "change",
  guarded(() => { saveListRoute(); return load(); }),
);
byId("refresh").addEventListener("click", guarded(refreshDashboard));
document.addEventListener("visibilitychange", () => {
  clearTimeout(refreshTimer);
  if (!connected) {
    if (document.hidden) connectionController?.abort();
    else startConnection();
    return;
  }
  if (!document.hidden) {
    guarded(async () => {
      try {
        await refreshDashboard();
      } finally {
        scheduleRefresh();
      }
    })();
  }
});
byId("more").addEventListener(
  "click",
  guarded(() => load(true)),
);
byId("more-events").addEventListener("click", guarded(loadEvents));
byId("edit-form").elements.namedItem("carrier").addEventListener("change", updateRecipientField);
byId("login").addEventListener(
  "submit",
  guarded(async () => {
    token = byId("login").elements.namedItem("token").value;
    await connect();
    byId("login").reset();
    notice("");
    await openDashboard();
  }),
);

document.querySelectorAll("[data-close]").forEach((button) => {
  button.addEventListener("click", () => {
    userNavigation();
    if (button.dataset.close === "detail") closeDetail();
    else closeEditor();
  });
});
for (const [id, close] of [["detail", closeDetail], ["editor", closeEditor]]) {
  const dialog = byId(id);
  let pressedBackdrop = false;
  const onBackdrop = (event) => {
    const bounds = dialog.getBoundingClientRect();
    return event.target === dialog &&
      (event.clientX < bounds.left || event.clientX > bounds.right ||
       event.clientY < bounds.top || event.clientY > bounds.bottom);
  };
  dialog.addEventListener("pointerdown", (event) => {
    pressedBackdrop = onBackdrop(event);
  });
  dialog.addEventListener("pointercancel", () => { pressedBackdrop = false; });
  dialog.addEventListener("click", (event) => {
    // Releasing a drag outside or clicking the dialog's padding keeps it open.
    const dismiss = pressedBackdrop && onBackdrop(event);
    pressedBackdrop = false;
    if (!dismiss) return;
    userNavigation();
    close();
  });
}
// Invalidate on Escape before its queued close event. Close events belonging
// to a dismissed dialog must not cancel a newer detail request still loading.
byId("detail").addEventListener("cancel", (event) => {
  event.preventDefault();
  userNavigation();
  closeDetail();
});
byId("editor").addEventListener("cancel", (event) => {
  event.preventDefault();
  userNavigation();
  closeEditor();
});
byId("detail").addEventListener("close", () => {
  if (!byId("detail").open && selected) closeDetail();
});
byId("editor").addEventListener("close", () => {
  if (!byId("editor").open && (editing || createKey)) closeEditor();
});

byId("edit-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (savingEditor || !byId("editor").open) return;
  const generation = editorGeneration;
  const shipment = editing;
  const form = event.target;
  const values = Object.fromEntries(new FormData(form));
  const input = {
    title: values.title,
    direction: values.direction,
    merchant: values.merchant || null,
    order_reference: values.order_reference || null,
    signature_required: values.signature_required === "" ? null : values.signature_required === "true",
    tracking: {
      carrier: values.carrier.trim() || null,
      number: values.number.trim(),
      recipient_url: values.carrier === "DoorDash" ? values.recipient_url.trim() || null : null,
    },
  };
  const payload = JSON.stringify(input);
  if (!shipment) {
    if (submittedCreatePayload !== null && submittedCreatePayload !== payload)
      createKey = crypto.randomUUID();
    submittedCreatePayload = payload;
  }
  const key = createKey;
  byId("edit-error").textContent = "";
  setEditorSaving(true);
  try {
    const headers = shipment
      ? { "If-Match": `"${shipment.revision}"` }
      : { "Idempotency-Key": key };
    const saved = await api(shipment ? `v1/shipments/${shipment.id}` : "v1/shipments", {
      method: shipment ? "PATCH" : "POST",
      headers,
      body: payload,
    });
    const current = generation === editorGeneration && byId("editor").open;
    if (current) {
      closeEditor(false);
      await show(saved.id);
    }
    await load();
    if (current) notice("Shipment saved.");
  } catch (error) {
    if (generation === editorGeneration && byId("editor").open)
      byId("edit-error").textContent = editError(error);
  } finally {
    if (generation === editorGeneration) {
      setEditorSaving(false);
    }
  }
});

byId("override-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const values = Object.fromEntries(new FormData(event.target));
  await mutate("status-override", "PUT", values);
});

function linkDisclosure(disclosure) {
  disclosure.querySelector("summary").addEventListener("click", () => {
    userNavigation();
    requestAnimationFrame(() => {
      if (!byId("detail").open) return;
      if (disclosure.open) writeRoute({ section: disclosure.id });
      else if (route().get("section") === disclosure.id) writeRoute({ section: null });
    });
  });
}

for (const id of ["correction", "activity"]) linkDisclosure(byId(id));
for (const [id, key] of [["installation", "installation"], ["agent-access", "agents"]]) {
  byId(id).querySelector("summary").addEventListener("click", () => {
    userNavigation();
    requestAnimationFrame(() => writeRoute({ [key]: byId(id).open ? "open" : "closed" }));
  });
}

for (const button of byId("directions").querySelectorAll("button")) {
  button.addEventListener("click", guarded(() => {
    activeDirection = button.dataset.direction;
    saveListRoute();
    return load();
  }));
}
window.addEventListener("hashchange", guarded(() => applyRoute()));

window.addEventListener("message", guarded(async event => {
  if (window.parent === window || event.source !== window.parent || event.origin !== location.origin) return;
  const data = event.data;
  if (!data || typeof data !== "object" || data.type !== "home-assistant/properties") return;
  const parentRoute = data.route;
  if (!parentRoute || typeof parentRoute.prefix !== "string" || typeof parentRoute.path !== "string") return;
  if (!parentRoute.prefix.startsWith("/") || parentRoute.prefix.startsWith("//") ||
      /[?#]/.test(parentRoute.prefix) || parentRoute.prefix.length > 500 || parentRoute.path.length > 12000) return;
  homeAssistantPrefix = parentRoute.prefix.replace(/\/$/, "");
  homeAssistantPath = parentRoute.path;
  let hash = "";
  if (parentRoute.path.startsWith("/shipments/")) {
    try { hash = decodeURIComponent(parentRoute.path.slice("/shipments/".length)); }
    catch { return; }
    if (hash.length > 4096) return;
  } else if (parentRoute.path !== "/" && parentRoute.path !== "") return;
  if (hash === location.hash.slice(1)) return;
  history.replaceState(null, "", hash ? `#${hash}` : location.pathname + location.search);
  if (connected) await applyRoute();
}));
if (window.parent !== window)
  window.parent.postMessage({ type: "home-assistant/subscribe-properties" }, location.origin);
window.addEventListener("pagehide", () => {
  connectionController?.abort();
  clearTimeout(refreshTimer);
  if (window.parent !== window)
    window.parent.postMessage({ type: "home-assistant/unsubscribe-properties" }, location.origin);
});
window.addEventListener("pageshow", event => {
  if (!event.persisted) return;
  if (!connected) startConnection();
  else scheduleRefresh();
});
byId("retry-connection").addEventListener("click", startConnection);

byId("add").hidden = true;
startConnection();
