import assert from "node:assert/strict";
import test from "node:test";
import { ApiError, safeRequestId } from "../web/api-error.js";

const requestId = "fbf22920-3494-4f72-833f-dd34eb2e323b";

test("API errors preserve existing message/status callers and validated correlation metadata", () => {
  const error = new ApiError("Existing conflict message", 409, {
    error_code: "amazon_connect_registration_account_identity_missing",
    request_id: requestId.toUpperCase(),
    message: "Unused provider body",
  });
  assert.ok(error instanceof Error);
  assert.equal(error.message, "Existing conflict message");
  assert.equal(error.status, 409);
  assert.equal(error.code, "amazon_connect_registration_account_identity_missing");
  assert.equal(error.requestId, requestId);
  const legacy = new ApiError("Existing failure", 503);
  assert.equal(legacy.message, "Existing failure");
  assert.equal(legacy.status, 503);
  assert.equal(legacy.code, null);
  assert.equal(legacy.requestId, null);
});

test("invalid error metadata is discarded without coercing arbitrary values", () => {
  for (const error_code of [null, 10, {}, [], "", "Amazon_connect_registration", "amazon_connect_registration_timeout\n", "amazon_connect_registration_timeout?token=SECRET", "a".repeat(129)]) {
    const error = new ApiError("Failure", 400, { error_code, request_id: requestId });
    assert.equal(error.code, null);
    assert.equal(error.requestId, requestId);
  }
  for (const id of [null, 1, {}, [], "", requestId + "\n", requestId + "?code=SECRET", "https://example.test/" + requestId, "SYNTHETIC_SECRET", "fbf22920-3494-4f72-833f-dd34eb2e323g"]) {
    assert.equal(safeRequestId(id), null);
    assert.equal(new ApiError("Failure", 400, { request_id: id }).requestId, null);
  }
  for (const body of [null, 1, "invalid envelope", []]) {
    const error = new ApiError("Failure", 400, body);
    assert.equal(error.code, null);
    assert.equal(error.requestId, null);
  }
});
