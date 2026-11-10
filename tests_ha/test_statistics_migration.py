"""Merge real Core Recorder history without replacing destination observations."""

import asyncio
import json
import os
import sys
import threading
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

import pytest
from homeassistant.components.recorder import Recorder
from homeassistant.components.recorder import migration as recorder_migration
from homeassistant.components.recorder.db_schema import (
    Statistics,
    StatisticsMeta,
    StatisticsShortTerm,
)
from homeassistant.components.recorder.models import (
    StatisticData,
    StatisticMeanType,
    StatisticMetaData,
)
from homeassistant.components.recorder.statistics import statistics_during_period
from homeassistant.components.recorder.tasks import RecorderTask
from homeassistant.config_entries import SOURCE_IGNORE
from homeassistant.core import Context, HomeAssistant
from homeassistant.exceptions import HomeAssistantError, Unauthorized
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers import recorder as recorder_helper
from homeassistant.helpers.recorder import session_scope
from pytest_homeassistant.common import MockConfigEntry, MockUser
from sqlalchemy import event as sql_event
from sqlalchemy import select
from sqlalchemy.orm import Session

from custom_components.packages.const import DOMAIN, SENSOR_KEYS

KEYS = tuple(key for key in SENSOR_KEYS if not key.startswith("pickups_"))
START = datetime(2026, 1, 2, 10, tzinfo=UTC)
SERVICE = "migrate_shipments_statistics"


@pytest.fixture
def mock_recorder_before_hass(recorder_db_url: str, monkeypatch) -> None:
    """Select the real SQLite database before the HA fixture initializes."""
    # Python3.14 evaluates this TYPE_CHECKING-only annotation when the upstream
    # Recorder fixture autospecs schema validation. The worker itself stays real.
    monkeypatch.setattr(recorder_migration, "Recorder", Recorder, raising=False)
    monkeypatch.setattr(recorder_helper, "Recorder", Recorder, raising=False)
    monkeypatch.setattr(recorder_helper, "Session", Session, raising=False)


@pytest.fixture
async def statistics_entry(
    recorder_mock: Recorder, entry: MockConfigEntry, hass: HomeAssistant
) -> MockConfigEntry:
    await hass.async_start()
    await hass.async_block_till_done()
    await hass.async_add_executor_job(recorder_mock.block_till_done)
    return entry


def metadata(statistic_id: str) -> StatisticMetaData:
    return {
        "statistic_id": statistic_id,
        "source": "recorder",
        "mean_type": StatisticMeanType.ARITHMETIC,
        "has_sum": False,
        "name": None,
        "unit_class": "unitless",
        "unit_of_measurement": None,
    }


def observation(start: datetime, value: float) -> StatisticData:
    return {"start": start, "mean": value, "min": value - 1, "max": value + 1}


async def seed(recorder: Recorder) -> None:
    """Use the native producer for hourly and five-minute records."""
    for key in KEYS:
        old, current = f"sensor.shipments_{key}", f"sensor.packages_{key}"
        for table in (Statistics, StatisticsShortTerm):
            step = table.duration
            recorder.async_import_statistics(
                metadata(old),
                [observation(START, 2), observation(START + step, 3)],
                table,
            )
            recorder.async_import_statistics(
                metadata(current),
                [observation(START + step, 3), observation(START + 2 * step, 4)],
                table,
            )
    recorder.async_import_statistics(
        metadata("sensor.unrelated_measurement"), [observation(START, 99)], Statistics
    )
    await recorder.hass.async_add_executor_job(recorder.block_till_done)


def database_rows(recorder: Recorder) -> dict[str, dict[str, list[tuple]]]:
    """Read native rows independently, including IDs and creation timestamps."""
    with session_scope(session=recorder.get_session(), read_only=True) as session:
        names = {
            id_: name
            for id_, name in session.execute(
                select(StatisticsMeta.id, StatisticsMeta.statistic_id)
            )
        }
        result = {}
        for table in (Statistics, StatisticsShortTerm):
            grouped = {}
            for row in session.scalars(
                select(table).order_by(table.start_ts, table.id)
            ):
                grouped.setdefault(names[row.metadata_id], []).append(
                    (
                        row.id,
                        row.created_ts,
                        row.start_ts,
                        row.mean,
                        row.mean_weight,
                        row.min,
                        row.max,
                        row.last_reset_ts,
                        row.state,
                        row.sum,
                    )
                )
            result[table.__tablename__] = grouped
        return result


async def migrate(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    user: MockUser,
    digest: str | None = None,
):
    fields = {"config_entry_id": entry.entry_id}
    if digest is not None:
        fields["expected_digest"] = digest
    return await hass.services.async_call(
        DOMAIN,
        SERVICE,
        fields,
        context=Context(user_id=user.id),
        blocking=True,
        return_response=True,
    )


def database_metadata(recorder: Recorder) -> dict[str, int]:
    with session_scope(session=recorder.get_session(), read_only=True) as session:
        return {
            name: id_
            for name, id_ in session.execute(
                select(StatisticsMeta.statistic_id, StatisticsMeta.id)
            )
        }


async def test_native_merge_preserves_both_tables_and_destination_rows(
    hass: HomeAssistant,
    recorder_mock: Recorder,
    statistics_entry: MockConfigEntry,
    hass_admin_user: MockUser,
) -> None:
    await seed(recorder_mock)
    before = await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
    metadata_before = await recorder_mock.async_add_executor_job(
        database_metadata, recorder_mock
    )
    registry_before = dict(er.async_get(hass).entities)
    preview = await migrate(hass, statistics_entry, hass_admin_user)
    assert preview["status"] == "preview"
    assert (
        await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
        == before
    )
    result = await migrate(hass, statistics_entry, hass_admin_user, preview["digest"])
    assert result["status"] == "migrated"
    after = await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
    assert await recorder_mock.async_add_executor_job(
        database_metadata, recorder_mock
    ) == {
        name: id_
        for name, id_ in metadata_before.items()
        if not name.startswith("sensor.shipments_")
    }
    assert dict(er.async_get(hass).entities) == registry_before
    for table in before:
        for key in KEYS:
            old, current = f"sensor.shipments_{key}", f"sensor.packages_{key}"
            assert old not in after[table]
            assert after[table][current] == sorted(
                [before[table][old][0], *before[table][current]], key=lambda row: row[2]
            )
        assert after[table].get("sensor.unrelated_measurement") == before[table].get(
            "sensor.unrelated_measurement"
        )
    retry = await migrate(hass, statistics_entry, hass_admin_user, preview["digest"])
    assert retry["status"] == "already_complete"
    assert (
        await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
        == after
    )
    for period in ("hour", "5minute"):
        consumed = await recorder_mock.async_add_executor_job(
            statistics_during_period,
            hass,
            START,
            None,
            {f"sensor.packages_{key}" for key in KEYS},
            period,
            None,
            {"mean", "min", "max"},
        )
        assert len(consumed) == 14
        assert all(len(rows) == 3 for rows in consumed.values())


@pytest.mark.parametrize("table", [Statistics, StatisticsShortTerm])
async def test_conflicting_overlap_aborts_without_changing_either_table(
    hass: HomeAssistant,
    recorder_mock: Recorder,
    statistics_entry: MockConfigEntry,
    hass_admin_user: MockUser,
    table,
) -> None:
    await seed(recorder_mock)
    recorder_mock.async_import_statistics(
        metadata("sensor.packages_active"),
        [observation(START + table.duration, 100)],
        table,
    )
    await hass.async_add_executor_job(recorder_mock.block_till_done)
    before = await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
    with pytest.raises(HomeAssistantError, match="different values"):
        await migrate(hass, statistics_entry, hass_admin_user)
    assert (
        await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
        == before
    )


async def test_native_write_after_review_invalidates_digest(
    hass: HomeAssistant,
    recorder_mock: Recorder,
    statistics_entry: MockConfigEntry,
    hass_admin_user: MockUser,
) -> None:
    await seed(recorder_mock)
    preview = await migrate(hass, statistics_entry, hass_admin_user)
    recorder_mock.async_import_statistics(
        metadata("sensor.packages_active"),
        [observation(START + 4 * StatisticsShortTerm.duration, 5)],
        StatisticsShortTerm,
    )
    await hass.async_add_executor_job(recorder_mock.block_till_done)
    before = await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
    with pytest.raises(HomeAssistantError, match="changed; preview"):
        await migrate(hass, statistics_entry, hass_admin_user, preview["digest"])
    assert (
        await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
        == before
    )


async def test_concurrent_confirmations_commit_once(
    hass: HomeAssistant,
    recorder_mock: Recorder,
    statistics_entry: MockConfigEntry,
    hass_admin_user: MockUser,
) -> None:
    await seed(recorder_mock)
    preview = await migrate(hass, statistics_entry, hass_admin_user)
    results = await asyncio.gather(
        *(
            migrate(hass, statistics_entry, hass_admin_user, preview["digest"])
            for _ in range(2)
        )
    )
    assert sorted(result["status"] for result in results) == [
        "already_complete",
        "migrated",
    ]


@pytest.mark.parametrize(
    "identity", ["user", "anonymous", "wrong_entry", "two_entries"]
)
async def test_only_the_explicit_single_entry_administrator_can_migrate(
    hass: HomeAssistant,
    recorder_mock: Recorder,
    statistics_entry: MockConfigEntry,
    hass_admin_user: MockUser,
    hass_user: MockUser,
    identity: str,
) -> None:
    await seed(recorder_mock)
    before = await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
    user_id = hass_user.id if identity == "user" else hass_admin_user.id
    if identity == "anonymous":
        user_id = None
    entry_id = statistics_entry.entry_id
    if identity == "wrong_entry":
        entry_id = "not-the-loaded-connection"
    if identity == "two_entries":
        MockConfigEntry(domain=DOMAIN).add_to_hass(hass)
    expected = Unauthorized if identity in {"anonymous", "user"} else HomeAssistantError
    with pytest.raises(expected):
        await hass.services.async_call(
            DOMAIN,
            SERVICE,
            {"config_entry_id": entry_id},
            blocking=True,
            return_response=True,
            context=Context(user_id=user_id),
        )
    assert (
        await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
        == before
    )


@dataclass(slots=True)
class QueueGate(RecorderTask):
    """Pause the actual worker to exercise ownership changes before admission."""

    started: asyncio.Event
    release: threading.Event

    def run(self, instance: Recorder) -> None:
        instance.hass.loop.call_soon_threadsafe(self.started.set)
        self.release.wait(10)


@pytest.mark.parametrize(
    "change", ["source", "rename", "unload", "cancel", "second_entry", "options"]
)
async def test_queued_migration_cannot_outlive_its_entry_or_source_precondition(
    hass: HomeAssistant,
    recorder_mock: Recorder,
    statistics_entry: MockConfigEntry,
    hass_admin_user: MockUser,
    monkeypatch,
    change: str,
) -> None:
    from custom_components.packages.statistics_migration import MigrationTask

    await seed(recorder_mock)
    preview = await migrate(hass, statistics_entry, hass_admin_user)
    before = await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
    gate = QueueGate(asyncio.Event(), threading.Event())
    recorder_mock.queue_task(gate)
    await gate.started.wait()
    queued = asyncio.Event()
    original_queue = recorder_mock.queue_task

    def enqueue(task: RecorderTask) -> None:
        original_queue(task)
        if isinstance(task, MigrationTask):
            queued.set()

    monkeypatch.setattr(recorder_mock, "queue_task", enqueue)
    pending = asyncio.create_task(
        migrate(hass, statistics_entry, hass_admin_user, preview["digest"])
    )
    try:
        await queued.wait()
        if change == "source":
            hass.states.async_set("sensor.shipments_active", "6")
        elif change == "rename":
            er.async_get(hass).async_update_entity(
                "sensor.packages_active", new_entity_id="sensor.renamed_count"
            )
        elif change == "unload":
            assert await hass.config_entries.async_unload(statistics_entry.entry_id)
        elif change == "second_entry":
            await hass.config_entries.async_add(
                MockConfigEntry(domain=DOMAIN, source=SOURCE_IGNORE)
            )
        elif change == "options":
            hass.config_entries.async_update_entry(
                statistics_entry, options={"fixture": True}
            )
        else:
            pending.cancel()
        await hass.async_block_till_done()
    finally:
        gate.release.set()
    with pytest.raises(
        asyncio.CancelledError if change == "cancel" else HomeAssistantError
    ):
        await pending
    await hass.async_add_executor_job(recorder_mock.block_till_done)
    if change == "rename":
        for rows in before.values():
            rows["sensor.renamed_count"] = rows.pop("sensor.packages_active")
    assert (
        await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
        == before
    )


async def test_failure_between_table_updates_rolls_back_then_retries(
    hass: HomeAssistant,
    recorder_mock: Recorder,
    statistics_entry: MockConfigEntry,
    hass_admin_user: MockUser,
) -> None:
    from sqlalchemy.sql.dml import Update

    await seed(recorder_mock)
    preview = await migrate(hass, statistics_entry, hass_admin_user)
    before = await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
    engine = recorder_mock.engine
    assert engine is not None
    injected = False

    def fail_second_table(conn, cursor, statement, parameters, context, executemany):
        nonlocal injected
        compiled = context.compiled
        if (
            compiled is not None
            and isinstance(compiled.statement, Update)
            and compiled.statement.table.name == "statistics_short_term"
        ):
            injected = True
            raise RuntimeError("Injected failure after the hourly move")

    sql_event.listen(engine, "before_cursor_execute", fail_second_table)
    try:
        with pytest.raises(HomeAssistantError, match="migration failed"):
            await migrate(hass, statistics_entry, hass_admin_user, preview["digest"])
    finally:
        sql_event.remove(engine, "before_cursor_execute", fail_second_table)
    assert injected
    assert (
        await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
        == before
    )
    result = await migrate(hass, statistics_entry, hass_admin_user, preview["digest"])
    assert result["status"] == "migrated"


async def test_source_reappearance_during_transaction_rolls_back_both_tables(
    hass: HomeAssistant,
    recorder_mock: Recorder,
    statistics_entry: MockConfigEntry,
    hass_admin_user: MockUser,
) -> None:
    from sqlalchemy.sql.dml import Update

    await seed(recorder_mock)
    preview = await migrate(hass, statistics_entry, hass_admin_user)
    before = await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
    engine = recorder_mock.engine
    assert engine is not None
    changed = asyncio.Event()
    release = threading.Event()

    def pause_after_hourly(conn, cursor, statement, parameters, context, executemany):
        compiled = context.compiled
        if (
            compiled is not None
            and isinstance(compiled.statement, Update)
            and compiled.statement.table.name == "statistics"
        ):
            hass.loop.call_soon_threadsafe(changed.set)
            release.wait(10)

    sql_event.listen(engine, "after_cursor_execute", pause_after_hourly)
    pending = asyncio.create_task(
        migrate(hass, statistics_entry, hass_admin_user, preview["digest"])
    )
    try:
        await changed.wait()
        hass.states.async_set("sensor.shipments_active", "12")
        await hass.async_block_till_done()
    finally:
        release.set()
    try:
        with pytest.raises(HomeAssistantError, match="ownership changed"):
            await pending
    finally:
        sql_event.remove(engine, "after_cursor_execute", pause_after_hourly)
    assert (
        await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
        == before
    )


async def test_empty_legacy_metadata_moves_without_inventing_observations(
    hass: HomeAssistant,
    recorder_mock: Recorder,
    statistics_entry: MockConfigEntry,
    hass_admin_user: MockUser,
) -> None:
    for key in KEYS:
        for prefix in ("shipments", "packages"):
            recorder_mock.async_import_statistics(
                metadata(f"sensor.{prefix}_{key}"), [], Statistics
            )
    await hass.async_add_executor_job(recorder_mock.block_till_done)
    preview = await migrate(hass, statistics_entry, hass_admin_user)
    assert all(
        counts["source"] == counts["destination"] == 0
        for counts in preview["tables"].values()
    )
    assert (await migrate(hass, statistics_entry, hass_admin_user, preview["digest"]))[
        "status"
    ] == "migrated"
    assert await recorder_mock.async_add_executor_job(database_rows, recorder_mock) == {
        "statistics": {},
        "statistics_short_term": {},
    }
    assert set(
        await recorder_mock.async_add_executor_job(database_metadata, recorder_mock)
    ) == {f"sensor.packages_{key}" for key in KEYS}


async def test_review_limit_never_silently_truncates_history(
    hass: HomeAssistant,
    recorder_mock: Recorder,
    statistics_entry: MockConfigEntry,
    hass_admin_user: MockUser,
    monkeypatch,
) -> None:
    import custom_components.packages.statistics_migration as migration

    await seed(recorder_mock)
    before = await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
    monkeypatch.setattr(migration, "_MAX_ROWS", 10)
    with pytest.raises(HomeAssistantError, match="review limit"):
        await migrate(hass, statistics_entry, hass_admin_user)
    assert (
        await recorder_mock.async_add_executor_job(database_rows, recorder_mock)
        == before
    )


async def test_actual_queries_use_indexes_with_representative_recorder_data(
    hass: HomeAssistant,
    recorder_mock: Recorder,
    statistics_entry: MockConfigEntry,
    hass_admin_user: MockUser,
    record_property,
) -> None:
    await seed(recorder_mock)
    for index in range(1000):
        for table in (Statistics, StatisticsShortTerm):
            recorder_mock.async_import_statistics(
                metadata(f"sensor.unrelated_{index}"),
                [
                    observation(START + step * table.duration, index)
                    for step in range(5)
                ],
                table,
            )
    # Match the live legacy series' scale without using household observations.
    for key in KEYS:
        recorder_mock.async_import_statistics(
            metadata(f"sensor.shipments_{key}"),
            [
                observation(START - step * StatisticsShortTerm.duration, step)
                for step in range(1, 2900)
            ],
            StatisticsShortTerm,
        )
    await hass.async_add_executor_job(recorder_mock.block_till_done)
    engine = recorder_mock.engine
    assert engine is not None
    plans = []

    def explain(conn, cursor, statement, parameters, context, executemany):
        if context.compiled is None or executemany:
            return
        compiled = context.compiled.statement
        froms = (
            compiled.get_final_froms()
            if hasattr(compiled, "get_final_froms")
            else [compiled.table]
        )
        names = {table.name for table in froms}
        if not names <= {"statistics", "statistics_short_term", "statistics_meta"}:
            return
        cursor.execute("EXPLAIN QUERY PLAN " + statement, parameters)
        details = [row[3] for row in cursor.fetchall()]
        plans.append({"sql": statement, "parameters": parameters, "plan": details})

    sql_event.listen(engine, "before_cursor_execute", explain)
    try:
        preview = await migrate(hass, statistics_entry, hass_admin_user)
        result = await migrate(
            hass, statistics_entry, hass_admin_user, preview["digest"]
        )
    finally:
        sql_event.remove(engine, "before_cursor_execute", explain)
    assert result["status"] == "migrated"
    assert preview["tables"]["statistics_short_term"]["source"] > 40_000
    assert plans
    assert all(any("SEARCH" in detail for detail in plan["plan"]) for plan in plans)
    record_property("statistics_migration_sql_plans", json.dumps(plans))


async def test_hard_process_crash_is_atomic_and_restart_retry_is_safe(
    hass: HomeAssistant,
    recorder_mock: Recorder,
    statistics_entry: MockConfigEntry,
    hass_admin_user: MockUser,
    tmp_path: Path,
) -> None:
    """Kill the actual Recorder process mid-transaction and after commit."""
    from sqlalchemy.sql.dml import Delete, Update

    stage = os.environ.get("PACKAGES_STATISTICS_CRASH_STAGE")
    if stage:
        capture = Path(os.environ["PACKAGES_STATISTICS_CRASH_CAPTURE"])
        if stage.startswith("recover_"):
            saved = json.loads(await hass.async_add_executor_job(capture.read_text))
            actual = await recorder_mock.async_add_executor_job(
                database_rows, recorder_mock
            )
            if stage == "recover_before":
                assert json.loads(json.dumps(actual)) == saved["rows"]
                preview = await migrate(hass, statistics_entry, hass_admin_user)
                assert (
                    await migrate(
                        hass, statistics_entry, hass_admin_user, preview["digest"]
                    )
                )["status"] == "migrated"
                actual = await recorder_mock.async_add_executor_job(
                    database_rows, recorder_mock
                )
            for table, rows in saved["rows"].items():
                for key in KEYS:
                    old, current = f"sensor.shipments_{key}", f"sensor.packages_{key}"
                    assert old not in actual[table]
                    assert json.loads(json.dumps(actual[table][current])) == [
                        rows[old][0],
                        *rows[current],
                    ]
            assert (
                await migrate(hass, statistics_entry, hass_admin_user, saved["digest"])
            )["status"] == "already_complete"
            return
        await seed(recorder_mock)
        preview = await migrate(hass, statistics_entry, hass_admin_user)
        before = await recorder_mock.async_add_executor_job(
            database_rows, recorder_mock
        )
        await hass.async_add_executor_job(
            capture.write_text,
            json.dumps({"rows": before, "digest": preview["digest"]}),
        )
        engine = recorder_mock.engine
        assert engine is not None
        retiring = False

        def crash_before(conn, cursor, statement, parameters, context, executemany):
            nonlocal retiring
            compiled = context.compiled
            if compiled is None:
                return
            if (
                stage == "before"
                and isinstance(compiled.statement, Update)
                and compiled.statement.table.name == "statistics_short_term"
            ):
                os._exit(73)
            if (
                isinstance(compiled.statement, Delete)
                and compiled.statement.table.name == "statistics_meta"
            ):
                retiring = True

        def crash_after(session):
            if retiring:
                os._exit(74)

        sql_event.listen(engine, "before_cursor_execute", crash_before)
        sql_event.listen(Session, "after_commit", crash_after)
        await migrate(hass, statistics_entry, hass_admin_user, preview["digest"])
        pytest.fail("Recorder did not reach the requested crash boundary")

    for boundary, exit_code in (("before", 73), ("after", 74)):
        database = tmp_path / f"{boundary}.sqlite"
        capture = tmp_path / f"{boundary}.json"
        for attempt, expected_code in (
            (boundary, exit_code),
            (f"recover_{boundary}", 0),
        ):
            environment = {
                **os.environ,
                "PACKAGES_STATISTICS_CRASH_STAGE": attempt,
                "PACKAGES_STATISTICS_CRASH_CAPTURE": str(capture),
            }
            process = await asyncio.create_subprocess_exec(
                sys.executable,
                "-m",
                "pytest",
                "-x",
                "-q",
                "tests_ha/test_statistics_migration.py::"
                "test_hard_process_crash_is_atomic_and_restart_retry_is_safe",
                "--dburl",
                "sqlite:///" + str(database),
                env=environment,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT,
            )
            try:
                output, _ = await asyncio.wait_for(process.communicate(), timeout=60)
            except TimeoutError:
                process.kill()
                await process.wait()
                raise
            assert process.returncode == expected_code, output.decode()[-6000:]
