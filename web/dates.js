function civilDay(instant, timeZone) {
  const formatter = new Intl.DateTimeFormat("en-CA", {
    year: "numeric", month: "2-digit", day: "2-digit", ...(timeZone ? { timeZone } : {}),
  });
  const parts = Object.fromEntries(formatter.formatToParts(instant).map(({ type, value }) => [type, value]));
  return `${parts.year}-${parts.month}-${parts.day}`;
}

// Civil dates remain civil; real timestamps use Home Assistant's timezone.
export function calendarDate(value, today = new Date(), timeZone) {
  if (typeof value !== "string" || !/^\d{4}-\d{2}-\d{2}(?:$|T.*(?:Z|[+-]\d{2}:\d{2})$)/i.test(value)) return "date not reported";
  const civil = value.length === 10;
  const instant = new Date(civil ? value + "T12:00:00Z" : value);
  if (Number.isNaN(instant.getTime())) return "date not reported";
  const day = civil ? value : civilDay(instant, timeZone);
  if (civil && instant.toISOString().slice(0, 10) !== value) return "date not reported";
  const currentDay = civilDay(today, timeZone);
  // UTC is used only to compare civil date ordinals, never to interpret an ETA.
  const difference = (Date.parse(day) - Date.parse(currentDay)) / 86400000;
  if (difference === 0) return "today";
  if (difference === 1) return "tomorrow";
  if (difference === -1) return "yesterday";
  return new Date(day + "T12:00:00Z").toLocaleDateString([], {
    timeZone: "UTC", weekday: "short", month: "short", day: "numeric",
    ...(day.slice(0, 4) !== currentDay.slice(0, 4) ? { year: "numeric" } : {}),
  });
}

export function deliveryDescription(shipment, today = new Date(), timeZone, deliveredView = false) {
  const delivered = shipment.status === "delivered" || deliveredView;
  const label = delivered ? "Delivered" : "Expected delivery";
  const tracking = shipment.carrier_tracking;
  const value = delivered
    ? tracking?.delivered_at
    : tracking?.estimated_delivery_at;
  if (value) {
    const date = calendarDate(value, today, timeZone);
    if (date !== "date not reported") return { label, value: date, summary: `${delivered ? "Delivered" : "Expected"} ${date}` };
  }
  if (!delivered && tracking?.estimated_delivery_window) {
    const { start, end } = tracking.estimated_delivery_window;
    const first = calendarDate(start, today, timeZone);
    const last = calendarDate(end, today, timeZone);
    if (first !== "date not reported" && last !== "date not reported" && Date.parse(end) >= Date.parse(start)) {
      const range = first === last ? first : `${first} – ${last}`;
      return { label, value: range, summary: `Expected ${range}` };
    }
  }
  const report = tracking?.delivery_date;
  if (report && (delivered ? report.kind === "delivered" : report.kind === "estimated" || report.kind === "estimated_range")) {
    const first = calendarDate(report.kind === "estimated_range" ? report.start : report.date, today, timeZone);
    const last = report.kind === "estimated_range" ? calendarDate(report.end, today, timeZone) : first;
    if (first !== "date not reported" && last !== "date not reported" &&
        (report.kind !== "estimated_range" || Date.parse(report.end) >= Date.parse(report.start))) {
      const range = first === last ? first : `${first} – ${last}`;
      return { label, value: range, summary: `${delivered ? "Delivered" : "Expected"} ${range}` };
    }
  }
  if (delivered && shipment.status_override?.status === "delivered") {
    const date = calendarDate(shipment.status_override.set_at, today, timeZone);
    if (date !== "date not reported") return { label, value: date, summary: `Delivered ${date}` };
  }
  return { label, value: "Not reported yet", summary: delivered ? "Delivery date not reported" : "Delivery date not reported yet" };
}
