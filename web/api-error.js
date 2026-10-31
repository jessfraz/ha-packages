// Validate correlation metadata before a UI uses it.
export function safeRequestId(value) {
  return typeof value === "string" && /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(value)
    ? value.toLowerCase() : null;
}

export class ApiError extends Error {
  constructor(message, status, body) {
    super(message);
    this.status = status;
    this.code = typeof body?.error_code === "string" && /^[a-z][a-z0-9_]{0,127}$/.test(body.error_code)
      ? body.error_code : null;
    this.requestId = safeRequestId(body?.request_id);
  }
}
