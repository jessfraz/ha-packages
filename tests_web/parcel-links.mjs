import assert from "node:assert/strict";
import test from "node:test";
import { carrierLabel, carrierTrackingUrl, providerTrackingUrl, trackingLinks } from "../web/parcel-links.js";

const number = "SYNTHETIC123";
const shipment = (carrier, provider = "aftership", tracking_url = null) => ({
  tracking: { carrier, number }, sync: { provider, tracking_url },
});

test("Amazon MCF has official carrier tracking even without an AfterShip public URL", () => {
  const links = trackingLinks(shipment("AmazonMcf"));
  assert.deepEqual(links.map(([label, url]) => [label, url.href]), [
    ["Carrier tracking", "https://www.swiship.com/track?id=SYNTHETIC123"],
  ]);
});

test("Amazon Account carrier uses its recipient fallback", () => {
  for (const number of ["TBA_SYNTHETIC123", "tba_synthetic123"]) {
    const links = trackingLinks({ tracking: { carrier: "AmazonAccount", number }, sync: { provider: "amazon_account" } });
    assert.deepEqual(links.map(([label, url]) => [label, url.href]), [
      ["Carrier tracking", `https://track.amazon.com/tracking/${number}`],
    ]);
  }
  assert.equal(carrierTrackingUrl({ carrier: "AmazonAccount", number }), null);
});

test("TBA-prefixed Amazon MCF keeps its public delivery link and AfterShip ownership", () => {
  const providerUrl = "https://www.aftership.com/track/synthetic";
  for (const number of ["TBA_SYNTHETIC123", "tba_synthetic123"]) {
    const value = shipment("AmazonMcf", "aftership", providerUrl);
    value.tracking.number = number;
    assert.deepEqual(trackingLinks(value).map(([label, url]) => [label, url.href]), [
      ["Carrier tracking", `https://track.amazon.com/tracking/${number}`],
      ["AfterShip tracking", providerUrl],
    ]);
    assert.equal(value.tracking.carrier, "AmazonMcf");
    assert.equal(value.sync.provider, "aftership");
    assert.equal(carrierLabel(value.tracking.carrier), "Amazon MCF");
  }
});

test("carrier labels distinguish Amazon Account from genuine MCF in all presentation contexts", async () => {
  const { carrierLabel, carrierOptionLabel, providerNames } = await import("../web/parcel-links.js");
  assert.equal(carrierLabel("AmazonAccount"), "Amazon Account");
  assert.equal(carrierOptionLabel("AmazonAccount"), "Amazon Account");
  assert.equal(providerNames.amazon_account, "Amazon Account");
  assert.equal(carrierLabel("AmazonMcf"), "Amazon MCF");
  assert.equal(carrierOptionLabel("AmazonMcf"), "Amazon MCF (AfterShip)");
  assert.equal(carrierLabel(null), "Carrier not identified");
  assert.equal(carrierLabel(null, "Auto-detect carrier"), "Auto-detect carrier");
  assert.equal(carrierLabel("FutureCarrier"), "FutureCarrier");
  assert.equal(carrierOptionLabel("FedExDefault"), "FedEx Default (EasyPost account)");
});

test("supported carrier families and their account aliases retain direct tracking", () => {
  for (const [carriers, host, parameter] of [
    [["USPS", "UspsShip"], "tools.usps.com", "tLabels"],
    [["UPS", "UPSDAP", "UPSMailInnovations"], "www.ups.com", "tracknum"],
    [["FedEx", "FedExDefault", "FedexSmartPost"], "www.fedex.com", "trknbr"],
    [["DHL", "DHLExpress", "DHLExpressDefault"], "www.dhl.com", "tracking-id"],
    [["AmazonMcf"], "www.swiship.com", "id"],
    [["OnTrac", "OntracV3", "LaserShipV2"], "www.ontrac.com", "number"],
  ]) for (const carrier of carriers) {
    const url = carrierTrackingUrl({ carrier, number });
    assert.equal(url.hostname, host, carrier);
    assert.equal(url.searchParams.get(parameter), number, carrier);
    if (carrier.startsWith("DHL")) assert.equal(url.searchParams.get("submit"), "1", carrier);
    assert.equal(carrierTrackingUrl({ carrier: carrier.toUpperCase(), number }).href, url.href);
  }
});

test("verified AfterShip and Amazon Shipping routes encode shipment identifiers", () => {
  for (const [carrier, expected] of [
    ["SpeedX", "https://tracking.speedx.io/SYNTHETIC123"],
    ["Sway", "https://www.returnmates.com/tracking/SYNTHETIC123"],
    ["Veho", "https://track.shipveho.com/#/trackingid/SYNTHETIC123"],
    ["AmazonShipping", "https://track.amazon.com/tracking/SYNTHETIC123"],
  ]) assert.equal(carrierTrackingUrl({ carrier, number }).href, expected);
  for (const carrier of ["SpeedX", "Sway", "Veho", "AmazonShipping", "AmazonMcf", "UPS"]) {
    const url = carrierTrackingUrl({ carrier, number: 'A/B?x=1&redirect=https://evil.example/#"' });
    assert.ok(!url.href.includes("redirect=https://evil.example/"));
    assert.equal(url.protocol, "https:");
    assert.equal(url.username, "");
  }
});

test("each aggregator keeps its real public URL alongside the carrier link", () => {
  for (const [carrier, provider, publicUrl, label] of [
    ["FedExDefault", "easypost", "https://track.easypost.com/synthetic", "EasyPost tracking"],
    ["AmazonMcf", "aftership", "https://www.aftership.com/track/synthetic", "AfterShip tracking"],
    ["Veho", "aftership", "https://track.aftership.com/synthetic", "AfterShip tracking"],
    ["Sway", "aftership", "https://www.aftership.com/track/synthetic", "AfterShip tracking"],
  ]) {
    const links = trackingLinks(shipment(carrier, provider, publicUrl));
    assert.equal(links.length, 2);
    assert.equal(links[0][0], "Carrier tracking");
    assert.equal(links[1][0], label);
    assert.equal(links[1][1].href, publicUrl);
  }
});

test("DoorDash uses the original private recipient link exactly once", () => {
  const link = "https://www.doordash.com/orders/drive?urlCode=SYNTHETIC_123";
  const value = shipment("DoorDash", "doordash", link);
  value.tracking.recipient_url = link;
  assert.deepEqual(trackingLinks(value).map(([label, url]) => [label, url.href]), [["DoorDash recipient tracking", link]]);
  assert.equal(trackingLinks(shipment("DoorDash", "doordash")).length, 0);
});

test("unknown carriers do not get fabricated URLs, but keep validated provider links", () => {
  for (const carrier of [null, 123, {}, "Unknown", "AmazonMws"]) {
    assert.equal(carrierTrackingUrl({ carrier, number }), null);
    assert.equal(trackingLinks(shipment(carrier, "easypost", "https://track.easypost.com/synthetic")).length, 1);
  }
  for (const tracking of [null, {}, { carrier: "UPS" }, { carrier: "UPS", number: "" }, { carrier: "UPS", number: "\n" }, { carrier: "UPS", number: "x".repeat(257) }])
    assert.equal(carrierTrackingUrl(tracking), null);
});

test("external links reject untrusted hosts, credentials, ports, schemes and controls", () => {
  for (const value of [
    "javascript:alert(1)", "http://track.easypost.com/test", "//track.easypost.com/test",
    "https://track.easypost.com.evil.example/test", "https://evil.example/track.easypost.com",
    "https://user:secret@track.easypost.com/test", "https://track.easypost.com:444/test",
    "https://track.easy\npost.com/test", "https://track.easypost.com/" + "x".repeat(2048), {},
  ]) assert.equal(providerTrackingUrl("easypost", value), null);
  assert.equal(providerTrackingUrl("aftership", "https://track.easypost.com/test"), null);
  assert.equal(providerTrackingUrl("unknown", "https://track.easypost.com/test"), null);
  for (const suffix of ["&redirect=evil", "&urlCode=other", "#fragment", "!"])
    assert.equal(providerTrackingUrl("doordash", `https://www.doordash.com/orders/drive?urlCode=SYNTHETIC${suffix}`), null);
});

test("path identifiers cannot traverse or crash the detail view", () => {
  for (const carrier of ["SpeedX", "Sway", "AmazonShipping"])
    for (const number of [".", "..", "\ud800"])
      assert.equal(carrierTrackingUrl({ carrier, number }), null);
});

test("Amazon Account links prefer the actual account page while retaining the real carrier", () => {
  const url = "https://www.amazon.com/gp/your-account/ship-track?orderId=SYNTHETIC&shipmentId=PACKAGE&packageId=1";
  const links = trackingLinks(shipment("USPS", "amazon_account", url));
  assert.deepEqual(links.map(([label]) => label), ["Amazon Account tracking", "Carrier tracking"]);
  assert.equal(links[0][1].href, url);
  assert.equal(links[1][1].hostname, "tools.usps.com");
  for (const value of [
    "https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=SECRET",
    "https://www.amazon.com/gp/your-account/ship-track?orderId=ONE&orderId=TWO",
    "https://www.amazon.com/gp/your-account/ship-track?orderId=ONE&token=SECRET",
    "https://www.amazon.com/gp/your-account/ship-track?shipmentId=PACKAGE",
    "https://www.amazon.com/gp/your-account/ship-track?orderId=ONE#secret",
    "https://user:secret@www.amazon.com/gp/your-account/ship-track?orderId=ONE",
    "https://www.amazon.com.evil.test/gp/your-account/ship-track?orderId=ONE",
  ]) assert.equal(providerTrackingUrl("amazon_account", value), null);
});

test("Amazon Account links retain indexed package and item identity without invented private URLs", () => {
  const query = "orderId=SYNTHETIC&shipmentId=PACKAGE&packageIndex=0&lineItemId=LINE&itemId=ITEM&trackingId=TBASYNTHETIC123";
  for (const path of [
    "/gp/your-account/ship-track", "/progress-tracker/package",
    "/-/en/gp/your-account/ship-track", "/-/en/progress-tracker/package",
  ]) {
    const url = `https://www.amazon.com${path}?${query}`;
    assert.equal(providerTrackingUrl("amazon_account", url)?.href, url);
    const value = shipment("AmazonAccount", "amazon_account", url);
    value.tracking.number = "TBASYNTHETIC123";
    assert.deepEqual(trackingLinks(value).map(([label, link]) => [label, link.href]), [
      ["Amazon Account tracking", url],
    ]);
    for (const suffix of [
      "&packageIndex=1", "&lineItemId=OTHER", "&token=PRIVATE", "&orderId=OTHER",
      "&itemId=ITEM%2FPATH", "#fragment",
    ]) assert.equal(providerTrackingUrl("amazon_account", url + suffix), null);
  }
  for (const url of [
    "http://www.amazon.com/progress-tracker/package?orderId=ONE",
    "https://www.amazon.com:444/progress-tracker/package?orderId=ONE",
    "https://www.amazon.com.evil.test/progress-tracker/package?orderId=ONE",
    "https://user:private@www.amazon.com/progress-tracker/package?orderId=ONE",
    "https://www.amazon.com/progress-tracker/package/delivery-information?orderId=ONE",
    "https://www.amazon.com/progress-tracker/package?orderId=ONE&lineItemId=LINE%2FOTHER",
    "https://www.amazon.com/progress-tracker/package?orderId=ONE&lineItemId=" + "x".repeat(129),
  ]) assert.equal(providerTrackingUrl("amazon_account", url), null);
  assert.equal(trackingLinks(shipment("AmazonAccount", "amazon_account")).length, 0);
});
