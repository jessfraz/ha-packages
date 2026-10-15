-- A documented setup failure needs a human fix, not another automatic poll.
-- Keep the existing constrained storage state; the owned error code exposes
-- the more specific configuration_required state through the API.
UPDATE shipments SET registration_state='rejected',next_attempt_at=NULL,
    lease_token=NULL,lease_until=NULL
WHERE last_error_code IN ('tracking_provider_credentials',
    'tracking_carrier_account_required','tracking_carrier_credentials_required',
    'tracking_test_key')
  AND (last_provider_status IS NULL OR last_provider_status IN (400,401,403,404,422));
