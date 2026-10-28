// Civil dates remain civil; real timestamps use Home Assistant's timezone.
export function calendarDate(value, today = new Date(), timeZone) {
  if (typeof value !== "string" || !/^\d{4}-\d{2}-\d{2}(?:$|T.*(?:Z|[+-]\d{2}:\d{2})$)/i.test(value)) return "date not reported";
  const formatter = new Intl.DateTimeFormat("en-CA", {
    year: "numeric", month: "2-digit", day: "2-digit", ...(timeZone ? { timeZone } : {}),
  });
  const parts = (instant) => Object.fromEntries(formatter.formatToParts(instant).map(({ type, value }) => [type, value]));
  const civil = value.length === 10;
  const instant = new Date(civil ? value + "T12:00:00Z" : value);
  if (Number.isNaN(instant.getTime())) return "date not reported";
  const day = civil ? value : (() => { const p = parts(instant); return `${p.year}-${p.month}-${p.day}`; })();
  if (civil && instant.toISOString().slice(0, 10) !== value) return "date not reported";
  const current = parts(today);
  const currentDay = `${current.year}-${current.month}-${current.day}`;
  // UTC is used only to compare civil date ordinals, never to interpret an ETA.
  const difference = (Date.parse(day) - Date.parse(currentDay)) / 86400000;
  if (difference === 0) return "today";
  if (difference === 1) return "tomorrow";
  if (difference === -1) return "yesterday";
  return new Date(day + "T12:00:00Z").toLocaleDateString([], {
    timeZone: "UTC", weekday: "short", month: "short", day: "numeric",
    ...(day.slice(0, 4) !== current.year ? { year: "numeric" } : {}),
  });
}
