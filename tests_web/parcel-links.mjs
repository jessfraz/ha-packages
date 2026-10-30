import assert from "node:assert/strict";
import test from "node:test";
import { carrierTrackingUrl, providerTrackingUrl, trackingLinks } from "../web/parcel-links.js";

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

test("Amazon Logistics numbers use the recipient route instead of Swiship", () => {
  for (const number of ["TBA_SYNTHETIC123", "tba_synthetic123"]) {
    const links = trackingLinks({ tracking: { carrier: "AmazonMcf", number }, sync: { provider: "aftership" } });
    assert.deepEqual(links.map(([label, url]) => [label, url.href]), [
      ["Carrier tracking", `https://track.amazon.com/tracking/${number}`],
    ]);
  }
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
