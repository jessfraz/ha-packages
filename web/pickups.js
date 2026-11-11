// Physical operations have their own explicit review and durable receipts.
// No tracking-agent credential is sufficient to open this admin-only view.
import { calendarDate, civilDay } from "./dates.js";

const carrierNames = { usps: "USPS", ups: "UPS", fedex: "FedEx" };
const states = {
  pending: "Checking eligibility",
  dispatching: "Awaiting carrier confirmation",
  scheduled: "Scheduled",
  cancel_pending: "Cancellation queued",
  cancelling: "Awaiting cancellation",
  cancelled: "Cancelled",
  rejected: "Not scheduled",
  unknown: "Outcome unknown",
};
const terminal = new Set(["scheduled", "cancelled", "rejected", "unknown"]);
const trackingCarriers = { USPS: "usps", UspsShip: "usps", UPS: "ups", UPSDAP: "ups", FedEx: "fedex", FedExDefault: "fedex", FedexSmartPost: "fedex" };
function node(tag, text, className) {
  const element = document.createElement(tag);
  if (text !== undefined) element.textContent = text;
  if (className) element.className = className;
  return element;
}
function addressText(site) {
  return postalText(site.details.address);
}
function postalText(a) {
  return [a.street, a.secondary, `${a.city}, ${a.state} ${a.postal_code}`]
    .filter(Boolean)
    .join(", ");
}
function addressKey(a) {
  return JSON.stringify([a.street, a.secondary || null, a.city, a.state, a.postal_code, a.country]);
}
function windowText(plan) {
  return plan.carrier === "usps"
    ? "During normal mail delivery"
    : `${plan.ready_time}–${plan.close_time}`;
}
function feeText(fee, carrier) {
  if (fee?.kind === "free") return "Free normal-route pickup";
  if (fee?.kind === "estimate")
    return `Estimated fee: ${fee.currency} ${fee.amount}. The carrier may assess a different charge.`;
  return `Fee not quoted. ${carrierNames[carrier] || "The carrier"} may charge a pickup fee to your ${carrierNames[carrier] || "carrier"} shipping account. The amount is not known yet.`;
}
function serviceLabel(service) {
  const [carrier, ...words] = service.split("_");
  return [carrierNames[carrier] ?? carrier, ...words.map((word) => word.charAt(0).toUpperCase() + word.slice(1))].join(" ");
}
function reviewDate(date, timeZone) {
  const relative = calendarDate(date, new Date(), timeZone);
  if (relative !== "today" && relative !== "tomorrow") return relative;
  const absolute = new Date(`${date}T12:00:00Z`).toLocaleDateString([], {
    timeZone: "UTC", weekday: "short", month: "short", day: "numeric",
  });
  return `${relative === "today" ? "Today" : "Tomorrow"} · ${absolute}`;
}
export function installPickups(
  root,
  opener,
  packages,
  request,
  onForbidden,
  options = {},
) {
  const find = (id) => root.ownerDocument.getElementById(id);
  const field = (form, name) => form.elements.namedItem(name);
  // Dialogs live outside either page so an action never has to navigate first.
  const dialogs = [
    "pickup-site-dialog",
    "pickup-schedule-dialog",
    "pickup-cancel-dialog",
  ].map(find);
  const siteForm = find("pickup-site-form");
  const form = find("pickup-schedule-form");
  let access = false;
  let generation = 0;
  let sites = [];
  let addressSettingsKey = "";
  let carriers = [];
  let editingSite = null;
  let selectingDefault = false;
  let dateEdited = false;
  let addressReview = null;
  let confirmedAddress = null;
  let siteFormRevision = 0;
  let pendingTracking = null;
  let reviewed = null;
  let reviewedCarriers = [];
  let operationKey = null;
  let polling = null;
  let cancelRecord = null;
  let cancelKey = null;
  let busy = false;
  let protectedDialogs = new Set();
  let loading = false;
  let loadEpoch = 0;
  let queuedLoad = false;
  let capabilityLoad = null;
  let capabilitiesKnown = false;
  let capabilityEpoch = 0;
  const available = () => access && carriers.some((carrier) => carrier.configured === true);
  const configured = (carrier) => access && carriers.some((status) => status.carrier === carrier && status.configured === true);
  const current = (value) => access && value === generation;
  function showError(error, id, version) {
    const message = errorText(error);
    if (current(version)) find(id).textContent = message;
  }
  for (const dialog of dialogs) {
    dialog.addEventListener("cancel", (event) => {
      if (busy) event.preventDefault();
      else if (dialog === find("pickup-site-dialog")) pendingTracking = null;
    });
    dialog.addEventListener("close", () => {
      if (access && busy && protectedDialogs.has(dialog) && !dialog.open)
        dialog.showModal();
    });
  }
  const clearTimer = () => {
    if (polling !== null) clearTimeout(polling);
    polling = null;
  };
  function errorText(error) {
    if ([401, 403].includes(error.status)) onForbidden();
    return (
      error.message ||
      "The request could not be completed. Existing pickup records are unchanged."
    );
  }
  function setBusy(value) {
    // A queued close from the address dialog must not reopen it over the
    // next dialog when setup continues into scheduling.
    if (value && !busy)
      protectedDialogs = new Set(dialogs.filter((dialog) => dialog.open));
    if (!value) protectedDialogs.clear();
    busy = value;
    opener.disabled = value;
    for (const dialog of dialogs)
      for (const button of dialog.querySelectorAll("button"))
        button.disabled = value;
    for (const dialog of dialogs)
      if (dialog.open) dialog.setAttribute("aria-busy", String(value));
    const configured = carriers.some((carrier) => carrier.configured);
    const ready = configured && sites.some((site) => site.is_default);
    find("pickup-plan-fields").disabled = value || reviewed !== null || !ready;
    find("pickup-review-button").disabled = value || !ready;
    field(form, "accept_fee").disabled = value;
  }
  function showPackages() {
    root.hidden = true;
    packages.hidden = false;
    clearTimer();
  }
  function fillSites() {
    const select = field(form, "site_id");
    const selected = select.value;
    select.replaceChildren(
      ...sites.map((site) => {
        const { name, address } = site.details;
        const description = name === address.street ? name : `${name} · ${address.street}`;
        const option = node("option", `${description}${site.is_default ? " (default)" : ""}`);
        option.value = site.id;
        option.selected = sites.some((s) => s.id === selected)
          ? site.id === selected
          : site.is_default;
        return option;
      }),
    );
    const site = sites.find((s) => s.is_default);
    find("pickup-site-summary").textContent = site
      ? `Default: ${addressText(site)}`
      : "Save a pickup address once. It will be selected for future pickups.";
    find("pickup-setup-site").hidden = Boolean(site);
    find("pickup-settings").hidden = !sites.length;
    const key = sites.map((site) => `${site.id}:${site.revision}`).join(";");
    if (key !== addressSettingsKey) {
      addressSettingsKey = key;
      find("pickup-addresses").replaceChildren(...sites.map(siteRow));
    }
  }
  function defaultDate() {
    if (dateEdited) return;
    const site = sites.find((site) => site.id === field(form, "site_id").value);
    if (site) field(form, "date").value = civilDay(new Date(), site.details.timezone);
  }
  field(form, "site_id").addEventListener("change", defaultDate);
  field(form, "date").addEventListener("input", () => { dateEdited = true; });
  function siteRow(site) {
    const row = node("div", undefined, "pickup-address-row");
    const details = node("div");
    details.append(node("strong", site.details.name));
    if (site.is_default) details.append(node("span", "Default", "pickup-carrier-badge"));
    details.append(node("p", addressText(site), "muted"));
    const actions = node("div", undefined, "actions");
    const edit = node("button", "Edit", "quiet");
    edit.type = "button";
    edit.setAttribute("aria-label", `Edit pickup address: ${site.details.name}`);
    edit.addEventListener("click", () => editSite(site));
    actions.append(edit);
    if (!site.is_default) {
      const makeDefault = node("button", "Make default", "quiet");
      makeDefault.type = "button";
      makeDefault.setAttribute("aria-label", `Make ${site.details.name} the default pickup address`);
      makeDefault.addEventListener("click", () => changeDefault(site));
      actions.append(makeDefault);
    }
    row.append(details, actions);
    return row;
  }
  function fillCarriers() {
    const carrierSelect = field(form, "carrier");
    const selectedCarrier = carrierSelect.value;
    const enabled = carriers.filter((carrier) => carrier.configured === true);
    find("pickup-carrier-badges").replaceChildren(...enabled.map((carrier) =>
      node("li", carrierNames[carrier.carrier], "pickup-carrier-badge"),
    ));
    carrierSelect.replaceChildren(node("option", "Detect from tracking"));
    carrierSelect.firstElementChild.value = "";
    for (const carrier of enabled) {
      const option = node("option", carrierNames[carrier.carrier]);
      option.value = carrier.carrier;
      carrierSelect.append(option);
    }
    carrierSelect.value = enabled.some((carrier) => carrier.carrier === selectedCarrier) ? selectedCarrier : "";
    const select = field(form, "service");
    const selectedService = select.value;
    select.replaceChildren(node("option", "Use saved label service"));
    select.firstElementChild.value = "";
    for (const carrier of enabled.filter((carrier) => !carrierSelect.value || carrier.carrier === carrierSelect.value))
      for (const service of carrier.capabilities.services) {
        const option = node("option", serviceLabel(service));
        option.value = service;
        select.append(option);
      }
    select.value = [...select.options].some((option) => option.value === selectedService) ? selectedService : "";
    find("pickup-configuration").textContent = !enabled.length
      ? "Connect a carrier account before scheduling. Add developer credentials in the app’s private configuration."
      : `Charges use your configured ${enabled.map((carrier) => carrierNames[carrier.carrier]).join(", ")} shipping ${enabled.length === 1 ? "account" : "accounts"}.`;
    setBusy(busy);
  }
  field(form, "carrier").addEventListener("change", fillCarriers);
  function updateCapabilities(status) {
    const next = status.filter((carrier) => Object.hasOwn(carrierNames, carrier.carrier) && carrier.configured === true);
    const changed = JSON.stringify(carriers) !== JSON.stringify(next);
    carriers = next;
    if (capabilitiesKnown && changed) {
      capabilityEpoch++;
      pendingTracking = null;
      cancelRecord = null;
      cancelKey = null;
      setBusy(false);
      for (const dialog of dialogs) dialog.close();
      editPlan();
    }
    capabilitiesKnown = true;
    opener.hidden = !available();
    fillCarriers();
    options.onCapabilitiesChanged?.(available());
  }
  async function refreshCapabilities() {
    if (!access) return;
    if (capabilityLoad) return capabilityLoad;
    const version = generation;
    const operation = request("v1/pickups/carriers").then((status) => {
      if (current(version)) updateCapabilities(status);
    }).catch((error) => {
      if (!current(version)) return;
      updateCapabilities([]);
      if ([401, 403].includes(error.status)) onForbidden();
    });
    capabilityLoad = operation;
    try { await operation; }
    finally { if (capabilityLoad === operation) capabilityLoad = null; }
  }
  async function loadSetup(version) {
    const [locations] = await Promise.all([
      request("v1/pickups/locations"),
      refreshCapabilities(),
    ]);
    if (!current(version)) return;
    sites = locations;
    fillSites();
    fillCarriers();
  }
  function render(records) {
    const list = find("pickup-records");
    const focused = list.contains(document.activeElement)
      ? document.activeElement
      : null;
    const focusId = focused?.dataset.pickupId;
    const focusAction = focused?.dataset.pickupAction;
    list.replaceChildren();
    if (!records.length) {
      list.append(node("p", "No pickup requests yet.", "muted"));
      return;
    }
    for (const record of records) {
      const card = node("article", undefined, "pickup-card");
      const heading = node("div", undefined, "pickup-heading");
      heading.append(
        node("h3", carrierNames[record.plan.carrier]),
        node(
          "span",
          states[record.state] || "Unknown",
          `pickup-state ${record.state}`,
        ),
      );
      card.append(heading);
      card.append(
        node(
          "p",
          record.plan.tracking.length
            ? record.plan.tracking.join(" · ")
            : `${record.plan.parcels.reduce((n, p) => n + p.count, 0)} prepaid parcels`,
        ),
      );
      card.append(
        node(
          "p",
          `${record.confirmation?.pickup_date || record.offer?.date || record.plan.date || "Next available date"} · ${windowText(record.plan)} · ${record.plan.site.details.timezone}`,
          "muted",
        ),
      );
      card.append(node("p", addressText(record.plan.site), "muted"));
      if (record.confirmation)
        card.append(node("p", `Confirmation: ${record.confirmation.number}`));
      if (record.offer)
        card.append(
          node("p", feeText(record.offer.fee, record.plan.carrier), "muted"),
        );
      if (record.state === "scheduled")
        card.append(
          node(
            "p",
            "Pickup booked. Physical collection has not been confirmed.",
            "muted",
          ),
        );
      if (record.state === "unknown")
        card.append(
          node(
            "p",
            "Do not request another pickup for these labels. Confirm the outcome with the carrier first, using this request’s details.",
          ),
        );
      if (record.state === "scheduled" && record.plan.carrier === "fedex")
        card.append(
          node(
            "p",
            "FedEx does not provide pickup status lookups. Contact FedEx with the confirmation number to check your pickup.",
            "muted",
          ),
        );
      if (record.error)
        card.append(node("p", record.error.code.replaceAll("_", " "), "muted"));
      const actions = node("div", undefined, "header-actions");
      if (record.state === "scheduled" && configured(record.plan.carrier)) {
        if (record.plan.carrier !== "fedex") {
          const refresh = node("button", "Check with carrier", "quiet");
          refresh.type = "button";
          refresh.dataset.pickupId = record.id;
          refresh.dataset.pickupAction = "refresh";
          refresh.addEventListener("click", async () => {
            const version = generation;
            setBusy(true);
            try {
              await request(`v1/pickups/records/${record.id}/refresh`, {
                method: "POST",
                headers: { "If-Match": `"${record.revision}"` },
                body: "{}",
              });
              if (current(version)) {
                loadEpoch++;
                await load();
              }
            } catch (error) {
              showError(error, "pickup-error", version);
            } finally {
              if (current(version)) setBusy(false);
            }
          });
          actions.append(refresh);
        }
        const cancel = node("button", "Cancel pickup", "quiet");
        cancel.type = "button";
        cancel.dataset.pickupId = record.id;
        cancel.dataset.pickupAction = "cancel";
        cancel.addEventListener("click", () => {
          cancelRecord = record;
          cancelKey = crypto.randomUUID();
          find("pickup-cancel-details").textContent =
            `${carrierNames[record.plan.carrier]} · ${record.confirmation.pickup_date} · ${record.confirmation.number} · ${windowText(record.plan)} · ${record.plan.site.details.timezone} · ${addressText(record.plan.site)}`;
          find("pickup-cancel-error").textContent = "";
          find("pickup-cancel-dialog").showModal();
        });
        actions.append(cancel);
      }
      card.append(actions);
      list.append(card);
    }
    if (focusId && focusAction)
      for (const button of list.querySelectorAll("button"))
        if (
          button.dataset.pickupId === focusId &&
          button.dataset.pickupAction === focusAction
        )
          button.focus({ preventScroll: true });
  }
  async function load() {
    if (!access) return;
    if (loading) {
      queuedLoad = true;
      return;
    }
    const version = generation;
    const epoch = loadEpoch;
    loading = true;
    queuedLoad = false;
    try {
      const [locations, , page] = await Promise.all([
        request("v1/pickups/locations"),
        refreshCapabilities(),
        request("v1/pickups?limit=100"),
      ]);
      if (!current(version) || epoch !== loadEpoch) return;
      sites = locations;
      fillSites();
      render(page.items);
      fillCarriers();
      if (!root.hidden) {
        clearTimer();
        polling = setTimeout(
          load,
          page.items.some((r) => !terminal.has(r.state)) ? 1500 : 30000,
        );
      }
    } catch (error) {
      showError(error, "pickup-error", version);
    } finally {
      if (current(version)) {
        loading = false;
        if (queuedLoad) {
          queuedLoad = false;
          void load();
        }
      }
    }
  }
  opener.addEventListener("click", () => open([]));
  find("pickup-reload").addEventListener("click", load);
  function editSite(site = null, makeDefault = false) {
    if (!access || busy) return;
    editingSite = site;
    selectingDefault = makeDefault;
    confirmedAddress = null;
    clearAddressReview();
    const first = !sites.length;
    siteForm.reset();
    find("pickup-site-title").textContent = first || makeDefault ? "Set up pickup address" : site ? "Edit pickup address" : "Add pickup address";
    find("pickup-site-intro").textContent = first
      ? "Save your usual pickup address once. You can change it or add another in Pickup settings."
      : "Carriers collect here when you choose this address for a pickup.";
    find("pickup-site-label").hidden = first;
    find("pickup-site-default").hidden = first || makeDefault || Boolean(site?.is_default);
    find("pickup-site-save").textContent = first || makeDefault ? "Save default address" : "Save address";
    const value = editingSite?.details;
    if (value)
      for (const name of [
        "name",
        "contact_name",
        "phone",
        "email",
        "timezone",
        "ready_time",
        "close_time",
        "pickup_point",
        "instructions",
      ]) {
        field(siteForm, name).value = value[name] || "";
      }
    if (value) {
      for (const name of [
        "street",
        "secondary",
        "city",
        "state",
        "postal_code",
      ])
        field(siteForm, name).value = value.address[name] || "";
      field(siteForm, "residential").checked = value.residential;
    } else {
      field(siteForm, "timezone").value =
        options.getTimeZone?.() ||
        Intl.DateTimeFormat().resolvedOptions().timeZone;
      field(siteForm, "ready_time").value = "09:00";
      field(siteForm, "close_time").value = "18:00";
    }
    find("pickup-site-error").textContent = "";
    find("pickup-site-dialog").showModal();
  }
  find("pickup-setup-site").addEventListener("click", () => {
    pendingTracking = null;
    editSite(sites[0] ?? null, true);
  });
  find("pickup-add-site").addEventListener("click", () => editSite());
  find("pickup-site-close").addEventListener("click", () => {
    pendingTracking = null;
    find("pickup-site-dialog").close();
  });
  function clearAddressReview() {
    addressReview = null;
    find("pickup-address-review").hidden = true;
    find("pickup-address-review-message").textContent = "";
  }
  function showAddressReview(entered, result) {
    const suggested = result.status === "verified" ? result.normalized : result.suggestions?.[0];
    addressReview = { entered, suggested };
    find("pickup-address-review-message").textContent = suggested
      ? `Suggested: ${postalText(suggested)}`
      : result.status === "no_match"
        ? "The address verifier couldn’t find a match. Check it or keep the address you entered."
        : "Address verification wasn’t available. Check it or keep the address you entered.";
    find("pickup-use-suggested-address").hidden = !suggested;
    find("pickup-address-review").hidden = false;
    find("pickup-address-review-title").focus();
  }
  function confirmAddress(useSuggestion) {
    if (!addressReview || busy) return;
    const selected = useSuggestion ? addressReview.suggested : addressReview.entered;
    if (!selected) return;
    for (const name of ["street", "secondary", "city", "state", "postal_code"])
      field(siteForm, name).value = selected[name] ?? "";
    confirmedAddress = addressKey(selected);
    clearAddressReview();
    siteForm.requestSubmit();
  }
  find("pickup-use-suggested-address").addEventListener("click", () => confirmAddress(true));
  find("pickup-keep-entered-address").addEventListener("click", () => confirmAddress(false));
  siteForm.addEventListener("input", (event) => {
    siteFormRevision++;
    if (["street", "secondary", "city", "state", "postal_code"].includes(event.target.name)) {
      confirmedAddress = null;
      clearAddressReview();
    }
  });
  siteForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (busy || !access) return;
    const version = generation;
    const revision = siteFormRevision;
    setBusy(true);
    const value = (name) => field(siteForm, name).value.trim();
    const location = {
      name: value("name") || (!sites.length ? "Home" : value("street")),
      contact_name: value("contact_name"),
      company: null,
      phone: value("phone"),
      email: value("email") || null,
      address: {
        street: value("street"),
        secondary: value("secondary") || null,
        city: value("city"),
        state: value("state"),
        postal_code: value("postal_code"),
        country: "US",
      },
      timezone: value("timezone"),
      ready_time: value("ready_time"),
      close_time: value("close_time"),
      pickup_point: value("pickup_point"),
      instructions: value("instructions") || null,
      residential: field(siteForm, "residential").checked,
    };
    try {
      const address = addressKey(location.address);
      if (address !== confirmedAddress &&
          (!editingSite || address !== addressKey(editingSite.details.address))) {
        if (addressReview) {
          find("pickup-address-review-title").focus();
          return;
        }
        const result = await request("v1/pickups/verify-address", {
          method: "POST",
          body: JSON.stringify(location.address),
        });
        if (!current(version)) return;
        if (revision !== siteFormRevision) {
          find("pickup-site-error").textContent = "The form changed during verification. Review it and save again.";
          return;
        }
        if (result && (result.status !== "verified" || result.normalized &&
            addressKey(result.normalized) !== address)) {
          showAddressReview(location.address, result);
          return;
        }
        confirmedAddress = address;
      }
      await request(
        `v1/pickups/locations${editingSite ? `/${editingSite.id}` : ""}`,
        {
          method: editingSite ? "PUT" : "POST",
          headers: editingSite
            ? { "If-Match": `"${editingSite.revision}"` }
            : {},
          body: JSON.stringify({
            location,
            make_default: selectingDefault || !sites.length || Boolean(editingSite?.is_default) || field(siteForm, "make_default").checked,
          }),
        },
      );
      if (current(version)) {
        setBusy(false);
        find("pickup-site-dialog").close();
        if (pendingTracking !== null) {
          const tracking = pendingTracking;
          pendingTracking = null;
          await open(tracking);
        } else if (root.hidden) await loadSetup(version);
        else await load();
      }
    } catch (error) {
      showError(error, "pickup-site-error", version);
    } finally {
      if (current(version)) {
        setBusy(false);
        fillSites();
      }
    }
  });
  async function changeDefault(site) {
    if (!access || busy || site.is_default) return;
    const version = generation;
    setBusy(true);
    find("pickup-error").textContent = "";
    try {
      await request(`v1/pickups/locations/${site.id}`, {
        method: "PUT",
        headers: { "If-Match": `"${site.revision}"` },
        body: JSON.stringify({ location: site.details, make_default: true }),
      });
      if (!current(version)) return;
      await load();
      if (current(version)) {
        find("pickup-message").textContent = `${site.details.name} is the default pickup address.`;
        find("pickup-settings").querySelector("summary").focus();
      }
    } catch (error) {
      showError(error, "pickup-error", version);
    } finally {
      if (current(version)) setBusy(false);
    }
  }
  function editPlan() {
    reviewed = null;
    reviewedCarriers = [];
    operationKey = null;
    field(form, "accept_fee").required = false;
    find("pickup-fee-label").hidden = true;
    find("pickup-schedule-commit").hidden = true;
    find("pickup-review-button").hidden = false;
    find("pickup-plan-fields").hidden = false;
    find("pickup-review").hidden = true;
    find("pickup-review-details").replaceChildren();
    find("pickup-review-edit").hidden = true;
    find("pickup-review-button").textContent = "Review pickup";
    find("pickup-configuration").hidden = false;
    field(form, "accept_fee").checked = false;
    setBusy(busy);
  }
  find("pickup-schedule-close").addEventListener("click", () =>
    find("pickup-schedule-dialog").close(),
  );
  find("pickup-review-edit").addEventListener("click", () => {
    editPlan();
    field(form, "tracking").focus();
  });
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (
      busy || !access || reviewed ||
      !carriers.some((carrier) => carrier.configured)
    ) return;
    const version = generation;
    const epoch = capabilityEpoch;
    setBusy(true);
    find("pickup-schedule-error").textContent = "";
    try {
      await refreshCapabilities();
      if (!current(version) || epoch !== capabilityEpoch || !available()) return;
      if (!reviewed) {
        const input = {
          tracking: field(form, "tracking").value.split(/\s+/).filter(Boolean),
          accept_fee: false,
        };
        for (const name of [
          "carrier",
          "site_id",
          "date",
          "service",
          "weight_lbs",
          "ready_time",
          "close_time",
        ]) {
          const value = field(form, name).value;
          if (value) input[name] = value;
        }
        const previews = await request("v1/pickups/preview", {
          method: "POST",
          body: JSON.stringify(input),
        });
        if (!current(version) || epoch !== capabilityEpoch) return;
        if (!previews.length || previews.some((preview) => !configured(preview.plan.carrier)))
          throw new Error("The carrier configuration changed. Review the pickup again.");
        const site = previews[0]?.plan.site;
        const configuration = previews[0]?.configuration_id;
        if (
          !site ||
          previews.some(
            (preview) =>
              preview.plan.site.id !== site.id ||
              preview.plan.site.revision !== site.revision,
          )
        ) {
          throw new Error("Pickup address changed. Review the details again.");
        }
        if (!configuration || previews.some((preview) => preview.configuration_id !== configuration))
          throw new Error("The carrier configuration changed. Review the pickup again.");
        const details = find("pickup-review-details");
        details.replaceChildren();
        for (const preview of previews) {
          const p = preview.plan;
          const site = p.site.details;
          const count = p.parcels.reduce((n, piece) => n + piece.count, 0);
          const facts = node("dl", undefined, "pickup-review-facts");
          const point = [...field(siteForm, "pickup_point").options]
            .find((option) => option.value === site.pickup_point)?.textContent ?? site.pickup_point;
          for (const [label, text] of [
            ["Date", reviewDate(preview.offer.date, site.timezone)],
            ["Address", addressText(p.site)],
            ["Contact", [site.contact_name, site.phone, site.email].filter(Boolean).join(" · ")],
            ["Window", p.carrier === "usps" ? "During normal mail delivery"
              : `${preview.offer.ready_time}–${preview.offer.close_time} (${site.timezone})`],
            ["Collection point", [point, site.instructions].filter(Boolean).join(" · ")],
            ["Labels", p.tracking.join(", ")],
            ["Parcels", `${p.parcels.map((piece) => `${piece.count} × ${serviceLabel(piece.service)}`).join(", ")} · ${(p.total_weight_hundredths / 100).toFixed(2)} lb total`],
            ["Fee", feeText(preview.offer.fee, p.carrier)],
          ]) facts.append(node("dt", label), node("dd", text));
          details.append(node("h4", `${carrierNames[p.carrier]} · ${count} ${count === 1 ? "parcel" : "parcels"}`), facts);
        }
        const paid = previews.some(
          (p) => p.offer.fee?.kind !== "free" || p.plan.carrier !== "usps",
        );
        operationKey = crypto.randomUUID();
        reviewed = {
          ...input,
          site_id: site.id,
          reviewed_site: { id: site.id, revision: site.revision },
          reviewed_configuration: configuration,
        };
        reviewedCarriers = previews.map((preview) => preview.plan.carrier);
        find("pickup-fee-label").hidden = !paid;
        field(form, "accept_fee").required = paid;
        find("pickup-plan-fields").hidden = true;
        find("pickup-configuration").hidden = true;
        find("pickup-review").hidden = false;
        find("pickup-review-edit").hidden = false;
        find("pickup-review-button").hidden = true;
        find("pickup-schedule-commit").hidden = false;
        find("pickup-schedule-commit").textContent =
          previews.length > 1 ? "Schedule pickups" : "Schedule pickup";
        find("pickup-review-title").focus();
        find("pickup-review").scrollIntoView({ block: "nearest" });
      }
    } catch (error) {
      showError(error, "pickup-schedule-error", version);
    } finally {
      if (current(version)) setBusy(false);
    }
  });
  find("pickup-schedule-commit").addEventListener("click", async () => {
    if (busy || !access || !reviewed) return;
    if (
      field(form, "accept_fee").required &&
      !field(form, "accept_fee").checked
    ) {
      field(form, "accept_fee").reportValidity();
      return;
    }
    const version = generation;
    const epoch = capabilityEpoch;
    setBusy(true);
    find("pickup-schedule-error").textContent = "";
    try {
      await refreshCapabilities();
      if (!current(version) || epoch !== capabilityEpoch || !reviewed || !reviewedCarriers.every(configured)) return;
      const receipt = await request("v1/pickups", {
        method: "POST",
        headers: { "Idempotency-Key": operationKey },
        body: JSON.stringify({
          ...reviewed,
          accept_fee: field(form, "accept_fee").checked,
        }),
      });
      if (!current(version) || epoch !== capabilityEpoch) return;
      loadEpoch++;
      setBusy(false);
      find("pickup-schedule-dialog").close();
      editPlan();
      options.onScheduled?.();
      render(receipt.items);
      await load();
      if (current(version)) {
        find("pickup-message").textContent =
          "Pickup request saved. Each carrier’s confirmed or uncertain outcome is shown below.";
        find("pickup-reload").focus();
      }
    } catch (error) {
      if (current(version) && ["pickup_location_changed", "pickup_configuration_changed"].includes(error.code)) {
        editPlan();
        field(form, "tracking").focus();
        await load();
      }
      showError(error, "pickup-schedule-error", version);
    } finally {
      if (current(version)) setBusy(false);
    }
  });
  find("pickup-cancel-back").addEventListener("click", () =>
    find("pickup-cancel-dialog").close(),
  );
  find("pickup-cancel-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    if (busy || !access || !cancelRecord) return;
    const version = generation;
    const epoch = capabilityEpoch;
    setBusy(true);
    try {
      await refreshCapabilities();
      if (!current(version) || epoch !== capabilityEpoch || !cancelRecord || !configured(cancelRecord.plan.carrier)) return;
      await request(`v1/pickups/records/${cancelRecord.id}/cancel`, {
        method: "POST",
        headers: {
          "If-Match": `"${cancelRecord.revision}"`,
          "Idempotency-Key": cancelKey,
        },
        body: "{}",
      });
      if (current(version) && epoch === capabilityEpoch) {
        loadEpoch++;
        setBusy(false);
        find("pickup-cancel-dialog").close();
        cancelRecord = null;
        await load();
        find("pickup-reload").focus();
      }
    } catch (error) {
      showError(error, "pickup-cancel-error", version);
    } finally {
      if (current(version)) setBusy(false);
    }
  });
  async function open(tracking) {
    if (!available() || busy) return;
    const version = generation;
    const epoch = capabilityEpoch;
    form.reset();
    field(form, "site_id").value = "";
    dateEdited = false;
    editPlan();
    field(form, "tracking").value = tracking.join("\n");
    find("pickup-schedule-error").textContent = "";
    setBusy(true);
    try {
      await loadSetup(version);
      if (!current(version) || epoch !== capabilityEpoch || !available()) return;
      setBusy(false);
      if (!sites.some((site) => site.is_default)) {
        pendingTracking = tracking;
        editSite(sites[0] ?? null, true);
      } else {
        defaultDate();
        find("pickup-schedule-dialog").showModal();
        field(form, "tracking").focus();
      }
    } catch (error) {
      if (current(version)) options.onError?.(errorText(error));
    } finally {
      if (current(version)) setBusy(false);
    }
  }
  return {
    available,
    canSchedule(carrier) { return configured(trackingCarriers[carrier]); },
    refreshCapabilities,
    open,
    show() {
      if (!available()) return;
      packages.hidden = true;
      root.hidden = false;
      find("pickup-error").textContent = "";
      return load();
    },
    hide: showPackages,
    setAccess(allowed, reset = false) {
      if (access === allowed && !reset) return;
      generation++;
      access = allowed;
      clearTimer();
      loading = false;
      queuedLoad = false;
      loadEpoch++;
      busy = false;
      sites = [];
      carriers = [];
      capabilityLoad = null;
      capabilitiesKnown = false;
      capabilityEpoch++;
      editingSite = null;
      pendingTracking = null;
      addressSettingsKey = "";
      confirmedAddress = null;
      clearAddressReview();
      find("pickup-settings").open = false;
      cancelRecord = null;
      cancelKey = null;
      reviewed = null;
      reviewedCarriers = [];
      operationKey = null;
      opener.hidden = true;
      options.onCapabilitiesChanged?.(false);
      root.hidden = true;
      for (const dialog of dialogs) {
        dialog.close();
        for (const form of dialog.querySelectorAll("form")) form.reset();
      }
      for (const id of [
        "pickup-site-summary",
        "pickup-addresses",
        "pickup-carrier-badges",
        "pickup-message",
        "pickup-configuration",
        "pickup-error",
        "pickup-records",
        "pickup-review-details",
        "pickup-cancel-details",
        "pickup-schedule-error",
        "pickup-site-error",
        "pickup-cancel-error",
      ])
        find(id).replaceChildren();
      field(form, "site_id").replaceChildren();
      field(form, "service").replaceChildren();
      if (!packages.hidden || !allowed) packages.hidden = false;
      setBusy(false);
    },
  };
}
