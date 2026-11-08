"""Amazon failures use native, entry-owned Repairs without account notifications."""

import asyncio
import json
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from datetime import timedelta

import pytest
from homeassistant.components import persistent_notification as pn
from homeassistant.config_entries import ConfigEntryDisabler
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers import issue_registry as ir
from homeassistant.setup import async_setup_component
from homeassistant.util import dt as dt_util
from pytest_homeassistant.common import MockConfigEntry, async_fire_time_changed

from custom_components.shipments.const import (
    CONF_ADDON_SLUG,
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
)

from .conftest import Backend, get_state
from .test_discovery_health import discovery_report


@dataclass
class Notifications:
    current: dict[str, pn.Notification] = field(default_factory=dict)

    @callback
    def receive(
        self, kind: pn.UpdateType, notifications: dict[str, pn.Notification]
    ) -> None:
        if kind is pn.UpdateType.REMOVED:
            for key in notifications:
                self.current.pop(key, None)
        else:
            self.current.update(notifications)


@pytest.fixture
async def notifications(hass: HomeAssistant) -> AsyncIterator[Notifications]:
    assert await async_setup_component(hass, "persistent_notification", {})
    observed = Notifications()
    unsubscribe = pn.async_register_callback(hass, observed.receive)
    yield observed
    unsubscribe()


def report(
    backend: Backend, discovery: object = None, *, amazon: object = None
) -> None:
    backend.responses[("GET", "/v1/system")] = (
        200,
        json.dumps(
            {
                "version": backend.version,
                "amazon": amazon,
                "amazon_discovery": discovery,
            }
        ).encode(),
        {},
    )


def account(state: str, *, challenge_required: bool = False) -> dict[str, object]:
    return {
        "state": state,
        "imports_enabled": True,
        "challenge_required": challenge_required,
        "unexpected_secret": "PRIVATE-TOKEN-AND-ACCOUNT",
    }


def issue(hass: HomeAssistant, entry: MockConfigEntry) -> ir.IssueEntry:
    current = ir.async_get(hass).async_get_issue(
        DOMAIN, f"amazon_discovery_{entry.entry_id}"
    )
    assert current is not None, "Confirmed Amazon failure has no native Repair"
    return current


@pytest.mark.parametrize("state", ["partial", "stale", "failed", "running"])
async def test_confirmed_discovery_failure_creates_one_owned_native_repair(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    notifications: Notifications,
    state: str,
) -> None:
    report(
        backend,
        discovery_report(
            state=state,
            problem=True,
            deferred_parcels=2,
            last_error_code="timeout",
        ),
    )
    await entry.runtime_data.async_refresh()
    first = issue(hass, entry)
    assert first.active and first.is_persistent
    assert first.config_entry_id == entry.entry_id
    assert first.translation_key == "amazon_discovery"
    assert first.translation_placeholders is not None
    assert first.translation_placeholders["state"] == state
    assert first.translation_placeholders["error_code"] == "timeout"
    assert first.translation_placeholders["deferred_parcels"] == "2"
    assert first.translation_placeholders["logs_url"] == "/config/logs"
    assert first.learn_more_url == "/config/integrations/integration/shipments"
    assert "PRIVATE-" not in json.dumps(first.to_json(), default=str)
    assert notifications.current == {}
    await entry.runtime_data.async_refresh()
    assert issue(hass, entry).created == first.created
    assert get_state(hass, "sensor.shipments_active").state == "4"


@pytest.mark.parametrize(
    ("amazon", "key"),
    [
        (account("auth_required"), "amazon_discovery_auth_required"),
        (
            account("connected", challenge_required=True),
            "amazon_discovery_challenge_required",
        ),
    ],
)
async def test_account_action_migrates_only_its_legacy_notice_to_a_repair(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    notifications: Notifications,
    amazon: object,
    key: str,
) -> None:
    hass.config_entries.async_update_entry(
        entry, data={**entry.data, CONF_ADDON_SLUG: "fixture_shipments"}
    )
    old_notice = f"shipments_{entry.entry_id}_amazon_auth_required"
    pn.async_create(hass, "Old Amazon notice", notification_id=old_notice)
    pn.async_create(hass, "Delivery automation notice", notification_id="delivery")
    report(backend, amazon=amazon)
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    current = issue(hass, entry)
    assert current.translation_key == key
    assert current.translation_placeholders is not None
    assert current.translation_placeholders["settings_url"] == (
        "/shipments/shipments/amazon%3Dconnect"
    )
    assert current.translation_placeholders["logs_url"] == (
        "/config/app/fixture_shipments/logs"
    )
    assert "PRIVATE-" not in json.dumps(current.to_json(), default=str)
    assert list(notifications.current) == ["delivery"]
    assert not hass.config_entries.flow.async_progress_by_handler(DOMAIN)


@pytest.mark.parametrize(
    "unconfirmed",
    [
        None,
        {},
        discovery_report(checked_at=0),
        discovery_report(checked_at=int(dt_util.utcnow().timestamp()) + 3600),
        discovery_report(state="pending", problem=True),
        discovery_report(state="running", problem=True, has_more=True),
        discovery_report(state="partial", problem=True, has_more=True),
        discovery_report(state="retrying", problem=True, last_outcome="failed"),
    ],
)
async def test_unconfirmed_recovery_keeps_the_existing_repair(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    unconfirmed: object,
) -> None:
    report(backend, discovery_report(state="partial", problem=True))
    await entry.runtime_data.async_refresh()
    first = issue(hass, entry)
    report(backend, unconfirmed, amazon=account("connected"))
    await entry.runtime_data.async_refresh()
    assert issue(hass, entry).created == first.created


@pytest.mark.parametrize("state", ["complete", "disabled", "running", "retrying"])
async def test_fresh_confirmed_recovery_clears_only_the_owned_repair(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    state: str,
) -> None:
    report(backend, discovery_report(state="partial", problem=True))
    await entry.runtime_data.async_refresh()
    issue(hass, entry)
    ir.async_create_issue(
        hass,
        DOMAIN,
        "event_history_reset",
        is_fixable=False,
        severity=ir.IssueSeverity.WARNING,
        translation_key="event_history_reset",
    )
    report(backend, discovery_report(state=state))
    await entry.runtime_data.async_refresh()
    registry = ir.async_get(hass)
    assert (
        registry.async_get_issue(DOMAIN, f"amazon_discovery_{entry.entry_id}") is None
    )
    assert registry.async_get_issue(DOMAIN, "event_history_reset") is not None


async def test_account_reconnect_requires_a_current_discovery_report(
    hass: HomeAssistant, entry: MockConfigEntry, backend: Backend
) -> None:
    report(backend, amazon=account("auth_required"))
    await entry.runtime_data.async_refresh()
    issue(hass, entry)
    report(backend, amazon=account("connected"))
    await entry.runtime_data.async_refresh()
    issue(hass, entry)
    report(
        backend,
        discovery_report(state="disabled"),
        amazon=account("disconnected"),
    )
    await entry.runtime_data.async_refresh()
    assert (
        ir.async_get(hass).async_get_issue(DOMAIN, f"amazon_discovery_{entry.entry_id}")
        is None
    )


@pytest.mark.parametrize(
    "invalid",
    [
        {"state": [], "imports_enabled": True},
        {"state": "unknown", "imports_enabled": True},
        {"state": "connected", "imports_enabled": 1},
        {"state": "connected", "imports_enabled": True, "challenge_required": None},
    ],
)
async def test_malformed_account_state_does_not_clear_a_known_problem(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    invalid: object,
) -> None:
    report(backend, amazon=account("auth_required"))
    await entry.runtime_data.async_refresh()
    first = issue(hass, entry)
    report(backend, amazon=invalid)
    await entry.runtime_data.async_refresh()
    assert entry.runtime_data.last_update_success
    assert issue(hass, entry).created == first.created


async def test_outage_reload_and_config_entry_removal_preserve_native_ownership(
    hass: HomeAssistant, entry: MockConfigEntry, backend: Backend
) -> None:
    report(backend, discovery_report(state="stale", problem=True))
    await entry.runtime_data.async_refresh()
    first = issue(hass, entry)
    backend.responses[("GET", "/v1/system")] = (503, b"PRIVATE-ERROR", {})
    await entry.runtime_data.async_refresh()
    assert issue(hass, entry).created == first.created
    assert not await hass.config_entries.async_reload(entry.entry_id)
    assert issue(hass, entry).active
    assert issue(hass, entry).created == first.created
    await hass.config_entries.async_remove(entry.entry_id)
    assert (
        ir.async_get(hass).async_get_issue(DOMAIN, f"amazon_discovery_{entry.entry_id}")
        is None
    )


async def test_native_ignore_and_disable_survive_reload_until_real_recovery(
    hass: HomeAssistant, entry: MockConfigEntry, backend: Backend
) -> None:
    report(backend, discovery_report(state="partial", problem=True))
    await entry.runtime_data.async_refresh()
    registry = ir.async_get(hass)
    first = issue(hass, entry)
    registry.async_ignore(DOMAIN, first.issue_id, True)
    assert await hass.config_entries.async_reload(entry.entry_id)
    assert issue(hass, entry).dismissed_version is not None
    assert issue(hass, entry).created == first.created
    await hass.config_entries.async_set_disabled_by(
        entry.entry_id, ConfigEntryDisabler.USER
    )
    assert registry.async_is_hidden(issue(hass, entry))
    await hass.config_entries.async_set_disabled_by(entry.entry_id, None)
    report(backend, discovery_report())
    await entry.runtime_data.async_refresh()
    assert registry.async_get_issue(DOMAIN, first.issue_id) is None


async def test_late_problem_response_cannot_create_a_repair_after_unload(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    notifications: Notifications,
) -> None:
    report(backend, amazon=account("auth_required"))
    backend.system_read_started.clear()
    release = backend.system_read_release = asyncio.Event()
    refresh = hass.async_create_task(entry.runtime_data.async_refresh())
    try:
        async with asyncio.timeout(5):
            await backend.system_read_started.wait()
        assert await hass.config_entries.async_unload(entry.entry_id)
        release.set()
        await refresh
        assert (
            ir.async_get(hass).async_get_issue(
                DOMAIN, f"amazon_discovery_{entry.entry_id}"
            )
            is None
        )
        assert notifications.current == {}
    finally:
        release.set()
        await refresh


async def test_unknown_error_text_is_never_a_repair_or_health_attribute(
    hass: HomeAssistant, entry: MockConfigEntry, backend: Backend
) -> None:
    report(
        backend,
        discovery_report(
            state="failed", problem=True, last_error_code="PRIVATE-ORDER-AND-TOKEN"
        ),
    )
    await entry.runtime_data.async_refresh()
    current = issue(hass, entry)
    assert current.translation_placeholders is not None
    assert current.translation_placeholders["error_code"] == "unspecified"
    assert "PRIVATE-" not in json.dumps(current.to_json(), default=str)
    assert "PRIVATE-" not in json.dumps(
        dict(get_state(hass, "sensor.shipments_amazon_discovery").attributes)
    )


@pytest.mark.parametrize(
    ("kind", "reason"),
    [
        ("missing", "health_report_missing"),
        ("invalid", "health_report_invalid"),
        ("expired", "health_report_expired"),
        ("future", "health_report_future"),
        ("transport", "backend_unavailable"),
    ],
)
async def test_unavailable_source_health_creates_a_native_repair_after_healthy_report(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    kind: str,
    reason: str,
) -> None:
    report(backend, discovery_report())
    await entry.runtime_data.async_refresh()
    invalid: object = None
    if kind == "invalid":
        invalid = {}
    elif kind == "expired":
        invalid = discovery_report(checked_at=0)
    elif kind == "future":
        invalid = discovery_report(checked_at=int(dt_util.utcnow().timestamp()) + 120)
    if kind == "transport":
        backend.responses[("GET", "/v1/system")] = (503, b"PRIVATE-ERROR", {})
    else:
        report(backend, invalid)
    await entry.runtime_data.async_refresh()
    current = issue(hass, entry)
    assert current.translation_key == "amazon_discovery_unavailable"
    assert current.translation_placeholders is not None
    assert current.translation_placeholders["error_code"] == reason
    assert get_state(hass, "sensor.shipments_amazon_discovery").state == "unavailable"
    report(backend, discovery_report())
    await entry.runtime_data.async_refresh()
    assert ir.async_get(hass).async_get_issue(DOMAIN, current.issue_id) is None


async def test_source_health_expiry_creates_a_native_repair_when_polling_halts(
    hass: HomeAssistant, entry: MockConfigEntry, backend: Backend, freezer
) -> None:
    report(backend, discovery_report())
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done(wait_background_tasks=True)
    await entry.runtime_data.async_shutdown()
    backend.requests.clear()
    freezer.tick(timedelta(seconds=181))
    async_fire_time_changed(hass, dt_util.utcnow())
    await hass.async_block_till_done(wait_background_tasks=True)
    current = issue(hass, entry)
    assert current.translation_key == "amazon_discovery_unavailable"
    assert current.translation_placeholders is not None
    assert current.translation_placeholders["error_code"] == "health_report_expired"
    assert backend.requests == []


async def test_source_monitor_polls_with_every_entity_disabled(
    hass: HomeAssistant, entry: MockConfigEntry, backend: Backend, freezer
) -> None:
    report(backend, discovery_report())
    await entry.runtime_data.async_refresh()
    registry = er.async_get(hass)
    entities = er.async_entries_for_config_entry(registry, entry.entry_id)
    assert entities
    for entity in entities:
        registry.async_update_entity(
            entity.entity_id, disabled_by=er.RegistryEntryDisabler.USER
        )
    assert await hass.config_entries.async_reload(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert all(hass.states.get(entity.entity_id) is None for entity in entities)
    backend.requests.clear()
    report(backend, None)
    freezer.tick(timedelta(seconds=61))
    async_fire_time_changed(hass, dt_util.utcnow())
    await hass.async_block_till_done(wait_background_tasks=True)
    assert any(request.path == "/v1/system" for request in backend.requests)
    assert issue(hass, entry).translation_key == "amazon_discovery_unavailable"


async def test_source_monitor_has_bounded_initial_grace_and_preserves_confirmed_faults(
    hass: HomeAssistant, entry: MockConfigEntry, backend: Backend, freezer
) -> None:
    report(backend, None)
    await entry.runtime_data.async_refresh()
    registry = ir.async_get(hass)
    issue_id = f"amazon_discovery_{entry.entry_id}"
    freezer.tick(timedelta(seconds=179))
    async_fire_time_changed(hass, dt_util.utcnow())
    await hass.async_block_till_done(wait_background_tasks=True)
    assert registry.async_get_issue(DOMAIN, issue_id) is None
    freezer.tick(timedelta(seconds=31))
    async_fire_time_changed(hass, dt_util.utcnow())
    await hass.async_block_till_done(wait_background_tasks=True)
    assert issue(hass, entry).translation_key == "amazon_discovery_unavailable"
    report(backend, discovery_report(state="partial", problem=True))
    await entry.runtime_data.async_refresh()
    known = issue(hass, entry)
    assert known.translation_key == "amazon_discovery"
    backend.responses[("GET", "/v1/system")] = (503, b"PRIVATE-ERROR", {})
    await entry.runtime_data.async_refresh()
    assert issue(hass, entry) == known


async def test_source_monitor_defers_recovery_until_the_poll_completes(
    hass: HomeAssistant, entry: MockConfigEntry, backend: Backend
) -> None:
    report(backend, discovery_report(state="partial", problem=True))
    await entry.runtime_data.async_refresh()
    known = issue(hass, entry)
    report(backend, discovery_report())
    backend.responses[("GET", "/v1/summary")] = (503, b"PRIVATE-ERROR", {})
    await entry.runtime_data.async_refresh()
    assert issue(hass, entry) == known
    backend.responses.pop(("GET", "/v1/summary"))
    await entry.runtime_data.async_refresh()
    assert ir.async_get(hass).async_get_issue(DOMAIN, known.issue_id) is None


@pytest.mark.parametrize("status", [401, 403])
async def test_initial_backend_auth_uses_only_core_reauth(
    hass: HomeAssistant,
    backend: Backend,
    notifications: Notifications,
    status: int,
) -> None:
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={CONF_BACKEND_URL: backend.url, CONF_TOKEN: "test-owner-token"},
    )
    entry.add_to_hass(hass)
    old_notice = f"shipments_{entry.entry_id}_amazon_auth_required"
    pn.async_create(hass, "Old Amazon notice", notification_id=old_notice)
    backend.responses[("GET", "/v1/system")] = (status, b"PRIVATE-ERROR", {})
    assert not await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    registry = ir.async_get(hass)
    assert (
        registry.async_get_issue(
            "homeassistant", f"config_entry_reauth_{DOMAIN}_{entry.entry_id}"
        )
        is not None
    )
    assert (
        registry.async_get_issue(DOMAIN, f"amazon_discovery_{entry.entry_id}") is None
    )
    assert old_notice not in notifications.current


@pytest.mark.parametrize("prior_gap", [False, True])
@pytest.mark.parametrize("path", ["/v1/system", "/v1/summary"])
async def test_backend_reauth_replaces_only_the_availability_repair(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    notifications: Notifications,
    freezer,
    prior_gap: bool,
    path: str,
) -> None:
    report(backend, discovery_report())
    await entry.runtime_data.async_refresh()
    if prior_gap:
        backend.responses[("GET", "/v1/system")] = (503, b"PRIVATE-ERROR", {})
        await entry.runtime_data.async_refresh()
        assert issue(hass, entry).translation_key == "amazon_discovery_unavailable"
        report(backend, discovery_report())
    old_notice = f"shipments_{entry.entry_id}_amazon_auth_required"
    pn.async_create(hass, "Old Amazon notice", notification_id=old_notice)
    pn.async_create(hass, "Delivery automation notice", notification_id="delivery")
    backend.responses[("GET", path)] = (401, b"PRIVATE-ERROR", {})
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done(wait_background_tasks=True)
    registry = ir.async_get(hass)
    core_id = f"config_entry_reauth_{DOMAIN}_{entry.entry_id}"
    core = registry.async_get_issue("homeassistant", core_id)
    assert core is not None and core.active
    issue_id = f"amazon_discovery_{entry.entry_id}"
    assert registry.async_get_issue(DOMAIN, issue_id) is None
    assert list(notifications.current) == ["delivery"]
    backend.responses[("GET", path)] = (503, b"PRIVATE-ERROR", {})
    await entry.runtime_data.async_refresh()
    freezer.tick(timedelta(seconds=211))
    async_fire_time_changed(hass, dt_util.utcnow())
    await hass.async_block_till_done(wait_background_tasks=True)
    assert registry.async_get_issue("homeassistant", core_id) == core
    assert registry.async_get_issue(DOMAIN, issue_id) is None


async def test_confirmed_account_auth_uses_its_own_repair_during_backend_reauth(
    hass: HomeAssistant, entry: MockConfigEntry, backend: Backend
) -> None:
    report(backend, discovery_report())
    await entry.runtime_data.async_refresh()
    backend.responses[("GET", "/v1/system")] = (401, b"PRIVATE-ERROR", {})
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done(wait_background_tasks=True)
    core_id = f"config_entry_reauth_{DOMAIN}_{entry.entry_id}"
    assert ir.async_get(hass).async_get_issue("homeassistant", core_id) is not None
    report(backend, amazon=account("auth_required"))
    await entry.runtime_data.async_refresh()
    assert issue(hass, entry).translation_key == "amazon_discovery_auth_required"


@pytest.mark.parametrize(
    ("amazon", "discovery", "key"),
    [
        (account("auth_required"), None, "amazon_discovery_auth_required"),
        (None, discovery_report(state="partial", problem=True), "amazon_discovery"),
    ],
)
async def test_backend_reauth_preserves_confirmed_independent_amazon_fault(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    freezer,
    amazon: object,
    discovery: object,
    key: str,
) -> None:
    report(backend, discovery, amazon=amazon)
    await entry.runtime_data.async_refresh()
    known = issue(hass, entry)
    assert known.translation_key == key
    backend.responses[("GET", "/v1/system")] = (403, b"PRIVATE-ERROR", {})
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done(wait_background_tasks=True)
    assert (
        ir.async_get(hass).async_get_issue(
            "homeassistant", f"config_entry_reauth_{DOMAIN}_{entry.entry_id}"
        )
        is not None
    )
    freezer.tick(timedelta(seconds=211))
    async_fire_time_changed(hass, dt_util.utcnow())
    await hass.async_block_till_done(wait_background_tasks=True)
    assert issue(hass, entry) == known
