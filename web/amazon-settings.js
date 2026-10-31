import { safeRequestId } from "./api-error.js";

// Account credentials stay on the server. The one-time landing address only
// lives in the submit handler and is cleared before its request starts.
export function amazonSignInUrl(value) {
  if (typeof value !== "string" || value.length > 8192 || /[\\\u0000-\u0020\u007f]/.test(value)) return null;
  try {
    const url = new URL(value);
    if (url.protocol === "https:" && url.host === "www.amazon.com" &&
        url.pathname === "/ap/signin" && !url.username && !url.password && !url.hash)
      return url.href;
  } catch {
    // An invalid sign-in destination never becomes a clickable link.
  }
  return null;
}

const requestFailures = {
  upstream_rejected: "Amazon rejected this step. Start a new sign-in and complete any verification Amazon requests.",
  unexpected_response: "Amazon returned an unexpected response. Reopen Amazon settings to check the connection. If it keeps failing, check the app logs.",
  timeout: "Amazon did not respond in time. Check the app’s internet connection, then reopen Amazon settings before starting another sign-in.",
  connection: "The app could not reach Amazon. Check its internet connection, then reopen Amazon settings before starting another sign-in.",
  rate_limited: "Amazon is limiting requests. Wait before starting another sign-in; repeated attempts may extend the limit.",
  upstream_http: "Amazon returned an unsuccessful response. Reopen Amazon settings to check the connection. If it keeps failing, check the app logs.",
  configuration: "The app could not prepare this Amazon request. Check the app configuration and logs before starting another sign-in.",
};
const sessionChanged = "The Amazon connection changed during sign-in. Reopen Amazon settings to check its current state before starting another sign-in.";
const connectionSteps = {
  callback: {
    label: "Reading the Amazon sign-in address",
    failures: { invalid_input: "The final address was not recognized. Start a new sign-in and paste the complete final address from Amazon’s address bar." },
  },
  flow: {
    label: "Checking the sign-in attempt",
    failures: {
      invalid_input: "This sign-in attempt could not be matched to this tab. Start a new sign-in in this tab.",
      expired_flow: "This sign-in attempt expired or was already used. Start a new sign-in and paste the new final address promptly.",
      session_changed: sessionChanged,
    },
  },
  registration: {
    label: "Amazon device registration",
    failures: {
      ...requestFailures,
      account_identity_missing: "Amazon did not provide a stable account ID. Shipments could not confirm the connection. Check the app logs before starting another sign-in.",
    },
  },
  access_token: { label: "Checking the Amazon access token", failures: requestFailures },
  cookies: { label: "Opening the Amazon website session", failures: requestFailures },
  orders: { label: "Checking the Amazon orders page", failures: requestFailures },
  account: {
    label: "Verifying the Amazon account",
    failures: {
      account_mismatch: "The selected Amazon account differs from the connected account. Sign in to the connected account. To switch accounts, disconnect Amazon in settings first.",
      session_changed: sessionChanged,
    },
  },
  credentials: {
    label: "Saving the Amazon connection",
    failures: {
      persistence: "Shipments could not confirm that the connection was saved. Check the app’s data storage and free space, then reopen Amazon settings to check its state before starting another sign-in.",
      configuration: "Shipments could not access its private credential storage. Check the app’s data-directory permissions and logs, then reopen Amazon settings to check the connection.",
    },
  },
};

const websiteChallenge = "Amazon is showing a website challenge. The saved sign-in is retained. Open Amazon orders in your browser and complete the requested check, then retry Preview shipments.";
const isChallengeError = code => code === "amazon_preview_orders_challenge_page" || code === "provider_challenge_required";
const previewRequestFailures = {
  upstream_rejected: "Amazon no longer accepts the saved sign-in. Reconnect Amazon, then retry Preview shipments.",
  unexpected_response: "Amazon returned a response Shipments could not read. The saved sign-in is retained. Check the app logs before retrying Preview shipments.",
  timeout: "Amazon did not respond in time. The saved sign-in is retained. Check the app’s internet connection, then retry Preview shipments.",
  connection: "The app could not reach Amazon. The saved sign-in is retained. Check its internet connection, then retry Preview shipments.",
  rate_limited: "Amazon is limiting requests. The saved sign-in is retained. Wait before retrying Preview shipments.",
  upstream_http: "Amazon returned an unsuccessful response. The saved sign-in is retained. Retry Preview shipments later; check the app logs if it keeps failing.",
  configuration: "The app could not prepare this preview request. The saved sign-in is retained. Check the app configuration and logs before retrying Preview shipments.",
  in_progress: "An Amazon check is already running. Wait for it to finish before requesting another preview.",
};
const previewSteps = {
  access_token: { label: connectionSteps.access_token.label, failures: previewRequestFailures },
  cookies: { label: connectionSteps.cookies.label, failures: previewRequestFailures },
  orders: {
    label: connectionSteps.orders.label,
    failures: {
      ...previewRequestFailures,
      unsupported_orders_layout: "Your Amazon sign-in is saved. Shipments does not yet support this orders-page layout, so no unverified shipments were imported. Keep the saved sign-in; the parser needs a compatibility update. Check the app logs.",
      challenge_page: websiteChallenge,
    },
  },
  credentials: {
    label: "Saving the preview check",
    failures: {
      persistence: "Shipments could not save the preview check. Check the app’s data storage and free space, then retry Preview shipments. The saved sign-in has not been removed.",
      configuration: "Shipments could not access its private credential storage. Check the app’s data-directory permissions and logs before retrying Preview shipments.",
      session_changed: "The Amazon connection changed during the preview. Reopen Amazon settings to check its current state before retrying Preview shipments.",
    },
  },
};

function amazonError(error, fallback) {
  let explanation = error?.code === "provider_challenge_required" ? websiteChallenge : fallback;
  if (typeof error?.code === "string") {
    for (const [operation, steps, instruction] of [
      ["connect", connectionSteps, " Do not reuse the previous final Amazon address."],
      ["preview", previewSteps, ""],
    ]) {
      for (const [stage, step] of Object.entries(steps)) {
        const prefix = `amazon_${operation}_${stage}_`;
        if (!error.code.startsWith(prefix)) continue;
        const reason = error.code.slice(prefix.length);
        if (Object.hasOwn(step.failures, reason))
          explanation = `${step.label}: ${step.failures[reason]}${instruction}`;
        break;
      }
    }
  }
  const requestId = safeRequestId(error?.requestId);
  return requestId ? `${explanation} Request ID: ${requestId}.` : explanation;
}

function validStatus(value) {
  return value && ["disconnected", "connected", "auth_required"].includes(value.state) &&
    typeof value.imports_enabled === "boolean" &&
    (value.challenge_required === undefined || typeof value.challenge_required === "boolean");
}

export function installAmazonSettings(root, request, onUnauthorized) {
  const byId = id => root.querySelector(`#${id}`);
  let owner = false;
  let generation = 0;
  let controller = null;
  let flowId = null;
  let expiryTimer = null;
  let busy = false;
  let status = null;
  let previewed = false;

  const message = (text, error = false) => {
    byId("amazon-message").textContent = text;
    byId("amazon-error").replaceChildren();
    byId("amazon-error").textContent = error ? text : "";
    if (error) byId("amazon-message").textContent = "";
  };

  function clearFlow() {
    clearTimeout(expiryTimer);
    expiryTimer = null;
    flowId = null;
    byId("amazon-landing-url").value = "";
    byId("amazon-sign-in-link").removeAttribute("href");
    byId("amazon-sign-in").hidden = true;
  }

  function clearPreview() {
    previewed = false;
    byId("amazon-preview-list").replaceChildren();
    byId("amazon-preview-result").hidden = true;
  }

  function appendOrdersLink(target) {
    const orders = root.ownerDocument.createElement("a");
    orders.href = "https://www.amazon.com/your-orders/orders";
    orders.target = "_blank";
    orders.rel = "noopener noreferrer";
    orders.textContent = " Open Amazon orders ↗";
    target.append(orders);
  }

  function render() {
    const connected = status?.state === "connected";
    const challenged = connected && status.challenge_required === true;
    byId("amazon-state").textContent = status?.state === "auth_required"
      ? "Sign-in required"
      : challenged ? "Verification required"
      : connected ? status.imports_enabled ? "Connected" : previewed ? "Signed in · preview checked" : "Signed in · preview required"
      : "Not connected";
    byId("amazon-description").textContent = status?.state === "auth_required"
      ? "Amazon no longer accepts the saved sign-in. Reconnect to resume shipment updates."
      : challenged ? "Your saved sign-in is retained. Open Amazon orders in your browser, complete any requested check, then choose Preview shipments."
      : connected
        ? status.imports_enabled
          ? "New shipments are imported automatically using the same tracking schedule as your other shipments."
          : "Your Amazon sign-in is saved. Preview shipments to check page support before enabling automatic imports. The preview leaves saved shipments unchanged."
      : "Connect one Amazon.com account to find shipments automatically. Your password and verification stay on Amazon.";
    byId("amazon-check-note").replaceChildren();
    byId("amazon-check-note").textContent = challenged
      ? "Automatic checks are paused. Saved shipments remain available."
      : connected && status.last_error_code
        ? status.imports_enabled
          ? "The last Amazon check failed. Saved shipments remain available, and tracking will retry automatically."
          : "The last preview check failed. Your sign-in is saved. Retry Preview shipments after resolving the reported issue."
      : "";
    if (challenged && !byId("amazon-error").textContent) appendOrdersLink(byId("amazon-check-note"));
    byId("amazon-last-success").textContent = status?.last_success_at && Number.isFinite(Date.parse(status.last_success_at))
      ? `Last successful check: ${new Date(status.last_success_at).toLocaleString()}.` : "";
    byId("amazon-start").textContent = status?.state === "disconnected" || !status ? "Connect Amazon" : "Reconnect Amazon";
    byId("amazon-start").hidden = Boolean(flowId) || challenged;
    byId("amazon-preview").hidden = !connected;
    byId("amazon-enable").hidden = !connected || challenged || status.imports_enabled || !previewed;
    byId("amazon-refresh").hidden = !connected || challenged || !status.imports_enabled;
    byId("amazon-disconnect").hidden = !status || status.state === "disconnected";
    for (const control of root.querySelectorAll("button, input"))
      control.disabled = !owner || (busy && control.id !== "amazon-cancel");
  }

  function invalidate() {
    generation += 1;
    controller?.abort();
    controller = null;
    busy = false;
    clearFlow();
    clearPreview();
  }

  function updateStatus(value) {
    if (!owner || !validStatus(value)) return;
    if ((status?.state !== value.state && value.state !== "connected") ||
        (value.challenge_required === true && status?.challenge_required !== true)) {
      clearPreview();
      if (!busy) message("");
    }
    status = value;
    render();
  }

  async function run(progress, failure, action) {
    if (!owner || busy || !root.open) return;
    const current = generation;
    controller = new AbortController();
    busy = true;
    message(progress);
    render();
    try {
      await action(controller.signal, () => current === generation && owner && root.open);
    } catch (error) {
      if (current !== generation) return;
      if ([401, 403].includes(error.status)) {
        onUnauthorized();
        return;
      }
      if (isChallengeError(error.code) && status?.state === "connected")
        updateStatus({ ...status, challenge_required: true });
      message(amazonError(error, failure), true);
      if (isChallengeError(error.code)) appendOrdersLink(byId("amazon-error"));
    } finally {
      if (current === generation) {
        busy = false;
        controller = null;
        render();
      }
    }
  }

  byId("amazon-start").addEventListener("click", () => {
    if (status?.state === "connected" && status.challenge_required === true) return;
    return run(
      "Preparing Amazon sign-in…", "Could not prepare sign-in. Try Connect Amazon again.",
      async (signal, current) => {
        clearFlow();
        const flow = await request("v1/amazon/sign-in", { method: "POST", body: "{}", signal });
        if (!current()) return;
        const url = amazonSignInUrl(flow?.url);
        if (!url || typeof flow.flow_id !== "string" || !flow.flow_id || flow.flow_id.length > 200)
          throw new Error("Invalid sign-in response");
        flowId = flow.flow_id;
        byId("amazon-sign-in-link").href = url;
        byId("amazon-sign-in").hidden = false;
        message("Open the Amazon sign-in link, then return here with the final address.");
        byId("amazon-sign-in-link").focus();
        expiryTimer = setTimeout(() => {
          if (!flowId || current() === false) return;
          clearFlow();
          message("This sign-in link expired. Choose Connect Amazon to start again.", true);
          render();
        }, 10 * 60 * 1000);
        expiryTimer.unref?.();
      },
    );
  });

  byId("amazon-sign-in-link").addEventListener("click", event => {
    if (!owner || busy || !flowId) event.preventDefault();
  });

  byId("amazon-sign-in").addEventListener("submit", event => {
    event.preventDefault();
    if (!flowId || busy || !owner) return;
    const landingUrl = byId("amazon-landing-url").value.trim();
    if (!landingUrl) {
      message("Paste the final address from Amazon’s sign-in page.", true);
      return;
    }
    const submittedFlow = flowId;
    clearFlow();
    run("Checking Amazon sign-in…", "Could not complete sign-in. Choose Reconnect Amazon to start again.", async (signal, current) => {
      const result = await request("v1/amazon/complete", {
        method: "POST", body: JSON.stringify({ flow_id: submittedFlow, landing_url: landingUrl }), signal,
      });
      if (!current()) return;
      if (!validStatus(result) || result.state !== "connected") throw new Error("Invalid connection response");
      clearPreview();
      updateStatus(result);
      message(result.imports_enabled
        ? "Amazon reconnected. Shipment updates will resume."
        : "Amazon sign-in saved. Preview shipments to check page support before enabling imports.");
    });
  });

  byId("amazon-cancel").addEventListener("click", () => {
    invalidate();
    message("Sign-in cleared from this tab.");
    render();
    byId("amazon-start").focus();
  });

  byId("amazon-preview").addEventListener("click", () => run(
    "Reading a shipment preview…", "Could not read a preview. Your saved shipments have not been changed.",
    async (signal, current) => {
      clearPreview();
      const result = await request("v1/amazon/preview", { method: "POST", body: "{}", signal });
      if (!current()) return;
      if (!Array.isArray(result?.shipments) || result.shipments.length > 100 ||
          !Number.isSafeInteger(result.deferred) || result.deferred < 0 ||
          result.shipments.some(item => typeof item.title !== "string" || !item.title || item.title.length > 500 ||
            typeof item.tracking?.number !== "string" || !item.tracking.number || item.tracking.number.length > 256 ||
            typeof item.tracking?.carrier !== "string" || !item.tracking.carrier || item.tracking.carrier.length > 100 ||
            (item.order_reference != null && (typeof item.order_reference !== "string" || item.order_reference.length > 500)))) throw new Error("Invalid preview response");
      if (status?.state === "connected") updateStatus({ ...status, challenge_required: false });
      const list = byId("amazon-preview-list");
      for (const shipment of result.shipments) {
        const row = root.ownerDocument.createElement("li");
        const title = root.ownerDocument.createElement("strong");
        title.textContent = shipment.title;
        const tracking = root.ownerDocument.createElement("p");
        tracking.className = "muted";
        tracking.textContent = `${shipment.tracking.carrier} · ${shipment.tracking.number}${shipment.order_reference ? ` · ${shipment.order_reference}` : ""}`;
        row.append(title, tracking);
        list.append(row);
      }
      byId("amazon-preview-note").textContent = result.shipments.length
        ? `${result.shipments.length} verified shipment${result.shipments.length === 1 ? "" : "s"}. Nothing has been added or changed.`
        : "No verified shipments are ready to import. Nothing has been added or changed.";
      byId("amazon-deferred").textContent = result.deferred
        ? `${result.deferred} candidate${result.deferred === 1 ? " needs" : "s need"} a tracking number or exact parcel details before import.` : "";
      byId("amazon-preview-result").hidden = false;
      previewed = result.shipments.length > 0 || result.deferred === 0;
      message(previewed ? "Preview ready." : "No verified shipments are ready yet. Automatic imports remain off.");
    },
  ));

  byId("amazon-enable").addEventListener("click", () => {
    if (!previewed || status?.state !== "connected" || status.challenge_required === true || status.imports_enabled) return;
    return run(
      "Enabling automatic imports…", "Could not confirm automatic imports. Reopen Amazon settings to check before retrying.",
      async (signal, current) => {
        const result = await request("v1/amazon/enable", { method: "POST", body: "{}", signal });
        if (!current()) return;
        if (!validStatus(result) || result.state !== "connected" || !result.imports_enabled) throw new Error("Invalid connection response");
        updateStatus(result);
        message("Automatic imports enabled. Amazon shipments will appear in your existing list.");
      },
    );
  });

  byId("amazon-refresh").addEventListener("click", () => {
    if (status?.state === "connected" && status.challenge_required === true) return;
    return run(
      "Queuing a tracking check…", "Could not queue a check. Reopen Amazon settings to check the connection.",
      async (signal, current) => {
        await request("v1/amazon/refresh", { method: "POST", body: "{}", signal });
        if (current()) message("Tracking check queued. Existing rate limits and retry delays still apply.");
      },
    );
  });

  byId("amazon-disconnect").addEventListener("click", () => run(
    "Disconnecting Amazon…", "Could not confirm disconnection. Reopen Amazon settings to check before retrying.",
    async (signal, current) => {
      await request("v1/amazon", { method: "DELETE", signal });
      if (!current()) return;
      clearFlow();
      clearPreview();
      updateStatus({ state: "disconnected", imports_enabled: false });
      message("Amazon disconnected. Saved shipments and history are still available.");
    },
  ));

  root.addEventListener("toggle", () => {
    if (!root.open) {
      invalidate();
      message("");
      render();
      return;
    }
    run("Checking Amazon connection…", "Could not check the Amazon connection. Reopen settings to retry.", async (signal, current) => {
      const result = await request("v1/amazon", { signal });
      if (!current()) return;
      if (!validStatus(result)) throw new Error("Invalid connection response");
      updateStatus(result);
      message("");
    });
  });

  return {
    setAccess(value, reset = false) {
      if (owner === value && !reset) return;
      invalidate();
      owner = value;
      status = null;
      root.hidden = !owner;
      if (!owner) root.open = false;
      message("");
      render();
    },
    updateStatus,
    clear() {
      invalidate();
      render();
    },
    open() {
      if (!owner) return;
      root.open = true;
      root.scrollIntoView({ block: "start" });
      byId(status?.state === "connected" && status.challenge_required === true ? "amazon-preview" : "amazon-start").focus({ preventScroll: true });
    },
  };
}
