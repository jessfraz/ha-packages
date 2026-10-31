import assert from "node:assert/strict";
import test from "node:test";
import { ApiError } from "../web/api-error.js";
import { amazonSignInUrl, installAmazonSettings } from "../web/amazon-settings.js";

class Node extends EventTarget {
  constructor(id = "", tagName = "div") {
    super();
    this.id = id;
    this.tagName = tagName;
    this.children = [];
    this.textContent = "";
    this.value = "";
    this.hidden = false;
    this.disabled = false;
    this.open = false;
    this.href = "";
  }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren(...nodes) { this.children = nodes; }
  removeAttribute(name) { this[name] = ""; }
  focus() { this.focused = true; }
  scrollIntoView() { this.scrolled = true; }
}

const disconnected = { state: "disconnected", imports_enabled: false };
const connected = { state: "connected", imports_enabled: false, last_success_at: "2026-10-30T19:00:00Z" };
const signIn = { flow_id: "synthetic-flow", url: "https://www.amazon.com/ap/signin?openid.oa2.code_challenge=SYNTHETIC" };
const landing = "https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=SYNTHETIC_SECRET";
const settle = () => new Promise(resolve => setImmediate(resolve));

function setup(initial = disconnected) {
  const nodes = new Map();
  for (const id of ["state", "description", "check-note", "last-success", "start", "preview", "enable", "refresh", "disconnect", "landing-url", "sign-in-link", "sign-in", "cancel", "complete", "message", "error", "preview-list", "preview-note", "preview-result", "deferred"])
    nodes.set(`amazon-${id}`, new Node(`amazon-${id}`, ["start", "preview", "enable", "refresh", "disconnect", "cancel", "complete"].includes(id) ? "button" : id === "landing-url" ? "input" : "div"));
  const root = new Node("amazon");
  root.ownerDocument = { createElement: tag => new Node("", tag) };
  root.querySelector = selector => {
    const node = nodes.get(selector.slice(1));
    assert.ok(node, `Known UI element ${selector}`);
    return node;
  };
  root.querySelectorAll = () => [...nodes.values()].filter(node => ["button", "input"].includes(node.tagName));
  const calls = [];
  const replies = [];
  const request = async (path, options = {}) => {
    calls.push({ path, ...options });
    if (!replies.length) throw new Error(`No response queued for ${path}`);
    const reply = replies.shift();
    if (typeof reply === "function") return reply(options);
    if (reply instanceof Error) throw reply;
    return reply;
  };
  const settings = installAmazonSettings(root, request, () => settings.setAccess(false));
  settings.setAccess(true);
  settings.updateStatus(initial);
  root.open = true;
  return {
    settings, root, calls, replies,
    node: id => nodes.get(`amazon-${id}`),
    async click(id) { nodes.get(`amazon-${id}`).dispatchEvent(new Event("click", { cancelable: true })); await settle(); },
    async submit() { nodes.get("amazon-sign-in").dispatchEvent(new Event("submit", { cancelable: true })); await settle(); },
    async close() { root.open = false; root.dispatchEvent(new Event("toggle")); await settle(); },
  };
}

function content(node) {
  return [node.textContent, ...node.children.map(content)].join(" ");
}

test("sign-in destinations stay on Amazon's exact HTTPS sign-in page", () => {
  assert.equal(amazonSignInUrl(signIn.url), signIn.url);
  for (const value of [null, {}, "https://www.amazon.com.evil.test/ap/signin", "http://www.amazon.com/ap/signin", "https://user:secret@www.amazon.com/ap/signin", "https://www.amazon.com:444/ap/signin", "https://www.amazon.com/ap/maplanding?code=secret", "https://www.amazon.com/ap/signin#secret", "https://www.amazon.com\\ap/signin", "https://www.ama\nzon.com/ap/signin", "https://www.amazon.com/ap/signin?x=" + "a".repeat(8192)])
    assert.equal(amazonSignInUrl(value), null);
});

test("opening reconnect settings does not begin an Amazon sign-in", () => {
  const ui = setup({ state: "auth_required", imports_enabled: true });
  ui.root.open = false;
  ui.settings.open();
  assert.equal(ui.root.open, true);
  assert.equal(ui.root.scrolled, true);
  assert.equal(ui.node("start").focused, true);
  assert.equal(ui.node("start").textContent, "Reconnect Amazon");
  assert.equal(ui.node("description").textContent.includes("Reconnect"), true);
  assert.deepEqual(ui.calls, []);
});

test("only an explicit action prepares a validated sign-in link", async () => {
  const ui = setup();
  ui.replies.push(signIn);
  await ui.click("start");
  assert.equal(ui.calls.length, 1);
  assert.equal(ui.calls[0].path, "v1/amazon/sign-in");
  assert.equal(ui.calls[0].method, "POST");
  assert.equal(ui.calls[0].body, "{}");
  assert.equal(ui.node("sign-in-link").href, signIn.url);
  assert.equal(ui.node("sign-in").hidden, false);
  await ui.close();
});

test("landing address is cleared before submission finishes and never becomes a link", async () => {
  const ui = setup();
  ui.replies.push(signIn);
  await ui.click("start");
  let resolve;
  ui.replies.push(() => new Promise(done => { resolve = done; }));
  ui.node("landing-url").value = landing;
  await ui.submit();
  assert.equal(ui.node("landing-url").value, "");
  assert.equal(ui.node("sign-in-link").href, "");
  assert.equal(ui.node("sign-in").hidden, true);
  assert.deepEqual(JSON.parse(ui.calls[1].body), { flow_id: signIn.flow_id, landing_url: landing });
  resolve(connected);
  await settle();
  assert.equal(ui.node("state").textContent, "Signed in · preview required");
  assert.equal(ui.node("message").textContent.includes("SYNTHETIC_SECRET"), false);
  assert.equal(ui.node("preview").hidden, false);
  assert.equal(ui.node("enable").hidden, true);
});

test("closing or losing owner access aborts the request and ignores late sign-in responses", async () => {
  for (const invalidate of [ui => ui.close(), ui => ui.settings.setAccess(false)]) {
    const ui = setup();
    let resolve;
    ui.replies.push(() => new Promise(done => { resolve = done; }));
    await ui.click("start");
    ui.node("landing-url").value = landing;
    await invalidate(ui);
    assert.equal(ui.node("landing-url").value, "");
    assert.equal(ui.calls[0].signal.aborted, true);
    resolve(signIn);
    await settle();
    assert.equal(ui.node("sign-in-link").href, "");
    assert.equal(ui.node("sign-in").hidden, true);
  }
});

test("expired sign-in flows clear the address and link", async t => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const ui = setup();
  ui.replies.push(signIn);
  await ui.click("start");
  ui.node("landing-url").value = landing;
  t.mock.timers.tick(10 * 60 * 1000);
  assert.equal(ui.node("landing-url").value, "");
  assert.equal(ui.node("sign-in-link").href, "");
  assert.equal(ui.node("sign-in").hidden, true);
  assert.match(ui.node("error").textContent, /expired/);
});

test("sign-in failures never display backend messages or reuse submitted secrets", async () => {
  const ui = setup();
  ui.replies.push(signIn);
  await ui.click("start");
  ui.replies.push(new Error(`upstream rejected ${landing}`));
  ui.node("landing-url").value = landing;
  await ui.submit();
  assert.equal(ui.node("landing-url").value, "");
  assert.equal(ui.node("sign-in").hidden, true);
  assert.match(ui.node("error").textContent, /Could not complete sign-in/);
  assert.equal(ui.node("error").textContent.includes("SYNTHETIC_SECRET"), false);
  await ui.submit();
  assert.equal(ui.calls.length, 2);
});

test("untrusted sign-in responses never expose an external link", async () => {
  const ui = setup();
  ui.replies.push({ ...signIn, url: "https://evil.test/" });
  await ui.click("start");
  assert.equal(ui.node("sign-in").hidden, true);
  assert.equal(ui.node("sign-in-link").href, "");
  assert.match(ui.node("error").textContent, /Could not prepare/);
});

test("a non-writing preview renders parcel text safely and leaves enabling explicit", async () => {
  const ui = setup(connected);
  ui.replies.push({ shipments: [{ title: "<img src=x onerror=alert(1)>", order_reference: "SYNTHETIC-ORDER", tracking: { carrier: "USPS", number: "SYNTHETIC123" } }], deferred: 2 });
  await ui.click("preview");
  assert.equal(ui.calls.length, 1);
  assert.equal(ui.calls[0].path, "v1/amazon/preview");
  assert.equal(ui.node("preview-list").children.length, 1);
  assert.equal(ui.node("preview-list").children[0].children[0].textContent, "<img src=x onerror=alert(1)>");
  assert.equal(ui.node("preview-list").children[0].children[0].tagName, "strong");
  assert.match(content(ui.node("preview-list")), /USPS · SYNTHETIC123/);
  assert.match(ui.node("preview-note").textContent, /Nothing has been added or changed/);
  assert.match(ui.node("deferred").textContent, /2 candidates/);
  assert.equal(ui.node("enable").hidden, false);
  await ui.close();
  assert.equal(ui.node("preview-list").children.length, 0);
});

test("malformed previews cannot enable imports or render partial data", async () => {
  const ui = setup(connected);
  ui.replies.push({ shipments: [{ title: "Incomplete", tracking: { carrier: "USPS" } }], deferred: 0 });
  await ui.click("preview");
  assert.equal(ui.node("preview-list").children.length, 0);
  assert.equal(ui.node("enable").hidden, true);
  assert.match(ui.node("error").textContent, /Could not read a preview/);
});

test("enabling, refresh and disconnect follow the explicit connection state", async () => {
  const ui = setup(connected);
  ui.replies.push({ shipments: [], deferred: 0 });
  await ui.click("preview");
  ui.replies.push({ ...connected, imports_enabled: true });
  await ui.click("enable");
  assert.equal(ui.calls[1].path, "v1/amazon/enable");
  assert.equal(ui.node("enable").hidden, true);
  assert.equal(ui.node("refresh").hidden, false);
  ui.replies.push(null);
  await ui.click("refresh");
  assert.equal(ui.calls[2].path, "v1/amazon/refresh");
  ui.replies.push(null);
  await ui.click("disconnect");
  assert.equal(ui.calls[3].method, "DELETE");
  assert.equal(ui.node("state").textContent, "Not connected");
  assert.equal(ui.node("preview").hidden, true);
  assert.match(ui.node("message").textContent, /Saved shipments and history/);
});

test("role loss disables owner actions and clears preview data", async () => {
  const ui = setup(connected);
  ui.replies.push({ shipments: [{ title: "Synthetic package", tracking: { carrier: "UPS", number: "SYNTHETIC" } }], deferred: 0 });
  await ui.click("preview");
  ui.settings.setAccess(false);
  assert.equal(ui.root.hidden, true);
  assert.equal(ui.node("preview-list").children.length, 0);
  await ui.click("start");
  assert.equal(ui.calls.length, 1);
});


test("expired authentication removes stale preview and action messages", async () => {
  const ui = setup({ ...connected, imports_enabled: true });
  ui.replies.push({ shipments: [{ title: "Synthetic package", tracking: { carrier: "USPS", number: "SYNTHETIC" } }], deferred: 0 });
  await ui.click("preview");
  assert.equal(ui.node("preview-list").children.length, 1);
  ui.settings.updateStatus({ state: "auth_required", imports_enabled: true });
  assert.equal(ui.node("preview-list").children.length, 0);
  assert.equal(ui.node("preview-result").hidden, true);
  assert.equal(ui.node("message").textContent, "");
  assert.equal(ui.node("state").textContent, "Sign-in required");
});


test("missing account identity explains the registration failure without exposing provider text", async () => {
  const ui = setup();
  ui.replies.push(signIn);
  await ui.click("start");
  const requestId = "fbf22920-3494-4f72-833f-dd34eb2e323b";
  ui.replies.push(new ApiError(`private upstream response ${landing}`, 400, {
    error_code: "amazon_connect_registration_account_identity_missing", request_id: requestId,
  }));
  ui.node("landing-url").value = landing;
  await ui.submit();
  assert.match(ui.node("error").textContent, /device registration/i);
  assert.match(ui.node("error").textContent, /did not provide a stable account ID/i);
  assert.match(ui.node("error").textContent, /could not confirm the connection/i);
  assert.match(ui.node("error").textContent, /Check the app logs before starting another sign-in/);
  assert.match(ui.node("error").textContent, /Do not reuse the previous final Amazon address/);
  assert.equal(ui.node("error").textContent.includes("intended Amazon.com account"), false);
  assert.ok(ui.node("error").textContent.includes(requestId));
  assert.equal(ui.node("error").textContent.includes("SYNTHETIC_SECRET"), false);
  assert.equal(ui.node("error").textContent.includes("private upstream"), false);
  assert.equal(ui.node("landing-url").value, "");
  assert.equal(ui.node("sign-in-link").href, "");
  assert.equal(ui.node("enable").hidden, true);
  await ui.submit();
  assert.equal(ui.calls.length, 2, "one-use callback is not retried automatically or resubmitted");
});

async function failSignIn(body, status = 400) {
  const ui = setup();
  ui.replies.push(signIn);
  await ui.click("start");
  ui.replies.push(new ApiError(`DO_NOT_DISPLAY ${landing}`, status, body));
  ui.node("landing-url").value = landing;
  await ui.submit();
  assert.equal(ui.node("landing-url").value, "");
  assert.equal(ui.node("sign-in-link").href, "");
  assert.equal(ui.node("enable").hidden, true);
  assert.equal(ui.calls.length, 2);
  assert.equal(ui.node("error").textContent.includes("DO_NOT_DISPLAY"), false);
  assert.equal(ui.node("error").textContent.includes("SYNTHETIC_SECRET"), false);
  return ui;
}

test("known failure stages explain the failing step and a manual recovery action", async () => {
  for (const [code, stage, recovery] of [
    ["amazon_connect_callback_invalid_input", /sign-in address/i, /complete final address/],
    ["amazon_connect_flow_expired_flow", /sign-in attempt/i, /expired or was already used/],
    ["amazon_connect_flow_invalid_input", /sign-in attempt/i, /new sign-in in this tab/],
    ["amazon_connect_registration_upstream_rejected", /device registration/i, /new sign-in/],
    ["amazon_connect_access_token_timeout", /access token/i, /internet connection/],
    ["amazon_connect_cookies_unexpected_response", /website session/i, /app logs/],
    ["amazon_connect_orders_connection", /orders page/i, /could not reach Amazon/],
    ["amazon_connect_orders_rate_limited", /orders page/i, /Wait before/],
    ["amazon_connect_access_token_upstream_http", /access token/i, /check the connection/],
    ["amazon_connect_account_account_mismatch", /Verifying the Amazon account/i, /disconnect Amazon in settings first/],
    ["amazon_connect_account_session_changed", /Verifying the Amazon account/i, /check its current state/],
    ["amazon_connect_credentials_persistence", /Saving the Amazon connection/i, /data storage and free space/],
    ["amazon_connect_credentials_configuration", /Saving the Amazon connection/i, /permissions and logs/],
  ]) {
    const ui = await failSignIn({ error_code: code, message: `DO_NOT_DISPLAY ${landing}` });
    assert.match(ui.node("error").textContent, stage, code);
    assert.match(ui.node("error").textContent, recovery, code);
    assert.match(ui.node("error").textContent, /Do not reuse the previous final Amazon address/, code);
    assert.equal(ui.node("error").textContent.includes("Request ID:"), false);
  }
});

test("unsupported pairs and arbitrary error payloads retain the safe action fallback", async () => {
  const requestId = "fbf22920-3494-4f72-833f-dd34eb2e323b";
  for (const error_code of [
    "amazon_connect_callback_timeout", "amazon_connect_credentials_upstream_rejected",
    "amazon_connect_account_account_identity_missing", "amazon_connect_unknown_expired_flow",
    "amazon_connect_cookies___proto__", "amazon_connect_flow_constructor", "amazon_connect_orders_new_error",
    "provider_authentication_required", `amazon_connect_registration_account_identity_missing ${landing}`, {},
  ]) {
    const ui = await failSignIn({ error_code, request_id: requestId, message: `DO_NOT_DISPLAY ${landing}`, details: { token: "SYNTHETIC_SECRET" } });
    assert.equal(ui.node("error").textContent, `Could not complete sign-in. Choose Reconnect Amazon to start again. Request ID: ${requestId}.`);
  }
});

test("recognized errors reject malicious request IDs while keeping useful fixed guidance", async () => {
  for (const request_id of [landing, "fbf22920-3494-4f72-833f-dd34eb2e323b\nSYNTHETIC_SECRET", { request_id: "SYNTHETIC_SECRET" }]) {
    const ui = await failSignIn({ error_code: "amazon_connect_cookies_unexpected_response", request_id, message: landing });
    assert.match(ui.node("error").textContent, /Opening the Amazon website session/);
    assert.equal(ui.node("error").textContent.includes("Request ID:"), false);
    assert.equal(ui.node("error").textContent.includes("https:"), false);
  }
});

test("unsupported Amazon orders pages retain the saved sign-in without requesting another login", async () => {
  const ui = setup(connected);
  const requestId = "fbf22920-3494-4f72-833f-dd34eb2e323b";
  ui.replies.push(new ApiError(`DO_NOT_DISPLAY ${landing}`, 502, {
    error_code: "amazon_preview_orders_unsupported_orders_layout", request_id: requestId,
  }));
  await ui.click("preview");
  assert.match(ui.node("error").textContent, /does not yet support this orders-page layout/);
  assert.match(ui.node("error").textContent, /saved sign-in/);
  assert.match(ui.node("error").textContent, /no unverified shipments were imported/);
  assert.match(ui.node("error").textContent, /compatibility update/);
  assert.ok(ui.node("error").textContent.includes(requestId));
  assert.doesNotMatch(ui.node("error").textContent, /new sign-in|reuse|Reconnect Amazon|SYNTHETIC_SECRET|DO_NOT_DISPLAY/i);
  assert.equal(ui.node("state").textContent, "Signed in · preview required");
  assert.equal(ui.node("enable").hidden, true);
  assert.equal(ui.node("preview").hidden, false);
  assert.equal(ui.node("sign-in").hidden, true);
  assert.equal(ui.calls.length, 1);
});

async function failPreview(body) {
  const ui = setup(connected);
  ui.replies.push(new ApiError(`DO_NOT_DISPLAY ${landing}`, 502, body));
  await ui.click("preview");
  assert.equal(ui.node("enable").hidden, true);
  assert.equal(ui.node("preview").hidden, false);
  assert.equal(ui.node("sign-in").hidden, true);
  assert.equal(ui.node("state").textContent, "Signed in · preview required");
  assert.equal(ui.node("error").textContent.includes("SYNTHETIC_SECRET"), false);
  assert.equal(ui.node("error").textContent.includes("DO_NOT_DISPLAY"), false);
  assert.equal(ui.calls.length, 1, "preview does not trigger a new sign-in or another request");
  return ui;
}

test("preview transport and layout failures keep the connection and recommend preview recovery", async () => {
  for (const [error_code, recovery] of [
    ["amazon_preview_access_token_timeout", /internet connection/],
    ["amazon_preview_cookies_unexpected_response", /app logs/],
    ["amazon_preview_orders_connection", /could not reach Amazon/],
    ["amazon_preview_orders_rate_limited", /Wait before retrying Preview/],
    ["amazon_preview_orders_upstream_http", /retry Preview shipments later/i],
    ["amazon_preview_orders_configuration", /app configuration and logs/],
    ["amazon_preview_credentials_persistence", /data storage and free space/],
    ["amazon_preview_credentials_configuration", /permissions and logs/],
    ["amazon_preview_credentials_session_changed", /check its current state/],
  ]) {
    const ui = await failPreview({ error_code, message: landing });
    assert.match(ui.node("error").textContent, recovery, error_code);
    assert.match(ui.node("error").textContent, /Preview|preview/, error_code);
    assert.doesNotMatch(ui.node("error").textContent, /new sign-in|reuse|Reconnect Amazon/i, error_code);
  }
});

test("a website challenge offers only the fixed Amazon orders link and a manual preview retry", async () => {
  const ui = await failPreview({
    error_code: "amazon_preview_orders_challenge_page", message: landing,
    url: "https://evil.test/?token=SYNTHETIC_SECRET",
  });
  assert.match(ui.node("error").textContent, /website challenge/);
  assert.match(ui.node("error").textContent, /complete the requested check, then retry Preview shipments/);
  assert.doesNotMatch(ui.node("error").textContent, /new sign-in|reuse|Reconnect Amazon/i);
  assert.equal(ui.node("error").children.length, 1);
  const link = ui.node("error").children[0];
  assert.equal(link.tagName, "a");
  assert.equal(link.href, "https://www.amazon.com/your-orders/orders");
  assert.equal(link.target, "_blank");
  assert.equal(link.rel, "noopener noreferrer");
  ui.replies.push({ shipments: [{ title: "Synthetic parcel", tracking: { carrier: "USPS", number: "SYNTHETIC" } }], deferred: 0 });
  await ui.click("preview");
  assert.equal(ui.node("error").children.length, 0, "the old challenge link is cleared on the next action");
  assert.equal(ui.node("state").textContent, "Signed in · preview checked");
  assert.equal(ui.node("enable").hidden, false);
  assert.deepEqual(ui.calls.map(call => call.path), ["v1/amazon/preview", "v1/amazon/preview"]);
});

test("only confirmed authentication rejection recommends reconnecting during a preview", async () => {
  for (const stage of ["access_token", "cookies", "orders"]) {
    const ui = await failPreview({ error_code: `amazon_preview_${stage}_upstream_rejected` });
    assert.match(ui.node("error").textContent, /no longer accepts the saved sign-in/);
    assert.match(ui.node("error").textContent, /Reconnect Amazon, then retry Preview shipments/);
    assert.equal(ui.node("start").textContent, "Reconnect Amazon");
    assert.equal(ui.calls.length, 1);
  }
});

test("a failed preview invalidates previous preview permission to enable imports", async () => {
  const ui = setup(connected);
  ui.replies.push({ shipments: [{ title: "Synthetic parcel", tracking: { carrier: "USPS", number: "SYNTHETIC" } }], deferred: 0 });
  await ui.click("preview");
  assert.equal(ui.node("enable").hidden, false);
  ui.replies.push(new ApiError("Private provider text", 502, { error_code: "amazon_preview_orders_unsupported_orders_layout" }));
  await ui.click("preview");
  assert.equal(ui.node("enable").hidden, true);
  assert.equal(ui.node("preview-list").children.length, 0);
  assert.equal(ui.node("state").textContent, "Signed in · preview required");
  await ui.click("enable");
  assert.deepEqual(ui.calls.map(call => call.path), ["v1/amazon/preview", "v1/amazon/preview"]);
});

test("unknown-only candidates cannot enable imports while a recognized empty account can", async () => {
  const ui = setup(connected);
  ui.replies.push({ shipments: [], deferred: 2 });
  await ui.click("preview");
  assert.equal(ui.node("enable").hidden, true);
  assert.match(ui.node("message").textContent, /Automatic imports remain off/);
  assert.equal(ui.node("state").textContent, "Signed in · preview required");
  await ui.click("enable");
  assert.equal(ui.calls.length, 1);
  ui.replies.push({ shipments: [], deferred: 0 });
  await ui.click("preview");
  assert.equal(ui.node("enable").hidden, false);
  assert.equal(ui.node("state").textContent, "Signed in · preview checked");
});

test("unsupported preview pairs and unsafe metadata never display provider text or login blame", async () => {
  for (const error_code of ["amazon_preview_registration_upstream_rejected", "amazon_preview_credentials_upstream_rejected", "amazon_preview_access_token_unsupported_orders_layout", "amazon_preview_orders_unknown_reason", `amazon_preview_orders_challenge_page ${landing}`]) {
    const ui = await failPreview({ error_code, request_id: landing, message: landing });
    assert.equal(ui.node("error").textContent, "Could not read a preview. Your saved shipments have not been changed.");
    assert.equal(ui.node("error").children.length, 0);
    assert.doesNotMatch(ui.node("error").textContent, /new sign-in|reuse|Reconnect Amazon/i);
  }
});
