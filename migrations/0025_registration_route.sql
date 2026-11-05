-- Resume an automatic admission's provider without scanning other numbers.
CREATE INDEX tracking_verifications_number_idx
    ON tracking_verifications(tracking_number);
