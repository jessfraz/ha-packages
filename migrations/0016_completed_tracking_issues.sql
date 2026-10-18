-- A manual terminal correction acknowledges outstanding tracking problems.
-- Keep tracker ownership and uncertain registration intent for reconciliation
-- if the correction is later removed. Carrier history remains unchanged.
UPDATE shipments SET last_error=NULL,last_error_code=NULL,
    last_provider_status=NULL,last_provider_error_code=NULL,attempts=0,
    registration_state=CASE WHEN tracker_id IS NULL THEN 'pending' ELSE 'registered' END,
    next_attempt_at=NULL
WHERE override_status IN ('"delivered"','"return_to_sender"','"cancelled"');
