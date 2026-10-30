export const providerNames = { easypost: "EasyPost", aftership: "AfterShip", doordash: "DoorDash" };

// Only documented public tracking routes belong here. Carrier aliases share
// one route; the API provider (EasyPost/AfterShip) does not identify the carrier.
const carrierRoutes = [
  [["usps", "uspsship"], "https://tools.usps.com/go/TrackConfirmAction", "tLabels"],
  [["ups", "upsdap", "upsmailinnovations"], "https://www.ups.com/track", "tracknum"],
  [["fedex", "fedexdefault", "fedexsmartpost"], "https://www.fedex.com/fedextrack/", "trknbr"],
  [["dhl", "dhlexpress", "dhlexpressdefault"], "https://www.dhl.com/us-en/home/tracking.html?submit=1", "tracking-id"],
  [["amazonmcf"], "https://www.swiship.com/track", "id"],
  [["amazonshipping"], "https://track.amazon.com/tracking/", null],
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
  // Amazon Logistics TBA numbers use Amazon's recipient tracker. Swiship
  // does not resolve every Logistics shipment represented as AmazonMcf.
  const template = carrierTemplates.get(carrier === "amazonmcf" && /^TBA/i.test(number) ? "amazonshipping" : carrier);
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
  const candidates = [
    [typeof shipment.tracking?.carrier === "string" && shipment.tracking.carrier.toLowerCase() === "doordash" ? "DoorDash recipient tracking" : "Carrier tracking", carrierTrackingUrl(shipment.tracking)],
    [`${providerNames[provider] || "Provider"} tracking`, providerTrackingUrl(provider, shipment.sync?.tracking_url)],
  ];
  const seen = new Set();
  return candidates.filter(([, url]) => {
    if (!url || seen.has(url.href)) return false;
    seen.add(url.href);
    return true;
  });
}
