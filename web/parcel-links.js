export const providerNames = { easypost: "EasyPost", aftership: "AfterShip", doordash: "DoorDash", amazon_account: "Amazon Account" };

const carrierLabels = {
  AmazonAccount: "Amazon Account",
  AmazonMcf: "Amazon MCF",
  AmazonMws: "Amazon MWS",
  AmazonShipping: "Amazon Shipping",
  FedExAccount: "FedEx",
  FedExDefault: "FedEx Default",
  DHLExpress: "DHL Express",
  DHLExpressDefault: "DHL Express Default",
  Sway: "Sway / Returnmates",
};
const carrierOptionNotes = {
  FedEx: "EasyPost account",
  FedExAccount: "your carrier account",
  FedExDefault: "EasyPost account",
  DHLExpress: "your carrier account",
  DHLExpressDefault: "EasyPost account",
  AmazonMcf: "AfterShip",
  SpeedX: "AfterShip",
  Sway: "AfterShip",
  Veho: "AfterShip",
  DoorDash: "original tracking link",
};

export function carrierLabel(carrier, fallback = "Carrier not identified") {
  if (typeof carrier !== "string" || !carrier.trim()) return fallback;
  return Object.hasOwn(carrierLabels, carrier) ? carrierLabels[carrier] : carrier;
}

export function carrierOptionLabel(carrier) {
  const label = carrierLabel(carrier);
  return Object.hasOwn(carrierOptionNotes, carrier) ? `${label} (${carrierOptionNotes[carrier]})` : label;
}

// Only documented public tracking routes belong here. Carrier aliases share
// one route; the API provider (EasyPost/AfterShip) does not identify the carrier.
const carrierRoutes = [
  [["usps", "uspsship"], "https://tools.usps.com/go/TrackConfirmAction", "tLabels"],
  [["ups", "upsdap", "upsmailinnovations"], "https://www.ups.com/track", "tracknum"],
  [["fedex", "fedexdefault", "fedexsmartpost"], "https://www.fedex.com/fedextrack/", "trknbr"],
  [["dhl", "dhlexpress", "dhlexpressdefault"], "https://www.dhl.com/us-en/home/tracking.html?submit=1", "tracking-id"],
  [["amazonmcf"], "https://www.swiship.com/track", "id"],
  [["amazonshipping", "amazonaccount"], "https://track.amazon.com/tracking/", null],
  [["ontrac", "ontracv3", "lasershipv2"], "https://www.ontrac.com/tracking", "number"],
  [["sway"], "https://www.returnmates.com/tracking/", null],
  [["speedx"], "https://tracking.speedx.io/", null],
  [["veho"], "https://track.shipveho.com/", "#"],
];
const carrierTemplates = new Map(carrierRoutes.flatMap(([aliases, base, parameter]) =>
  aliases.map(alias => [alias, { base, parameter }])));

export function carrierTrackingUrl(tracking) {
  const carrier = typeof tracking?.carrier === "string" ? tracking.carrier.toLowerCase() : "";
  if (carrier === "doordash") return recipientTrackingUrl(tracking.recipient_url);
  const number = tracking?.number;
  if (typeof number !== "string" || !number.trim() || [".", ".."].includes(number) || number.length > 256 || /[\u0000-\u001f\u007f]/.test(number)) return null;
  // Account parcels may fall back to Amazon's public recipient tracker. MCF
  // remains a separate carrier and must not be inferred from a number prefix.
  if (carrier === "amazonaccount" && !/^TBA/i.test(number)) return null;
  const template = carrierTemplates.get(carrier);
  if (!template) return null;
  const url = new URL(template.base);
  try {
    if (template.parameter === "#") url.hash = `/trackingid/${encodeURIComponent(number)}`;
    else if (template.parameter === null) url.pathname += encodeURIComponent(number);
    else url.searchParams.set(template.parameter, number);
  } catch {
    // A malformed Unicode identifier must not prevent opening the detail UI.
    return null;
  }
  return url;
}

export function recipientTrackingUrl(value) {
  if (typeof value !== "string" || !value || value.length > 2048 || /[\u0000-\u001f\u007f]/.test(value)) return null;
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

export function providerTrackingUrl(provider, value) {
  if (typeof value !== "string" || !value || value.length > 2048 || /[\u0000-\u001f\u007f]/.test(value)) return null;
  if (provider === "doordash") return recipientTrackingUrl(value);
  if (provider === "amazon_account") {
    try {
      const url = new URL(value);
      const parameters = new Set(["orderId", "shipmentId", "packageId", "packageIndex", "lineItemId", "itemId", "trackingId"]);
      const path = url.pathname.replace(/^\/-\/en(?=\/)/, "");
      if (url.protocol === "https:" && url.host === "www.amazon.com" &&
          ["/gp/your-account/ship-track", "/progress-tracker/package"].includes(path) &&
          !url.username && !url.password && !url.hash && !/[\\\s]/.test(value) &&
          [...url.searchParams].every(([key, value]) => parameters.has(key) && /^[A-Za-z0-9_-]{1,128}$/.test(value) && url.searchParams.getAll(key).length === 1) &&
          url.searchParams.has("orderId")) return url;
    } catch {
      // Only the account's actual, validated tracking page may be rendered.
    }
    return null;
  }
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

// Resolve once for every UI surface; never invent a provider tracker URL.
export function trackingLinks(shipment) {
  const provider = shipment.sync?.provider;
  const carrier = typeof shipment.tracking?.carrier === "string" ? shipment.tracking.carrier.toLowerCase() : "";
  const carrierLink = [carrier === "doordash" ? "DoorDash recipient tracking" : "Carrier tracking", carrierTrackingUrl(shipment.tracking)];
  const providerUrl = providerTrackingUrl(provider, shipment.sync?.tracking_url);
  const providerLink = [`${providerNames[provider] || "Provider"} tracking`, providerUrl];
  // Use the account's actual private page when present, never a fabricated one.
  const candidates = provider === "amazon_account" && providerUrl
    ? [providerLink, ...(carrier === "amazonaccount" ? [] : [carrierLink])]
    : [carrierLink, providerLink];
  const seen = new Set();
  return candidates.filter(([, url]) => {
    if (!url || seen.has(url.href)) return false;
    seen.add(url.href);
    return true;
  });
}
