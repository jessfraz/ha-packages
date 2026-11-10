# Move the old Shipments statistics

The rename can leave count history under `sensor.shipments_*` while the current
sensors record under new Packages IDs. Clearing those orphaned statistics loses
history.

After a completed Home Assistant backup, an administrator can call
`packages.migrate_shipments_statistics` with the Packages `config_entry_id`.
This returns a preview with row counts, source and destination IDs, and a digest.
It does not change history. Call the same action with that `expected_digest` to
confirm. If new observations arrive first, request another preview.

The move runs on Home Assistant's Recorder worker in one database transaction.
It keeps existing destination observations, moves the missing hourly and
five-minute rows with their stored values and timestamps, and deduplicates only
identical overlapping observations. Different overlapping values, changed
metadata, an active old sensor, or changed entity ownership stop the transaction.
Only the empty old statistic metadata is removed after both tables verify.
Sensor IDs, configuration, and state history stay in place.

An interrupted call can be checked by previewing again. A failed transaction
rolls back; a committed move has no old statistics left and reports
`already_complete`. Completion comes from Recorder's database, without a separate
checkpoint that can fall behind it. The orphaned-statistics Repair clears when
Spook next inspects the migrated history.
