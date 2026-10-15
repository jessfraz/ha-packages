"use strict";

const byId = (id) => document.getElementById(id);
const labels = {
  expected: "Expected",
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

let token = "";
let selected = null;
let editing = null;
let nextPage = null;
let eventsPage = null;
let createKey = null;
let listGeneration = 0;
let detailGeneration = 0;
let editorGeneration = 0;
let loadingMore = false;
let loadingEvents = false;
let savingEditor = false;
let appliedSearch = "";
let activeDirection = "all";
let routeGeneration = 0;
let applyingRoute = false;
let connected = false;
let homeAssistantPrefix = null;
let homeAssistantPath = null;
let listController = null;
let refreshTimer = null;
let showInstallation = false;
let providerEnabled = false;
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

function shipmentBadges(shipment, className) {
  const badges = element("div", undefined, className);
  const direction = shipment.direction === "outbound" ? "outbound" : "inbound";
  badges.append(element("span", direction === "inbound" ? "Inbound" : "Outbound", `badge direction-${direction}`));
  badges.append(statusBadge(shipment.status));
  if (shipment.sync.registration_state === "pending")
    badges.append(element("span", "Registration pending", "badge registration-pending"));
  if (shipment.sync.registration_state === "rejected")
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
  const params = route();
  const direction = ["inbound", "outbound"].includes(params.get("direction")) ? params.get("direction") : "all";
  const requestedView = params.get("view") || "active";
  const view = [...byId("view").options].some(option => option.value === requestedView) ? requestedView : "active";
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
      if (disclosure) disclosure.open = section === sectionId;
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

async function connect() {
  const [system, carriers] = await Promise.all([api("v1/system"), api("v1/carriers")]);
  const carrierField = byId("edit-form").elements.namedItem("carrier");
  carrierField.replaceChildren(element("option", "Auto-detect"));
  carrierField.firstElementChild.value = "";
  const carrierLabels = {
    FedEx: "FedEx (your carrier account)",
    FedExDefault: "FedEx Default (EasyPost account)",
    DHLExpress: "DHL Express (your carrier account)",
    DHLExpressDefault: "DHL Express Default (EasyPost account)",
  };
  for (const carrier of carriers) {
    const option = element("option", carrierLabels[carrier] || carrier);
    option.value = carrier;
    carrierField.append(option);
  }
  providerEnabled = system.provider_enabled;
  byId("login").hidden = true;
  byId("dashboard").hidden = false;
  byId("add").hidden = false;
  showInstallation = system.credential_role === "owner";
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
  if (!showInstallation) byId("agent-access").open = false;
  else if (byId("agent-access").open) loadAgents();
  byId("health").textContent = system.provider_enabled
    ? `${system.pending_syncs} tracking checks scheduled · ${system.sync_errors} sync errors`
    : "Shipments save locally. Set the EasyPost API key to register them and receive tracking updates.";
  connected = true;
  try {
    await applyRoute(true);
  } finally {
    scheduleRefresh();
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
    query.set("sort", view === "delivered" ? "delivered" : "expected");
    if (view === "active") query.set("active", "true");
    if (view === "delivered") query.set("status", "delivered");
    if (view === "out_for_delivery") query.set("status", "out_for_delivery");
    if (view === "exceptions") query.set("exceptions", "true");
    if (search) query.set("search", search);
  }

  try {
    const [summary, page, system, installation] = await Promise.all([
      api(activeDirection === "all" ? "v1/summary" : `v1/summary?direction=${activeDirection}`, { signal: controller.signal }),
      api(`v1/shipments?${query}`, { signal: controller.signal }),
      api("v1/system", { signal: controller.signal }),
      showInstallation ? api("v1/installation", { signal: controller.signal }) : null,
    ]);
    // A search or archive-filter change owns its whole result, including any
    // pagination requests already in flight for the previous list.
    if (generation !== listGeneration) return;
    providerEnabled = system.provider_enabled;
    if (installation) renderInstallation(installation);
    byId("health").textContent = system.provider_enabled
      ? `${system.pending_syncs} tracking checks scheduled · ${system.sync_errors} sync errors`
      : "Shipments save locally. Set the EasyPost API key to register them and receive tracking updates.";
    byId("health").append(
      ` Last checked ${new Date().toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}.`,
    );
    for (const button of byId("directions").querySelectorAll("button"))
      button.setAttribute("aria-pressed", String(button.dataset.direction === activeDirection));
    const stats = byId("summary");
    const focusedStat = document.activeElement?.dataset?.stat;
    stats.replaceChildren();
    for (const [key, label] of [
      ["active", "ACTIVE"],
      ["out_for_delivery", "OUT FOR DELIVERY"],
      ["delivered", "DELIVERED"],
      ["exceptions", "EXCEPTIONS"],
    ]) {
      const card = element("button", undefined, `stat stat-${key}`);
      card.type = "button";
      card.dataset.stat = key;
      card.setAttribute("aria-pressed", String(view === key));
      card.setAttribute("aria-label", `${summary[key]} ${label.toLowerCase()} shipments`);
      card.append(element("strong", summary[key]), element("span", label));
      card.addEventListener("click", guarded(() => {
        byId("view").value = key;
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
        shipment.sync.last_error ? "Tracking needs attention" : null,
      ]
        .filter(Boolean)
        .join(" · ");
      copy.append(element("h2", shipment.title));
      if (description) copy.append(element("p", description));
      copy.append(element("p", `${shipment.tracking.carrier || "Carrier not identified"} · ${shipment.tracking.number}`, "shipment-tracking"));
      const delivered = shipment.status === "delivered";
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
        guarded(() => { userNavigation(); return show(shipment.id); }),
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
        out_for_delivery: ["Nothing out for delivery.", "Shipments appear here when the carrier starts the delivery run."],
        exceptions: ["No delivery exceptions.", "Shipments with delivery or tracking problems appear here."],
        all: [
          "No shipments yet.",
          "Use Add shipment when you have a tracking number. New labels do not need a carrier scan.",
        ],
        archived: [
          "No archived shipments.",
          "Archived shipments are kept here and can be restored from their details.",
        ],
      };
      const [heading, description] = search
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
    byId("installation").open = attention > 0;
    installationInitialized = true;
  }
}

byId("check-installation").addEventListener("click", guarded(async () => {
  const button = byId("check-installation");
  button.disabled = true;
  button.textContent = "Checking…";
  try {
    renderInstallation(await api("v1/installation/check", { method: "POST" }));
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
      carrier: shipment.tracking?.carrier,
      number: shipment.tracking?.number,
      signature_required: shipment.signature_required == null ? "" : String(shipment.signature_required),
    })) {
      form.elements.namedItem(key).value = value || "";
    }
  }
  byId("editor").showModal();
}

function setEditorSaving(saving) {
  savingEditor = saving;
  byId("shipment-fields").disabled = saving;
  byId("save").disabled = saving;
  byId("save").textContent = saving ? "Saving…" : "Save shipment";
  byId("edit-status").textContent = saving ? "Saving your shipment locally." : "";
}

function currentDetail(id, generation) {
  return (
    generation === detailGeneration &&
    selected?.id === id &&
    byId("detail").open
  );
}

function carrierTrackingUrl(tracking) {
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

function easyPostTrackingUrl(value) {
  if (!value) return null;
  try {
    const url = new URL(value);
    if (url.protocol === "https:" && url.host === "track.easypost.com" &&
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

function calendarDate(value) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "date not reported";
  const today = new Date();
  const day = new Date(date.getFullYear(), date.getMonth(), date.getDate());
  const currentDay = new Date(today.getFullYear(), today.getMonth(), today.getDate());
  const difference = Math.round((day - currentDay) / 86_400_000);
  if (difference === 0) return "today";
  if (difference === 1) return "tomorrow";
  if (difference === -1) return "yesterday";
  return date.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric",
    ...(date.getFullYear() !== today.getFullYear() ? { year: "numeric" } : {}) });
}

function renderCarrierHistory(tracking) {
  const list = byId("carrier-events");
  list.replaceChildren();
  const events = [...(tracking?.events || [])].sort((left, right) =>
    (Date.parse(right.datetime) || 0) - (Date.parse(left.datetime) || 0));
  byId("carrier-history-note").textContent = tracking?.events_truncated
    ? "Showing the most recent 512 carrier events. Earlier events exceeded the history limit."
    : events.length ? "" : "The carrier has not reported any scan events yet.";
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

async function show(id, preserveView = false) {
  writeRoute({ shipment: id, edit: null });
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
  for (const [label, url] of [
    ["Carrier tracking", carrierTrackingUrl(shipment.tracking)],
    ["EasyPost tracking", easyPostTrackingUrl(shipment.sync.tracking_url)],
  ]) {
    if (!url) continue;
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
  const providerState = !providerEnabled ? "Not configured" : shipment.sync.last_error
    ? "Needs attention" : shipment.sync.last_success_at ? "Connected" : "Waiting for first check";
  provider.append(element("summary", `Tracking connection · ${providerState}`));
  const fields = element("dl", undefined, "shipment-details");
  const detail = (label, value) => fields.append(element("dt", label), element("dd", value));
  detail("Provider", "EasyPost");
  detail("Registration", shipment.sync.registration_state === "registered" ? "Registered" : shipment.sync.registration_state === "rejected" ? "Needs attention; correct tracking details" : "Pending registration; saved locally");
  if (shipment.sync.error_code) detail("Error code", shipment.sync.error_code);
  if (shipment.sync.provider_error_code) detail("Provider error code", shipment.sync.provider_error_code);
  if (shipment.sync.provider_status) detail("Provider response", `HTTP ${shipment.sync.provider_status}`);
  detail("Last successful sync", dateLabel(shipment.sync.last_success_at, "Not checked"));
  detail("Next check", shipment.sync.next_attempt_at == null ? "Not scheduled"
    : dateLabel(shipment.sync.next_attempt_at * 1000));
  if (shipment.sync.last_error) detail("Last error", shipment.sync.last_error);
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
  renderCarrierHistory(shipment.carrier_tracking);

  const actions = byId("actions");
  actions.replaceChildren();
  const button = (label, action) => {
    const node = element("button", label, "quiet");
    node.addEventListener("click", guarded(() => { userNavigation(); return action(); }));
    actions.append(node);
  };
  button("Edit", () => openEditor(shipment));
  if (shipment.tracking && !shipment.archived)
    button("Refresh tracking", () => mutate("refresh", "POST"));
  if (shipment.status_override)
    button("Clear correction", () => mutate("status-override", "DELETE"));
  button(shipment.archived ? "Restore" : "Archive", () =>
    mutate("archive", shipment.archived ? "DELETE" : "PUT"),
  );
  if (!preserveView) {
    for (const id of ["correction", "activity"]) byId(id).open = route().get("section") === id;
  }
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
byId("login").addEventListener(
  "submit",
  guarded(async () => {
    token = byId("login").elements.namedItem("token").value;
    await connect();
    byId("login").reset();
    notice("");
  }),
);

document.querySelectorAll("[data-close]").forEach((button) => {
  button.addEventListener("click", () => {
    userNavigation();
    if (button.dataset.close === "detail") closeDetail();
    else closeEditor();
  });
});
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
  const key = createKey;
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
    },
  };
  byId("edit-error").textContent = "";
  setEditorSaving(true);
  try {
    const headers = shipment
      ? { "If-Match": `"${shipment.revision}"` }
      : { "Idempotency-Key": key };
    const saved = await api(shipment ? `v1/shipments/${shipment.id}` : "v1/shipments", {
      method: shipment ? "PATCH" : "POST",
      headers,
      body: JSON.stringify(input),
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
  if (window.parent !== window)
    window.parent.postMessage({ type: "home-assistant/unsubscribe-properties" }, location.origin);
});

byId("add").hidden = true;
connect().catch((error) => {
  byId("login").hidden = false;
  if (
    !error.message.includes("authentication") &&
    !error.message.includes("credential")
  )
    notice(error.message, true);
});
