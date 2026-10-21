export function calendarDate(value, today = new Date()) {
  // ISO dates are civil dates. Date.parse would interpret them as UTC.
  const civil = typeof value === "string" && /^\d{4}-\d{2}-\d{2}$/.test(value);
  const date = new Date(civil ? `${value}T12:00:00` : value);
  if (Number.isNaN(date.getTime())) return "date not reported";
  const day = new Date(date.getFullYear(), date.getMonth(), date.getDate());
  const currentDay = new Date(today.getFullYear(), today.getMonth(), today.getDate());
  const difference = Math.round((day - currentDay) / 86_400_000);
  if (difference === 0) return "today";
  if (difference === 1) return "tomorrow";
  if (difference === -1) return "yesterday";
  return date.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric",
    ...(date.getFullYear() !== today.getFullYear() ? { year: "numeric" } : {}) });
}

