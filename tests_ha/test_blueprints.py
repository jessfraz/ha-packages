"""Run distributed blueprints through HA's real automation and device actions."""

import json
import sys
from collections.abc import AsyncIterator
from dataclasses import dataclass
from datetime import timedelta
from pathlib import Path
from types import ModuleType

import pytest
from freezegun.api import FrozenDateTimeFactory
from homeassistant.components.automation.config import AUTOMATION_BLUEPRINT_SCHEMA
from homeassistant.components.blueprint import Blueprint, BlueprintInputs
from homeassistant.core import HomeAssistant, ServiceCall
from homeassistant.helpers import device_registry as dr
from homeassistant.setup import async_setup_component
from homeassistant.util import dt as dt_util
from homeassistant.util.yaml import load_yaml_dict
from pytest_homeassistant_custom_component.common import (
    MockConfigEntry,
    async_fire_time_changed,
    async_mock_service,
)

BLUEPRINTS = Path(__file__).resolve().parents[1] / "blueprints/automation/shipments"
MOBILE_APP = Path(dr.__file__).resolve().parents[1] / "components/mobile_app"
EVENTS = "event.shipments"
ERRORS = "sensor.shipments_sync_errors"
ARRIVING = "sensor.shipments_inbound_out_for_delivery"
DELIVERED = "sensor.shipments_inbound_delivered_today"
SHIPMENT_ID = "0193c69f-e5c4-7e93-b6cb-8172dacb617e"
OTHER_ID = "0193c69f-e5c4-7e93-b6cb-8172dacb617f"
PRIVATE = "PRIVATE-TRACKING-NUMBER"


@dataclass
class Phone:
    """Native device-action target with a captured notification transport."""

    device_id: str
    calls: list[ServiceCall]


@dataclass
class NotificationTargets:
    """The transport registration consumed by the native mobile device action."""

    registered_targets: dict[str, str]


@pytest.fixture
async def phone(
    hass: HomeAssistant,
    monkeypatch: pytest.MonkeyPatch,
    freezer: FrozenDateTimeFactory,
) -> AsyncIterator[Phone]:
    # Isolate unrelated startup imports (cloud, camera, mobile sensors), while
    # importing the installed device_action.py, util.py and const.py normally.
    # This keeps actual action validation, rendering and notify dispatch intact.
    package = ModuleType("homeassistant.components.mobile_app")
    package.__dict__["__path__"] = [str(MOBILE_APP)]
    monkeypatch.setitem(sys.modules, package.__name__, package)
    monkeypatch.setitem(
        sys.modules,
        "homeassistant.components.cloud",
        ModuleType("homeassistant.components.cloud"),
    )
    entry = MockConfigEntry(domain="mobile_app")
    entry.add_to_hass(hass)
    device = dr.async_get(hass).async_get_or_create(
        config_entry_id=entry.entry_id,
        identifiers={("mobile_app", "blueprint-phone")},
    )
    hass.data["mobile_app"] = {
        "devices": {"fixture-webhook": device},
        "notify": NotificationTargets({"blueprint_phone": "fixture-webhook"}),
    }
    yield Phone(device.id, async_mock_service(hass, "notify", "blueprint_phone"))
    if automations := hass.states.async_entity_ids("automation"):
        await hass.services.async_call(
            "automation", "turn_off", {"entity_id": automations}, blocking=True
        )
        await hass.async_block_till_done()


async def setup_blueprint(
    hass: HomeAssistant,
    phone: Phone,
    name: str,
    inputs: dict[str, object] | None = None,
) -> None:
    """Validate !input YAML and execute its real substituted automation."""
    data = await hass.async_add_executor_job(
        load_yaml_dict, BLUEPRINTS / f"{name}.yaml"
    )
    blueprint = Blueprint(
        data,
        path=f"shipments/{name}.yaml",
        expected_domain="automation",
        schema=AUTOMATION_BLUEPRINT_SCHEMA,
    )
    assert blueprint.validate() is None
    instance = BlueprintInputs(
        blueprint,
        {
            "id": f"blueprint_{name}",
            "alias": f"Blueprint {name}",
            "use_blueprint": {
                "path": f"shipments/{name}.yaml",
                "input": {"notify_device": phone.device_id, **(inputs or {})},
            },
        },
    )
    instance.validate()
    assert await async_setup_component(
        hass, "automation", {"automation": [instance.async_substitute()]}
    )
    await hass.async_start()
    await hass.async_block_till_done()
    state = hass.states.get(f"automation.blueprint_{name}")
    assert state is not None and state.state == "on"


def shipment_event(
    event_id: int,
    *,
    title: str = "Office supplies: keep MY title!",
    direction: str = "inbound",
    event_type: str = "out_for_delivery",
    shipment_id: str = SHIPMENT_ID,
) -> dict[str, object]:
    """Build an event entity's public state, plus data that must stay private."""
    return {
        "event_id": event_id,
        "shipment_id": shipment_id,
        "title": title,
        "carrier": "USPS",
        "direction": direction,
        "event_type": event_type,
        "status": event_type,
        "at": "2026-10-15T15:30:00+00:00",
        "tracking_number": PRIVATE,
        "sources": [PRIVATE],
    }


def emit(hass: HomeAssistant, event_id: int, **kwargs: str) -> None:
    hass.states.async_set(
        EVENTS,
        f"2026-10-15T15:30:{event_id:02d}+00:00",
        shipment_event(event_id, **kwargs),
    )


async def advance(
    hass: HomeAssistant, freezer: FrozenDateTimeFactory, seconds: int
) -> None:
    freezer.tick(timedelta(seconds=seconds))
    async_fire_time_changed(hass, dt_util.utcnow())
    await hass.async_block_till_done()


@pytest.mark.parametrize(
    ("selected_direction", "event_direction", "event_type", "expected"),
    [
        ("inbound", "inbound", "out_for_delivery", True),
        ("inbound", "outbound", "out_for_delivery", False),
        ("outbound", "outbound", "delivered", True),
        ("outbound", "inbound", "delivered", False),
        ("all", "inbound", "exception", True),
        ("all", "outbound", "exception", True),
        ("all", "inbound", "delivered", False),
    ],
)
async def test_delivery_update_filters(
    hass: HomeAssistant,
    phone: Phone,
    selected_direction: str,
    event_direction: str,
    event_type: str,
    expected: bool,
) -> None:
    hass.states.async_set(EVENTS, "unknown")
    await setup_blueprint(
        hass,
        phone,
        "shipment_updates",
        {
            "direction": selected_direction,
            "event_types": ["out_for_delivery", "exception"]
            if selected_direction == "all"
            else [event_type],
        },
    )
    emit(hass, 1, direction=event_direction, event_type=event_type)
    await hass.async_block_till_done()
    assert len(phone.calls) == int(expected)
    if expected:
        assert phone.calls[0].data["target"] == "fixture-webhook"
        assert phone.calls[0].data["title"] == "Office supplies: keep MY title!"
        assert PRIVATE not in json.dumps(dict(phone.calls[0].data))


async def test_delivery_update_snapshots_bursts_and_exact_deep_links(
    hass: HomeAssistant, phone: Phone
) -> None:
    hass.states.async_set(EVENTS, "unknown")
    await setup_blueprint(hass, phone, "shipment_updates")
    # Write both before yielding: the action must use the triggering snapshot,
    # rather than reading the latest event entity's attributes for both calls.
    emit(hass, 1, title="Coffee beans", shipment_id=SHIPMENT_ID)
    emit(hass, 2, title="New view – 50 cards", shipment_id=OTHER_ID)
    await hass.async_block_till_done()
    assert [call.data["title"] for call in phone.calls] == [
        "Coffee beans",
        "New view – 50 cards",
    ]
    for call, shipment_id in zip(phone.calls, [SHIPMENT_ID, OTHER_ID], strict=True):
        assert call.data["data"] == {
            "url": f"/shipments/shipments/shipment%3D{shipment_id}",
            "clickAction": f"/shipments/shipments/shipment%3D{shipment_id}",
            "tag": f"shipments-{shipment_id}",
            "group": "shipments",
        }


async def test_delivery_updates_ignore_restore_duplicates_and_recovery_replays(
    hass: HomeAssistant, phone: Phone
) -> None:
    await setup_blueprint(hass, phone, "shipment_updates")
    emit(hass, 1)  # No prior state: a restored entity must not send old news.
    await hass.async_block_till_done()
    assert not phone.calls
    hass.states.async_set(EVENTS, "unavailable", shipment_event(1))
    emit(hass, 1)  # Same durable event, restored after a reconnect.
    await hass.async_block_till_done()
    assert not phone.calls
    hass.states.async_set(EVENTS, "unavailable", shipment_event(1))
    emit(hass, 2)  # A genuinely new event received during recovery.
    await hass.async_block_till_done()
    assert len(phone.calls) == 1
    hass.states.async_set(EVENTS, "2026-10-15T15:31:00+00:00", shipment_event(2))
    await hass.async_block_till_done()
    assert len(phone.calls) == 1


@pytest.mark.parametrize("initial", [None, "unknown", "unavailable", "3"])
async def test_tracking_problems_do_not_alert_on_initial_or_restored_errors(
    hass: HomeAssistant,
    phone: Phone,
    freezer: FrozenDateTimeFactory,
    initial: str | None,
) -> None:
    if initial is not None:
        hass.states.async_set(ERRORS, initial)
    await setup_blueprint(hass, phone, "tracking_issues")
    hass.states.async_set(ERRORS, "4")
    await hass.async_block_till_done()
    await advance(hass, freezer, 301)
    assert not phone.calls


async def test_tracking_problems_debounce_clear_and_rearm(
    hass: HomeAssistant, phone: Phone, freezer: FrozenDateTimeFactory
) -> None:
    hass.states.async_set(ERRORS, "0")
    await setup_blueprint(hass, phone, "tracking_issues")
    hass.states.async_set(ERRORS, "1")
    await hass.async_block_till_done()
    await advance(hass, freezer, 299)
    assert not phone.calls
    hass.states.async_set(ERRORS, "0")
    await hass.async_block_till_done()
    await advance(hass, freezer, 2)
    assert not phone.calls
    hass.states.async_set(ERRORS, "2")
    await hass.async_block_till_done()
    await advance(hass, freezer, 301)
    assert len(phone.calls) == 1
    assert "2 shipments have" in phone.calls[0].data["message"]
    assert phone.calls[0].data["data"]["url"] == (
        "/shipments/shipments/view%3Dtracking_issues"
    )
    hass.states.async_set(ERRORS, "3")
    await hass.async_block_till_done()
    await advance(hass, freezer, 301)
    assert len(phone.calls) == 1
    hass.states.async_set(ERRORS, "0")
    await hass.async_block_till_done()
    assert len(phone.calls) == 1
    hass.states.async_set(ERRORS, "1")
    await hass.async_block_till_done()
    await advance(hass, freezer, 301)
    assert len(phone.calls) == 2
    assert "1 shipment has" in phone.calls[1].data["message"]


@pytest.mark.parametrize("recovered", ["0", "2"])
async def test_tracking_problem_wait_is_cancelled_when_unavailable(
    hass: HomeAssistant,
    phone: Phone,
    freezer: FrozenDateTimeFactory,
    recovered: str,
) -> None:
    hass.states.async_set(ERRORS, "0")
    await setup_blueprint(hass, phone, "tracking_issues")
    hass.states.async_set(ERRORS, "1")
    await hass.async_block_till_done()
    await advance(hass, freezer, 200)
    hass.states.async_set(ERRORS, "unavailable")
    await hass.async_block_till_done()
    hass.states.async_set(ERRORS, recovered)
    await hass.async_block_till_done()
    await advance(hass, freezer, 301)
    assert not phone.calls


@pytest.mark.parametrize(
    ("arriving", "delivered"),
    [("0", "0"), ("unavailable", "2"), ("1", "unavailable"), ("unknown", "1")],
)
async def test_daily_summary_is_quiet_for_empty_or_unavailable_counts(
    hass: HomeAssistant,
    phone: Phone,
    freezer: FrozenDateTimeFactory,
    arriving: str,
    delivered: str,
) -> None:
    await hass.config.async_set_time_zone("America/Los_Angeles")
    freezer.move_to("2026-10-15T14:59:00+00:00")
    hass.states.async_set(ARRIVING, arriving, {"items": []})
    hass.states.async_set(DELIVERED, delivered, {"items": []})
    await setup_blueprint(hass, phone, "delivery_summary")
    await advance(hass, freezer, 61)
    assert not phone.calls


async def test_daily_summary_local_time_titles_truncation_and_privacy(
    hass: HomeAssistant, phone: Phone, freezer: FrozenDateTimeFactory
) -> None:
    await hass.config.async_set_time_zone("America/Los_Angeles")
    freezer.move_to("2026-10-15T14:59:00+00:00")
    items = [
        {
            "id": str(index),
            "title": f"Keep MY title: package {index}!",
            "carrier": "USPS",
            "status": "out_for_delivery",
            "tracking_number": PRIVATE,
            "url": f"/shipments/shipments/shipment%3D{index}",
        }
        for index in range(20)
    ]
    hass.states.async_set(ARRIVING, "25", {"items": items, "items_truncated": True})
    hass.states.async_set(
        DELIVERED,
        "1",
        {"items": [{"title": "New view – 50 cards", "carrier": "FedEx"}]},
    )
    await setup_blueprint(hass, phone, "delivery_summary")
    await advance(hass, freezer, 59)
    assert not phone.calls
    await advance(hass, freezer, 2)
    assert len(phone.calls) == 1
    call = phone.calls[0]
    message = call.data["message"]
    assert message == (
        "Out for delivery: 25\n"
        "• Keep MY title: package 0! (USPS)\n"
        "• Keep MY title: package 1! (USPS)\n"
        "• Keep MY title: package 2! (USPS)\n"
        "• Keep MY title: package 3! (USPS)\n"
        "• Keep MY title: package 4! (USPS)\n"
        "+ 20 more in Shipments\n"
        "\n"
        "Delivered today: 1\n"
        "• New view – 50 cards (FedEx)"
    )
    assert PRIVATE not in json.dumps(dict(call.data))
    assert call.data["data"]["url"] == (
        "/shipments/shipments/view%3Dall%26direction%3Dinbound"
    )


async def test_daily_summary_uses_configured_time_and_renamed_sensors(
    hass: HomeAssistant, phone: Phone, freezer: FrozenDateTimeFactory
) -> None:
    await hass.config.async_set_time_zone("America/Los_Angeles")
    freezer.move_to("2026-10-15T17:29:00+00:00")
    hass.states.async_set("sensor.my_deliveries", "1", {"items": []})
    hass.states.async_set("sensor.my_arrivals", "0", {"items": []})
    await setup_blueprint(
        hass,
        phone,
        "delivery_summary",
        {
            "out_for_delivery": "sensor.my_deliveries",
            "delivered_today": "sensor.my_arrivals",
            "notification_time": "10:30:00",
        },
    )
    await advance(hass, freezer, 61)
    assert len(phone.calls) == 1
    assert "Out for delivery: 1" in phone.calls[0].data["message"]
    assert "+ 1 more in Shipments" in phone.calls[0].data["message"]
