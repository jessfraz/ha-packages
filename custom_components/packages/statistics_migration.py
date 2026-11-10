"""Move legacy count statistics on Recorder's worker in one transaction."""

import asyncio
import hashlib
import json
import logging
import threading
from collections.abc import Callable
from dataclasses import dataclass, replace
from typing import Literal, TypedDict

import probatio as vol
from homeassistant.components.recorder import Recorder
from homeassistant.components.recorder.db_schema import (
    Statistics,
    StatisticsMeta,
    StatisticsShortTerm,
)
from homeassistant.components.recorder.models import StatisticMeanType
from homeassistant.components.recorder.statistics import (
    get_short_term_statistics_run_cache,
)
from homeassistant.components.recorder.tasks import RecorderTask
from homeassistant.config_entries import (
    SIGNAL_CONFIG_ENTRY_CHANGED,
    ConfigEntry,
    ConfigEntryChange,
    ConfigEntryState,
)
from homeassistant.const import EVENT_HOMEASSISTANT_STOP
from homeassistant.core import (
    Event,
    EventStateChangedData,
    HomeAssistant,
    ServiceCall,
    ServiceResponse,
    SupportsResponse,
    callback,
)
from homeassistant.exceptions import (
    HomeAssistantError,
    ServiceValidationError,
    Unauthorized,
)
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers.dispatcher import async_dispatcher_connect
from homeassistant.helpers.recorder import DATA_INSTANCE, session_scope
from sqlalchemy import case, delete, select, update
from sqlalchemy.orm import Session

from .const import DOMAIN, SUMMARY_KEYS

_LOGGER = logging.getLogger(__name__)
SERVICE = "migrate_shipments_statistics"
_LEGACY_KEYS = (
    *SUMMARY_KEYS,
    "inbound_delivered_today",
    "inbound_due_today",
    "inbound_active",
    "inbound_out_for_delivery",
    "inbound_exceptions",
    "outbound_active",
    "outbound_exceptions",
)
_MAX_ROWS = 100_000
_TABLES = (Statistics, StatisticsShortTerm)
type StatisticsTable = type[Statistics] | type[StatisticsShortTerm]
type EntityPair = tuple[str, str]


def _metadata_contract(meta: StatisticsMeta) -> tuple:
    return (
        meta.source,
        meta.mean_type,
        meta.has_sum,
        meta.unit_class,
        meta.unit_of_measurement,
        meta.name,
    )


class MigrationError(HomeAssistantError):
    """The reviewed history or entity ownership no longer permits migration."""


class TableCounts(TypedDict):
    """Only technical counts leave the Recorder job."""

    source: int
    destination: int
    identical_overlaps: int
    resulting: int


class MigrationReport(TypedDict):
    """A review token and the observed result, without sensor values."""

    status: Literal["preview", "migrated", "already_complete"]
    digest: str
    pairs: list[list[str]]
    tables: dict[str, TableCounts]


@dataclass(frozen=True)
class StoredRow:
    """Preserve every stored value, including row identity and creation time."""

    id: int
    metadata_id: int
    start: float
    created: float | None
    values: tuple[float | None, ...]


def _read_rows(
    session: Session, table: StatisticsTable, metadata_ids: list[int]
) -> tuple[StoredRow, ...]:
    statement = (
        select(
            table.id,
            table.metadata_id,
            table.start_ts,
            table.created_ts,
            table.mean,
            table.mean_weight,
            table.min,
            table.max,
            table.last_reset_ts,
            table.state,
            table.sum,
        )
        .where(table.metadata_id.in_(metadata_ids))
        .order_by(table.metadata_id, table.start_ts)
        .limit(_MAX_ROWS + 1)
    )
    rows = tuple(
        StoredRow(id_, metadata_id, start, created, tuple(values))
        for id_, metadata_id, start, created, *values in session.execute(statement)
    )
    if len(rows) > _MAX_ROWS:
        raise MigrationError("Statistics exceed the migration review limit")
    if any(row.start is None for row in rows):
        raise MigrationError("Statistics have an invalid timestamp")
    return rows


def _check_abort(aborted: threading.Event) -> None:
    if aborted.is_set():
        raise MigrationError("Migration cancelled or entity ownership changed")


def _merge(
    recorder: Recorder,
    entry_id: str,
    pairs: tuple[EntityPair, ...],
    expected_digest: str | None,
    aborted: threading.Event,
) -> MigrationReport:
    """Serialize with native statistics compilation and commit both tables together."""
    _check_abort(aborted)
    ids = [entity_id for pair in pairs for entity_id in pair]
    with session_scope(session=recorder.get_session()) as session:
        metadata = {
            meta.statistic_id: meta
            for meta in session.scalars(
                select(StatisticsMeta).where(StatisticsMeta.statistic_id.in_(ids))
            )
        }
        destinations: dict[int, int] = {}
        legacy_ids = []
        for old, current in pairs:
            if (target := metadata.get(current)) is None:
                raise MigrationError("Packages statistics have not initialized")
            if (source := metadata.get(old)) is None:
                continue
            if (
                _metadata_contract(source) != _metadata_contract(target)
                or source.source != "recorder"
                or source.mean_type != StatisticMeanType.ARITHMETIC
                or source.has_sum
                or source.unit_of_measurement is not None
                or source.unit_class not in (None, "unitless")
            ):
                raise MigrationError("Legacy and Packages statistic metadata differ")
            destinations[source.id] = target.id
            legacy_ids.append(old)

        metadata_ids = [meta.id for meta in metadata.values()]
        before = {table: _read_rows(session, table, metadata_ids) for table in _TABLES}
        digest = hashlib.sha256(
            json.dumps(
                [
                    entry_id,
                    pairs,
                    sorted(
                        (
                            meta.statistic_id,
                            meta.id,
                            meta.source,
                            meta.mean_type,
                            meta.has_sum,
                            meta.unit_class,
                            meta.unit_of_measurement,
                            meta.name,
                        )
                        for meta in metadata.values()
                    ),
                    [
                        [
                            (
                                row.id,
                                row.metadata_id,
                                row.start,
                                row.created,
                                row.values,
                            )
                            for row in before[table]
                        ]
                        for table in _TABLES
                    ],
                ],
                allow_nan=False,
                separators=(",", ":"),
            ).encode()
        ).hexdigest()
        report: MigrationReport = {
            "status": "preview" if legacy_ids else "already_complete",
            "digest": digest,
            "pairs": [list(pair) for pair in pairs],
            "tables": {},
        }
        expected_rows = {}
        duplicates = {}
        for table, rows in before.items():
            current = {
                (row.metadata_id, row.start): row
                for row in rows
                if row.metadata_id not in destinations
            }
            duplicate_ids = []
            result = list(current.values())
            for row in rows:
                if (target_id := destinations.get(row.metadata_id)) is None:
                    continue
                if (existing := current.get((target_id, row.start))) is not None:
                    if existing.values != row.values:
                        raise MigrationError(
                            "Overlapping statistics have different values"
                        )
                    duplicate_ids.append(row.id)
                else:
                    result.append(replace(row, metadata_id=target_id))
            duplicates[table] = duplicate_ids
            expected_rows[table] = {row.id: row for row in result}
            report["tables"][table.__tablename__] = {
                "source": len(rows) - len(current),
                "destination": len(current),
                "identical_overlaps": len(duplicate_ids),
                "resulting": len(result),
            }

        if expected_digest is None or not legacy_ids:
            return report
        if digest != expected_digest:
            raise MigrationError("Statistics changed; preview the migration again")
        _check_abort(aborted)
        for table in _TABLES:
            duplicate_ids = duplicates[table]
            chunk_size = min(400, recorder.max_bind_vars - len(destinations))
            for offset in range(0, len(duplicate_ids), chunk_size):
                session.execute(
                    delete(table).where(
                        table.id.in_(duplicate_ids[offset : offset + chunk_size]),
                        table.metadata_id.in_(destinations),
                    )
                )
            session.execute(
                update(table)
                .where(table.metadata_id.in_(destinations))
                .values(metadata_id=case(destinations, value=table.metadata_id))
            )
            actual = {row.id: row for row in _read_rows(session, table, metadata_ids)}
            if actual != expected_rows[table]:
                raise MigrationError("Recorder migration readback did not match")
        # Both tables have been checked before retiring only the now-empty metadata.
        # Any exception or cancellation rolls this entire transaction back.
        recorder.statistics_meta_manager.delete(session, legacy_ids)
        _check_abort(aborted)
        report["status"] = "migrated"
    # Native readers repopulate this cache after commit. Invalidating the old and
    # destination IDs also handles a destination which previously had no rows.
    get_short_term_statistics_run_cache(recorder.hass).remove_metadata_ids(metadata_ids)
    return report


@dataclass(slots=True)
class MigrationTask(RecorderTask):
    """Keep the transaction on Core's sole Recorder writer, including retries."""

    entry_id: str
    pairs: tuple[EntityPair, ...]
    expected_digest: str | None
    aborted: threading.Event
    finish: Callable[[MigrationReport | HomeAssistantError], None]

    def run(self, instance: Recorder) -> None:
        try:
            result = _merge(
                instance, self.entry_id, self.pairs, self.expected_digest, self.aborted
            )
        except MigrationError as error:
            instance.hass.loop.call_soon_threadsafe(self.finish, error)
        except Exception:
            # A lost acknowledgement is safe to inspect and retry: the database
            # transaction, not a separate checkpoint file, owns completion.
            _LOGGER.error(
                "Recorder statistics migration failed; review before retrying"
            )
            instance.hass.loop.call_soon_threadsafe(
                self.finish, MigrationError("Recorder migration failed")
            )
        else:
            instance.hass.loop.call_soon_threadsafe(self.finish, result)


def _owned_pairs(hass: HomeAssistant, entry_id: str) -> tuple[EntityPair, ...]:
    entry = hass.config_entries.async_get_entry(entry_id)
    if (
        entry is None
        or entry.domain != DOMAIN
        or entry.state is not ConfigEntryState.LOADED
        or len(hass.config_entries.async_entries(DOMAIN)) != 1
        or hass.config_entries.async_entries("shipments")
    ):
        raise ServiceValidationError("Select the single loaded Packages connection")
    registry = er.async_get(hass)
    pairs = []
    for key in _LEGACY_KEYS:
        old = f"sensor.shipments_{key}"
        current = registry.async_get_entity_id("sensor", DOMAIN, f"{entry_id}_{key}")
        target = registry.async_get(current) if current else None
        if (
            target is None
            or target.config_entry_id != entry_id
            or target.disabled_by is not None
            or current == old
        ):
            raise ServiceValidationError(
                "Packages count sensor ownership is incomplete"
            )
        if registry.async_get(old) is not None or hass.states.get(old) is not None:
            raise ServiceValidationError("Legacy Shipments sensors are still present")
        pairs.append((old, target.entity_id))
    return tuple(pairs)


@callback
def async_register_statistics_migration(hass: HomeAssistant) -> None:
    """Register an administrator-only review/commit action with no arbitrary IDs."""

    async def handle(call: ServiceCall) -> ServiceResponse:
        user_id = call.context.user_id
        user = await hass.auth.async_get_user(user_id) if user_id else None
        if user is None or not user.is_active or not user.is_admin:
            raise Unauthorized(context=call.context)
        pairs = _owned_pairs(hass, call.data["config_entry_id"])
        if (
            recorder := hass.data.get(DATA_INSTANCE)
        ) is None or not recorder.is_running:
            raise ServiceValidationError("Recorder is not running")
        aborted = threading.Event()
        watched = {entity_id for pair in pairs for entity_id in pair}
        sources = {old for old, _ in pairs}

        @callback
        def registry_changed(event: Event[er.EventEntityRegistryUpdatedData]) -> None:
            if (
                event.data.get("entity_id") in watched
                or event.data.get("old_entity_id") in watched
            ):
                aborted.set()

        @callback
        def state_changed(event: Event[EventStateChangedData]) -> None:
            if (
                event.data["entity_id"] in sources
                and event.data["new_state"] is not None
            ):
                aborted.set()

        entry = hass.config_entries.async_get_entry(call.data["config_entry_id"])
        if entry is None:
            raise ServiceValidationError("Packages connection was removed")

        @callback
        def entry_changed(change: ConfigEntryChange, changed: ConfigEntry) -> None:
            if changed.domain in (DOMAIN, "shipments"):
                aborted.set()

        @callback
        def stopping(event: Event) -> None:
            aborted.set()

        unsubscribe = [
            hass.bus.async_listen(er.EVENT_ENTITY_REGISTRY_UPDATED, registry_changed),
            hass.bus.async_listen("state_changed", state_changed),
            async_dispatcher_connect(hass, SIGNAL_CONFIG_ENTRY_CHANGED, entry_changed),
            hass.bus.async_listen_once(EVENT_HOMEASSISTANT_STOP, stopping),
        ]
        future: asyncio.Future[MigrationReport] = hass.loop.create_future()

        @callback
        def finish(result: MigrationReport | HomeAssistantError) -> None:
            if future.done():
                return
            if isinstance(result, HomeAssistantError):
                future.set_exception(result)
            else:
                future.set_result(result)

        try:
            recorder.queue_task(
                MigrationTask(
                    entry.entry_id,
                    pairs,
                    call.data.get("expected_digest"),
                    aborted,
                    finish,
                )
            )
            async with asyncio.timeout(60):
                report = await asyncio.shield(future)
                return {
                    "status": report["status"],
                    "digest": report["digest"],
                    "pairs": [[old, current] for old, current in report["pairs"]],
                    "tables": {
                        name: {
                            "source": counts["source"],
                            "destination": counts["destination"],
                            "identical_overlaps": counts["identical_overlaps"],
                            "resulting": counts["resulting"],
                        }
                        for name, counts in report["tables"].items()
                    },
                }
        except TimeoutError:
            raise MigrationError(
                "Migration timed out; preview before retrying"
            ) from None
        finally:
            aborted.set()
            for remove_listener in unsubscribe:
                remove_listener()
            future.cancel()

    hass.services.async_register(
        DOMAIN,
        SERVICE,
        handle,
        schema=vol.Schema(
            {
                vol.Required("config_entry_id"): str,
                vol.Optional("expected_digest"): vol.Match(r"^[0-9a-f]{64}$"),
            }
        ),
        supports_response=SupportsResponse.ONLY,
    )
