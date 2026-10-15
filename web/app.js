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
let listController = null;
let refreshTimer = null;

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
  const system = await api("v1/system");
  byId("login").hidden = true;
  byId("dashboard").hidden = false;
  byId("add").hidden = false;
  byId("health").textContent = system.provider_enabled
    ? `${system.pending_syncs} tracking checks scheduled · ${system.sync_errors} sync errors`
    : "Carrier tracking is off. Local records and manual controls are available.";
  try {
    await load();
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
    query.set("archived", String(view === "archived"));
    if (view === "active") query.set("active", "true");
    if (view === "delivered") query.set("status", "delivered");
    if (search) query.set("search", search);
  }

  try {
    const [summary, page, system] = await Promise.all([
      api("v1/summary", { signal: controller.signal }),
      api(`v1/shipments?${query}`, { signal: controller.signal }),
      api("v1/system", { signal: controller.signal }),
    ]);
    // A search or archive-filter change owns its whole result, including any
    // pagination requests already in flight for the previous list.
    if (generation !== listGeneration) return;
    byId("health").textContent = system.provider_enabled
      ? `${system.pending_syncs} tracking checks scheduled · ${system.sync_errors} sync errors`
      : "Carrier tracking is off. Local records and manual controls are available.";
    byId("health").append(
      ` Last checked ${new Date().toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}.`,
    );
    const stats = byId("summary");
    stats.replaceChildren();
    for (const [key, label] of [
      ["active", "ACTIVE"],
      ["out_for_delivery", "OUT FOR DELIVERY"],
      ["delivered", "DELIVERED"],
      ["exceptions", "EXCEPTIONS"],
    ]) {
      const card = element("div", undefined, "stat");
      card.append(element("strong", summary[key]), element("span", label));
      stats.append(card);
    }

    const list = byId("list");
    if (!append) list.replaceChildren();
    for (const shipment of page.items) {
      const row = element("button", undefined, "shipment");
      const copy = element("div");
      const description = [
        shipment.merchant,
        shipment.order_reference,
        shipment.sync.last_error ? "Tracking needs attention" : null,
      ]
        .filter(Boolean)
        .join(" · ");
      copy.append(
        element("h2", shipment.title),
        element("p", description || "Manual shipment"),
      );
      const warning = ["failure", "error", "return_to_sender"].includes(
        shipment.status,
      );
      const badge = element(
        "span",
        labels[shipment.status],
        `badge${warning ? " warning" : ""}`,
      );
      row.append(copy, badge);
      row.addEventListener(
        "click",
        guarded(() => show(shipment.id)),
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
        all: [
          "No shipments yet.",
          "Use Add shipment to track an expected delivery, with or without a tracking number.",
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

async function refreshDashboard() {
  if (listController || document.hidden || byId("dashboard").hidden) return;
  await load();
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

function closeDetail() {
  detailGeneration += 1;
  selected = null;
  eventsPage = null;
  loadingEvents = false;
  if (byId("detail").open) byId("detail").close();
}

function closeEditor() {
  editorGeneration += 1;
  editing = null;
  createKey = null;
  savingEditor = false;
  if (byId("editor").open) byId("editor").close();
}

function openEditor(shipment = null) {
  closeDetail();
  editorGeneration += 1;
  editing = shipment;
  createKey = shipment ? null : crypto.randomUUID();
  savingEditor = false;
  byId("save").disabled = false;
  const form = byId("edit-form");
  form.reset();
  byId("edit-error").textContent = "";
  byId("edit-title").textContent = shipment ? "Edit shipment" : "Add shipment";
  if (shipment) {
    for (const [key, value] of Object.entries({
      title: shipment.title,
      merchant: shipment.merchant,
      order_reference: shipment.order_reference,
      carrier: shipment.tracking?.carrier,
      number: shipment.tracking?.number,
    })) {
      form.elements.namedItem(key).value = value || "";
    }
  }
  byId("editor").showModal();
}

function currentDetail(id, generation) {
  return (
    generation === detailGeneration &&
    selected?.id === id &&
    byId("detail").open
  );
}

async function show(id) {
  const generation = ++detailGeneration;
  selected = null;
  let shipment;
  try {
    shipment = await api(`v1/shipments/${id}`);
  } catch (error) {
    if (generation === detailGeneration) throw error;
    return null;
  }
  if (generation !== detailGeneration) return null;
  selected = shipment;
  byId("detail-title").textContent = shipment.title;
  const content = byId("detail-content");
  content.replaceChildren();
  const detail = (text) => content.append(element("p", text, "detail-item"));
  detail(
    `Status: ${labels[shipment.status]}${shipment.status_override ? " · manual correction" : ""}`,
  );
  if (shipment.status_override) {
    detail(
      `${shipment.status_override.reason} (${shipment.status_override.actor_id})`,
    );
  }
  if (shipment.carrier_status)
    detail(`Carrier reports: ${labels[shipment.carrier_status]}`);
  if (shipment.tracking) {
    detail(
      `${shipment.tracking.carrier || "Auto-detect carrier"} · ${shipment.tracking.number}`,
    );
  }
  if (shipment.merchant) detail(`Merchant: ${shipment.merchant}`);
  if (shipment.order_reference) detail(`Order: ${shipment.order_reference}`);
  if (shipment.sync.last_success_at) {
    detail(
      `Last carrier update: ${new Date(shipment.sync.last_success_at).toLocaleString()}`,
    );
  }
  if (shipment.sync.last_error) detail(`Tracking: ${shipment.sync.last_error}`);
  for (const source of shipment.sources)
    detail(`Source: ${source.namespace} / ${source.key}`);

  const actions = byId("actions");
  actions.replaceChildren();
  const button = (label, action) => {
    const node = element("button", label, "quiet");
    node.addEventListener("click", guarded(action));
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
  byId("override-form").reset();
  byId("detail-error").textContent = "";
  byId("events").replaceChildren();
  eventsPage = null;
  loadingEvents = false;
  byId("more-events").hidden = true;
  byId("more-events").disabled = false;
  if (!byId("detail").open) byId("detail").showModal();
  await loadEvents();
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

byId("add").addEventListener("click", () => openEditor());
byId("search").addEventListener(
  "submit",
  guarded(() => {
    appliedSearch = byId("query").value.trim();
    return load();
  }),
);
byId("view").addEventListener(
  "change",
  guarded(() => load()),
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
    if (button.dataset.close === "detail") closeDetail();
    else closeEditor();
  });
});
// Invalidate on Escape before its queued close event. Close events belonging
// to a dismissed dialog must not cancel a newer detail request still loading.
byId("detail").addEventListener("cancel", (event) => {
  event.preventDefault();
  closeDetail();
});
byId("editor").addEventListener("cancel", (event) => {
  event.preventDefault();
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
  savingEditor = true;
  const form = event.target;
  const save = byId("save");
  save.disabled = true;
  const values = Object.fromEntries(new FormData(form));
  const input = {
    title: values.title,
    merchant: values.merchant || null,
    order_reference: values.order_reference || null,
    tracking: values.number
      ? { carrier: values.carrier || null, number: values.number }
      : null,
  };
  try {
    const headers = shipment
      ? { "If-Match": `"${shipment.revision}"` }
      : { "Idempotency-Key": key };
    await api(shipment ? `v1/shipments/${shipment.id}` : "v1/shipments", {
      method: shipment ? "PATCH" : "POST",
      headers,
      body: JSON.stringify(input),
    });
    const current = generation === editorGeneration && byId("editor").open;
    if (current) closeEditor();
    await load();
    if (current) notice("Shipment saved.");
  } catch (error) {
    if (generation === editorGeneration && byId("editor").open)
      byId("edit-error").textContent = editError(error);
  } finally {
    if (generation === editorGeneration) {
      savingEditor = false;
      save.disabled = false;
    }
  }
});

byId("override-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const values = Object.fromEntries(new FormData(event.target));
  await mutate("status-override", "PUT", values);
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
