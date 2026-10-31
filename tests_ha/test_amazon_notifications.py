"""Amazon account expiry uses HA notifications without breaking backend pairing."""

import json
from collections.abc import AsyncIterator
from dataclasses import dataclass, field

import pytest
from homeassistant.components import persistent_notification as pn
from homeassistant.core import HomeAssistant, callback
from homeassistant.setup import async_setup_component
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.shipments.const import CONF_ADDON_SLUG

from .conftest import Backend


@dataclass
class Notifications:
    """Observe real HA notification updates through its public callback."""

    current: dict[str, pn.Notification] = field(default_factory=dict)
    updates: list[pn.UpdateType] = field(default_factory=list)

    @callback
    def receive(
        self, kind: pn.UpdateType, notifications: dict[str, pn.Notification]
    ) -> None:
        self.updates.append(kind)
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


def report(backend: Backend, amazon: object) -> None:
    """Serve an account-health report through the real adapter HTTP path."""
    backend.responses[("GET", "/v1/system")] = (
        200,
        json.dumps({"version": backend.version, "amazon": amazon}).encode(),
        {},
    )


def account(state: str) -> dict[str, object]:
    return {
        "state": state,
        "last_success_at": None,
        "last_error_code": "amazon_auth_required" if state == "auth_required" else None,
        "imports_enabled": True,
        "unexpected_secret": "PRIVATE-TOKEN-AND-ACCOUNT",
    }


async def test_expired_account_with_no_shipments_notifies_once(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    notifications: Notifications,
) -> None:
    for summary in [*backend.summaries.values(), backend.delivery_summary]:
        summary.update(dict.fromkeys(summary, 0))
    backend.shipment_rows = []
    # The app panel exists only when the entry points at a Supervisor app.
    hass.config_entries.async_update_entry(
        entry, data={**entry.data, CONF_ADDON_SLUG: "fixture_shipments"}
    )
    report(backend, account("auth_required"))
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    key = f"shipments_{entry.entry_id}_amazon_auth_required"
    assert list(notifications.current) == [key]
    notice = notifications.current[key]
    assert notice["title"] == "Shipments: reconnect Amazon"
    assert (
        "[Reconnect Amazon](/shipments/shipments/amazon%3Dconnect)" in notice["message"]
    )
    assert "PRIVATE-" not in json.dumps(notice, default=str)
    assert entry.runtime_data.last_update_success
    assert entry.runtime_data.data["active"] == 0
    assert not hass.config_entries.flow.async_progress_by_handler("shipments")
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert notifications.updates == [pn.UpdateType.ADDED]


async def test_standalone_notification_opens_validated_backend_settings(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    notifications: Notifications,
) -> None:
    report(backend, account("auth_required"))
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    notice = next(iter(notifications.current.values()))
    assert f"[Reconnect Amazon]({backend.url}/#amazon=connect)" in notice["message"]


@pytest.mark.parametrize(
    "amazon",
    [
        None,
        {},
        [],
        {"state": [], "imports_enabled": True},
        {"state": "unknown", "imports_enabled": True},
        {"state": "connected"},
        {"state": "connected", "imports_enabled": 1},
    ],
)
async def test_incomplete_health_does_not_clear_an_expiry_notification(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    notifications: Notifications,
    amazon: object,
) -> None:
    report(backend, account("auth_required"))
    await entry.runtime_data.async_refresh()
    report(backend, amazon)
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert len(notifications.current) == 1
    assert notifications.updates == [pn.UpdateType.ADDED]


async def test_backend_outage_retains_notification_and_does_not_start_reauth(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    notifications: Notifications,
) -> None:
    report(backend, account("auth_required"))
    await entry.runtime_data.async_refresh()
    backend.responses[("GET", "/v1/system")] = (503, b"PRIVATE-PROVIDER-ERROR", {})
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert len(notifications.current) == 1
    assert notifications.updates == [pn.UpdateType.ADDED]
    assert not hass.config_entries.flow.async_progress_by_handler("shipments")


@pytest.mark.parametrize("state", ["connected", "disconnected"])
async def test_reconnection_or_explicit_disconnect_clears_only_owned_notification(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    notifications: Notifications,
    state: str,
) -> None:
    pn.async_create(hass, "Unrelated notification", notification_id="unrelated")
    report(backend, account("auth_required"))
    await entry.runtime_data.async_refresh()
    report(backend, account(state))
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert list(notifications.current) == ["unrelated"]
    assert notifications.updates.count(pn.UpdateType.REMOVED) == 1
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert notifications.updates.count(pn.UpdateType.REMOVED) == 1


async def test_entry_reload_restores_one_stable_notice_and_removal_cleans_it(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    notifications: Notifications,
) -> None:
    report(backend, account("auth_required"))
    await entry.runtime_data.async_refresh()
    key = f"shipments_{entry.entry_id}_amazon_auth_required"
    assert list(notifications.current) == [key]
    assert await hass.config_entries.async_reload(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert list(notifications.current) == [key]
    assert notifications.updates == [pn.UpdateType.ADDED, pn.UpdateType.UPDATED]
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert len(notifications.updates) == 2
    assert await hass.config_entries.async_remove(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert notifications.current == {}


async def test_dismissed_notice_stays_dismissed_until_a_new_auth_failure(
    hass: HomeAssistant,
    entry: MockConfigEntry,
    backend: Backend,
    notifications: Notifications,
) -> None:
    report(backend, account("auth_required"))
    await entry.runtime_data.async_refresh()
    key = f"shipments_{entry.entry_id}_amazon_auth_required"
    pn.async_dismiss(hass, key)
    await entry.runtime_data.async_refresh()
    assert notifications.current == {}
    report(backend, account("connected"))
    await entry.runtime_data.async_refresh()
    report(backend, account("auth_required"))
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert list(notifications.current) == [key]
    assert notifications.updates.count(pn.UpdateType.ADDED) == 2
