"""Installation evidence follows actual HA setup, authentication, and backup data."""

import asyncio
import json
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from unittest.mock import AsyncMock

import pytest
from aiohasupervisor import SupervisorClient
from aiohttp import web
from homeassistant.auth.models import User
from homeassistant.config_entries import ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryAuthFailed, ConfigEntryNotReady
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.loader import async_get_integration
from homeassistant.util import dt as dt_util
from pytest_homeassistant_custom_component.common import (
    MockConfigEntry,
    async_fire_time_changed,
)

from custom_components.shipments.const import (
    CONF_ADDON_SLUG,
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
)

from .conftest import Backend

APP_SLUG = "testrepo_shipments"
PRESENCE_PATH = "/v1/integration/presence"


@dataclass
class BackupInventory:
    """Serve the actual Supervisor wire format to its native Python client."""

    backups: list[dict[str, object]] = field(default_factory=list)
    status: int = 200
    delay: float = 0
    requests: int = 0
    started: asyncio.Event = field(default_factory=asyncio.Event)

    async def handle(self, request: web.Request) -> web.Response:
        assert request.headers["Authorization"] == "Bearer private-supervisor-token"
        self.requests += 1
        self.started.set()
        if self.delay:
            await asyncio.sleep(self.delay)
        return web.json_response(
            {"result": "ok", "data": {"backups": self.backups, "days_until_stale": 7}},
            status=self.status,
        )


@pytest.fixture
async def inventory(hass: HomeAssistant, aiohttp_server, socket_enabled, monkeypatch):
    state = BackupInventory()
    app = web.Application()
    app.router.add_get("/backups/info", state.handle)
    server = await aiohttp_server(app)
    client = SupervisorClient(
        str(server.make_url("")).rstrip("/"),
        "private-supervisor-token",
        session=async_get_clientsession(hass),
    )
    monkeypatch.setattr(
        "custom_components.shipments.coordinator.is_hassio", lambda hass: True
    )
    monkeypatch.setattr(
        "homeassistant.components.hassio.get_supervisor_client",
        lambda hass: client,
    )
    return state


def backup(slug: str, date: datetime, *, core: bool) -> dict[str, object]:
    return {
        "slug": "private-backup-id",
        "name": "Private household backup name",
        "date": date.isoformat(),
        "type": "partial",
        "location_attributes": {},
        "compressed": True,
        "content": {"homeassistant": core, "addons": [slug], "folders": []},
    }


async def setup_paired(hass: HomeAssistant, backend: Backend) -> MockConfigEntry:
    backend.responses[("POST", PRESENCE_PATH)] = (204, b"", {})
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: backend.url,
            CONF_TOKEN: "private-integration-token",
            CONF_ADDON_SLUG: APP_SLUG,
        },
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    return entry


def check_ins(backend: Backend):
    return [request for request in backend.requests if request.path == PRESENCE_PATH]


@pytest.mark.parametrize("status", ["complete", "app_only", "missing", "outdated"])
async def test_loaded_presence_contains_only_matching_backup_coverage(
    hass: HomeAssistant,
    backend: Backend,
    inventory: BackupInventory,
    freezer,
    status: str,
) -> None:
    now = datetime(2026, 10, 15, 1, 0, tzinfo=UTC)
    freezer.move_to(now - timedelta(days=3))
    entry = await setup_paired(hass, backend)
    freezer.move_to(now)
    inventory.backups.append(backup(f"{APP_SLUG}_other", now, core=True))
    if status in {"complete", "app_only"}:
        inventory.backups.append(
            backup(APP_SLUG, now - timedelta(days=2), core=status == "complete")
        )
        if status == "complete":
            inventory.backups.append(
                backup(APP_SLUG, now - timedelta(days=1), core=False)
            )
    elif status == "outdated":
        inventory.backups.append(backup(APP_SLUG, now - timedelta(days=4), core=True))
    await entry.runtime_data.async_refresh()
    assert entry.state is ConfigEntryState.LOADED
    assert [
        (request.path, request.query)
        for request in backend.requests
        if request.path in {"/v1/summary", PRESENCE_PATH}
    ][:7] == [
        ("/v1/summary", {}),
        ("/v1/summary", {"direction": "inbound"}),
        ("/v1/summary", {"direction": "outbound"}),
        ("/v1/summary", {}),
        ("/v1/summary", {"direction": "inbound"}),
        ("/v1/summary", {"direction": "outbound"}),
        (PRESENCE_PATH, {}),
    ]
    reports = check_ins(backend)
    assert len(reports) == 2
    report = json.loads(reports[-1].body)
    integration = await async_get_integration(hass, DOMAIN)
    assert report == {
        "version": str(integration.version),
        "administrator_ids": [
            user.id
            for user in await hass.auth.async_get_users()
            if user.is_active and user.is_admin
        ],
        "backup": {
            "status": status,
            "last_backup": int(
                (now - timedelta(days=4 if status == "outdated" else 2)).timestamp()
            )
            if status != "missing"
            else None,
        },
    }
    assert reports[0].headers["Authorization"] == "Bearer private-integration-token"
    assert reports[0].headers["X-Shipments-Actor"] == "ha:integration"
    assert "Private household" not in reports[0].body.decode()
    assert "private-backup-id" not in reports[0].body.decode()
    assert APP_SLUG not in reports[0].body.decode()
    assert entry.runtime_data.last_update_success


async def test_periodic_poll_reports_but_unloaded_entry_stops(
    hass: HomeAssistant, backend: Backend, inventory: BackupInventory
) -> None:
    entry = await setup_paired(hass, backend)
    coordinator = entry.runtime_data
    assert len(check_ins(backend)) == 1
    async_fire_time_changed(hass, dt_util.utcnow() + timedelta(seconds=65))
    await hass.async_block_till_done(wait_background_tasks=True)
    assert len(check_ins(backend)) == 2
    assert await hass.config_entries.async_unload(entry.entry_id)
    await coordinator.async_refresh()
    assert len(check_ins(backend)) == 2
    assert inventory.requests == 2
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert len(check_ins(backend)) == 3


async def test_failed_entity_setup_does_not_claim_loaded(
    hass: HomeAssistant, backend: Backend, monkeypatch
) -> None:
    backend.responses[("POST", PRESENCE_PATH)] = (204, b"", {})
    monkeypatch.setattr(
        hass.config_entries,
        "async_forward_entry_setups",
        AsyncMock(side_effect=ConfigEntryNotReady("Entity setup is not ready")),
    )
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: backend.url,
            CONF_TOKEN: "private-token",
            CONF_ADDON_SLUG: APP_SLUG,
        },
    )
    entry.add_to_hass(hass)
    assert not await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert entry.state is ConfigEntryState.SETUP_RETRY
    assert not check_ins(backend)


@pytest.mark.parametrize("failure_status", [401, 503])
@pytest.mark.parametrize("direction", [None, "inbound", "outbound"])
async def test_failed_summary_never_sends_check_in(
    hass: HomeAssistant,
    backend: Backend,
    inventory: BackupInventory,
    failure_status: int,
    direction: str | None,
) -> None:
    entry = await setup_paired(hass, backend)
    count = len(check_ins(backend))
    backup_calls = inventory.requests
    path = "/v1/summary"
    if direction is not None:
        path += f"?direction={direction}"
    backend.responses[("GET", path)] = (failure_status, b"{}", {})
    await entry.runtime_data.async_refresh()
    assert not entry.runtime_data.last_update_success
    assert len(check_ins(backend)) == count
    assert inventory.requests == backup_calls
    if failure_status == 401:
        assert isinstance(entry.runtime_data.last_exception, ConfigEntryAuthFailed)


@pytest.mark.parametrize("status", [401, 403])
async def test_presence_auth_failure_starts_reauthentication(
    hass: HomeAssistant, backend: Backend, inventory: BackupInventory, status: int
) -> None:
    entry = await setup_paired(hass, backend)
    backend.responses[("POST", PRESENCE_PATH)] = (status, b"{}", {})
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert isinstance(entry.runtime_data.last_exception, ConfigEntryAuthFailed)
    assert not entry.runtime_data.last_update_success
    assert any(
        flow["context"].get("entry_id") == entry.entry_id
        for flow in hass.config_entries.flow.async_progress()
    )


@pytest.mark.parametrize("failure", ["http", "timeout", "not_supervised"])
async def test_unavailable_backups_are_reported_honestly(
    hass: HomeAssistant,
    backend: Backend,
    inventory: BackupInventory,
    monkeypatch,
    failure: str,
) -> None:
    if failure == "http":
        inventory.status = 503
    elif failure == "timeout":
        inventory.delay = 0.03
        monkeypatch.setattr(
            "custom_components.shipments.coordinator._BACKUP_TIMEOUT_SECONDS", 0.01
        )
    else:
        monkeypatch.setattr(
            "custom_components.shipments.coordinator.is_hassio", lambda hass: False
        )
    await setup_paired(hass, backend)
    report = json.loads(check_ins(backend)[0].body)
    assert report["backup"] == {"status": "unavailable", "last_backup": None}


async def test_standalone_does_not_report_app_presence(
    hass: HomeAssistant, backend: Backend, entry, inventory: BackupInventory
) -> None:
    await entry.runtime_data.async_refresh()
    assert not check_ins(backend)
    assert inventory.requests == 0


async def test_unload_during_backup_lookup_cancels_pending_presence(
    hass: HomeAssistant, backend: Backend, inventory: BackupInventory
) -> None:
    entry = await setup_paired(hass, backend)
    inventory.delay = 1
    inventory.started.clear()
    count = len(check_ins(backend))
    entry.async_create_background_task(
        hass, entry.runtime_data.async_refresh(), "test check-in"
    )
    await inventory.started.wait()
    assert await hass.config_entries.async_unload(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert len(check_ins(backend)) == count


async def test_old_full_backup_cannot_promote_new_app_only_backup(
    hass: HomeAssistant, backend: Backend, inventory: BackupInventory, freezer
) -> None:
    paired = datetime(2026, 10, 14, 1, 0, tzinfo=UTC)
    freezer.move_to(paired)
    entry = await setup_paired(hass, backend)
    freezer.move_to(paired + timedelta(hours=2))
    inventory.backups = [
        backup(APP_SLUG, paired - timedelta(hours=1), core=True),
        backup(APP_SLUG, paired + timedelta(hours=1), core=False),
    ]
    await entry.runtime_data.async_refresh()
    assert json.loads(check_ins(backend)[-1].body)["backup"] == {
        "status": "app_only",
        "last_backup": int((paired + timedelta(hours=1)).timestamp()),
    }


async def test_unload_during_admin_lookup_does_not_claim_presence(
    hass: HomeAssistant, backend: Backend, monkeypatch
) -> None:
    entry = await setup_paired(hass, backend)
    started = asyncio.Event()
    release = asyncio.Event()
    original = hass.auth.async_get_users

    async def users() -> list[User]:
        started.set()
        await release.wait()
        return await original()

    monkeypatch.setattr(hass.auth, "async_get_users", users)
    before = len(check_ins(backend))
    entry.async_create_background_task(
        hass, entry.runtime_data.async_refresh(), "test admin lookup"
    )
    async with asyncio.timeout(2):
        await started.wait()
    assert await hass.config_entries.async_unload(entry.entry_id)
    release.set()
    await hass.async_block_till_done(wait_background_tasks=True)
    assert len(check_ins(backend)) == before


@pytest.mark.parametrize("status", [404, 503])
async def test_optional_presence_failure_preserves_summary(
    hass: HomeAssistant, backend: Backend, inventory: BackupInventory, status: int
) -> None:
    entry = await setup_paired(hass, backend)
    backend.responses[("POST", PRESENCE_PATH)] = (status, b"{}", {})
    await entry.runtime_data.async_refresh()
    assert entry.runtime_data.last_update_success
    assert entry.runtime_data.data["active"] == 4
