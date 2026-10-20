"""Native event delivery, calendar windows, diagnostics and panel lifecycle."""

import asyncio
import json
from unittest.mock import AsyncMock

import pytest
from homeassistant.components import frontend
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import Event, EventStateChangedData, HomeAssistant, callback
from homeassistant.helpers import device_registry as dr
from homeassistant.helpers import issue_registry as ir
from homeassistant.helpers.entity_platform import AddConfigEntryEntitiesCallback
from homeassistant.helpers.event import async_track_state_change_event
from homeassistant.helpers.storage import Store
from pytest_homeassistant_custom_component.common import MockConfigEntry, MockUser

from custom_components.shipments import sensor
from custom_components.shipments.const import (
    CONF_ADDON_SLUG,
    CONF_BACKEND_URL,
    CONF_TOKEN,
    DOMAIN,
    SUMMARY_KEYS,
)
from custom_components.shipments.coordinator import ShipmentsCoordinator

from .conftest import SHIPMENT_ID, Backend, get_state


def status_event(
    event_id: int, event_type: str = "out_for_delivery"
) -> dict[str, object]:
    return {
        "id": event_id,
        "shipment_id": SHIPMENT_ID,
        "event_type": event_type,
        "status": "failure" if event_type == "exception" else event_type,
        "title": "Office supplies",
        "carrier": "USPS",
        "direction": "inbound",
        "at": "2026-10-15T10:00:00Z",
    }


def feed_page(
    backend: Backend,
    after: int,
    events: list[dict[str, object]],
    *,
    more: bool = False,
) -> None:
    backend.responses[("GET", f"/v1/events?limit=200&after={after}")] = (
        200,
        json.dumps(
            {
                "items": events,
                "next_after": events[-1]["id"] if events else after,
                "has_more": more,
            }
        ).encode(),
        {},
    )


def capture_events(hass: HomeAssistant) -> list[int]:
    delivered: list[int] = []

    @callback
    def changed(event: Event[EventStateChangedData]) -> None:
        state = event.data["new_state"]
        if state and state.state != "unavailable" and "event_id" in state.attributes:
            delivered.append(state.attributes["event_id"])

    async_track_state_change_event(hass, ["event.shipments"], changed)
    return delivered


async def test_count_sensors_expose_measurement_statistics(
    hass: HomeAssistant, entry
) -> None:
    assert get_state(hass, "sensor.shipments_active").state == "4"
    for key in SUMMARY_KEYS:
        assert (
            get_state(hass, f"sensor.shipments_{key}").attributes.get("state_class")
            == "measurement"
        )


async def test_native_ids_are_stable_when_sensor_platform_finishes_last(
    hass: HomeAssistant, backend: Backend, monkeypatch
) -> None:
    """Every platform must establish the same device before naming its entities."""
    sensor_release = asyncio.Event()
    native_platforms_added = asyncio.Event()
    added_domains: set[str] = set()
    original_setup = sensor.async_setup_entry

    async def delayed_sensor_setup(
        hass: HomeAssistant,
        entry: ConfigEntry[ShipmentsCoordinator],
        async_add_entities: AddConfigEntryEntitiesCallback,
    ) -> None:
        await sensor_release.wait()
        await original_setup(hass, entry, async_add_entities)

    @callback
    def state_changed(event: Event) -> None:
        domain = event.data["entity_id"].partition(".")[0]
        if event.data["new_state"] is not None and domain in {
            "event",
            "calendar",
            "binary_sensor",
        }:
            added_domains.add(domain)
            if len(added_domains) == 3:
                native_platforms_added.set()

    unsubscribe = hass.bus.async_listen("state_changed", state_changed)
    monkeypatch.setattr(sensor, "async_setup_entry", delayed_sensor_setup)
    entry = MockConfigEntry(
        domain=DOMAIN,
        title="Mock Title",
        data={CONF_BACKEND_URL: backend.url, CONF_TOKEN: "integration-token"},
    )
    entry.add_to_hass(hass)
    setup = hass.async_create_task(hass.config_entries.async_setup(entry.entry_id))
    try:
        async with asyncio.timeout(5):
            await native_platforms_added.wait()
        assert hass.states.get("event.shipments") is not None
        assert hass.states.get("calendar.shipments_inbound_deliveries") is not None
        assert hass.states.get("binary_sensor.shipments_exceptions") is not None
        assert hass.states.get("binary_sensor.shipments_sync_errors") is not None
        device = dr.async_get(hass).async_get_device_by_identifier(
            (DOMAIN, entry.entry_id), entry.entry_id
        )
        assert device is not None
        assert device.name == "Shipments"
        assert device.manufacturer == "Shipments"
        assert device.model == "Shipment registry"
        assert device.configuration_url == backend.url
    finally:
        sensor_release.set()
        unsubscribe()
        assert await setup
        await hass.async_block_till_done(wait_background_tasks=True)
    assert len(hass.states.async_all("event")) == 1
    assert get_state(hass, "sensor.shipments_active").state == "4"


async def test_native_problem_indicators_and_label(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    assert (
        get_state(hass, "sensor.shipments_expected").attributes["friendly_name"]
        == "Shipments Awaiting pickup"
    )
    assert get_state(hass, "binary_sensor.shipments_exceptions").state == "off"
    assert get_state(hass, "binary_sensor.shipments_sync_errors").state == "on"
    backend.summaries[""].update(exceptions=2, sync_errors=0)
    await entry.runtime_data.async_refresh()
    assert get_state(hass, "binary_sensor.shipments_exceptions").state == "on"
    assert get_state(hass, "binary_sensor.shipments_sync_errors").state == "off"
    assert (
        get_state(hass, "binary_sensor.shipments_exceptions").attributes["device_class"]
        == "problem"
    )


async def test_event_pages_emit_each_persisted_transition_and_resume_after_reload(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    delivered = capture_events(hass)
    feed_page(backend, 0, [status_event(1), status_event(2, "delivered")], more=True)
    feed_page(backend, 2, [status_event(3, "exception")])
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert delivered == [1, 2, 3]
    attributes = get_state(hass, "event.shipments").attributes
    assert {
        key: attributes[key]
        for key in (
            "event_type",
            "event_id",
            "shipment_id",
            "title",
            "carrier",
            "direction",
            "status",
        )
    } == {
        "event_type": "exception",
        "event_id": 3,
        "shipment_id": SHIPMENT_ID,
        "title": "Office supplies",
        "carrier": "USPS",
        "direction": "inbound",
        "status": "failure",
    }
    checkpoint = Store(hass, 1, f"shipments.{entry.entry_id}.events")
    assert await checkpoint.async_load() == {"after": 3}
    assert await hass.config_entries.async_unload(entry.entry_id)
    feed_page(
        backend,
        3,
        [status_event(4, "delivered"), status_event(5), status_event(6, "exception")],
    )
    backend.requests.clear()
    backend.event_read_started.clear()
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert delivered[-3:] == [4, 5, 6]
    assert get_state(hass, "event.shipments").attributes.get("event_id") == 6
    assert await Store(hass, 1, f"shipments.{entry.entry_id}.events").async_load() == {
        "after": 6
    }
    queries = [
        request.query for request in backend.requests if request.path == "/v1/events"
    ]
    assert queries == [{"limit": "200", "after": "3"}]
    assert get_state(hass, "event.shipments").attributes["event_id"] == 6


async def test_feed_failure_does_not_reset_cursor_or_break_counts_calendar(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    delivered = capture_events(hass)
    backend.responses[("GET", "/v1/events?limit=200&after=0")] = (503, b"{}", {})
    await entry.runtime_data.async_refresh()
    assert get_state(hass, "event.shipments").state == "unavailable"
    assert get_state(hass, "sensor.shipments_active").state == "4"
    assert (
        get_state(hass, "calendar.shipments_inbound_deliveries").state != "unavailable"
    )
    assert ir.async_get(hass).async_get_issue(DOMAIN, "event_stream_unavailable")
    feed_page(backend, 0, [status_event(1)])
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert delivered == [1]
    assert not ir.async_get(hass).async_get_issue(DOMAIN, "event_stream_unavailable")


async def test_summary_recovery_delivers_each_queued_native_event(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    delivered = capture_events(hass)
    backend.responses[("GET", "/v1/summary")] = (503, b"{}", {})
    await entry.runtime_data.async_refresh()
    assert get_state(hass, "sensor.shipments_active").state == "unavailable"
    assert get_state(hass, "event.shipments").state == "unavailable"
    del backend.responses[("GET", "/v1/summary")]
    feed_page(backend, 0, [status_event(1), status_event(2, "delivered")], more=True)
    feed_page(backend, 2, [status_event(3, "exception")])
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert delivered == [1, 2, 3]


async def test_saved_cursor_ahead_of_restored_history_warns_and_rebaselines_tail(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    delivered = capture_events(hass)
    backend.responses[("GET", "/v1/events?limit=200&after=0")] = (409, b"{}", {})
    backend.responses[("GET", "/v1/events?limit=200")] = (
        200,
        b'{"items":[],"next_after":12,"has_more":false}',
        {},
    )
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert delivered == []
    assert ir.async_get(hass).async_get_issue(DOMAIN, "event_history_reset")
    checkpoint = Store(hass, 1, f"shipments.{entry.entry_id}.events")
    assert await checkpoint.async_load() == {"after": 12}
    feed_page(backend, 12, [status_event(13)])
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert delivered == [13]


async def test_failed_checkpoint_readback_replays_delivered_event(
    hass: HomeAssistant, entry, backend: Backend, monkeypatch
) -> None:
    delivered = capture_events(hass)
    feed_page(backend, 0, [status_event(1)])
    # Core's Store deliberately logs and swallows write errors. Simulate that
    # boundary behavior while retaining the actual persisted old checkpoint.
    store = entry.runtime_data._event_store
    original_save = store.async_save
    monkeypatch.setattr(store, "async_save", AsyncMock(return_value=None))
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert delivered == [1]
    assert get_state(hass, "event.shipments").state == "unavailable"
    monkeypatch.setattr(store, "async_save", original_save)
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    # Replay is deliberate: event_id is the public deduplication key.
    assert delivered == [1, 1]
    assert await store.async_load() == {"after": 1}


@pytest.mark.parametrize("problem", ["unordered", "cursor_gap", "invalid_type"])
async def test_invalid_event_page_never_checkpoints_past_unseen_data(
    hass: HomeAssistant, entry, backend: Backend, problem: str
) -> None:
    records = (
        [status_event(2), status_event(1)]
        if problem == "unordered"
        else [status_event(1)]
    )
    if problem == "invalid_type":
        records[0]["event_type"] = "arbitrary"
    backend.responses[("GET", "/v1/events?limit=200&after=0")] = (
        200,
        json.dumps(
            {
                "items": records,
                "next_after": 10 if problem == "cursor_gap" else 1,
                "has_more": False,
            }
        ).encode(),
        {},
    )
    delivered = capture_events(hass)
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert delivered == []
    assert await Store(hass, 1, f"shipments.{entry.entry_id}.events").async_load() == {
        "after": 0
    }
    assert get_state(hass, "sensor.shipments_active").state == "4"


async def test_unload_during_event_read_does_not_deliver_or_advance_checkpoint(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    feed_page(backend, 0, [status_event(1)])
    delivered = capture_events(hass)
    backend.requests.clear()
    backend.event_read_started.clear()
    backend.delay_seconds = 0.01
    entry.async_create_background_task(
        hass, entry.runtime_data.async_refresh(), "test event read"
    )
    async with asyncio.timeout(2):
        await backend.event_read_started.wait()
    assert await hass.config_entries.async_unload(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    assert delivered == []
    assert await Store(hass, 1, f"shipments.{entry.entry_id}.events").async_load() == {
        "after": 0
    }


async def test_calendar_follows_pages_and_returns_only_intersecting_estimates(
    hass: HomeAssistant, hass_client, entry, backend: Backend
) -> None:
    def shipment(identity: str, title: str, eta: str | None) -> dict[str, object]:
        return {
            "id": identity,
            "title": title,
            "status": "unknown",
            "carrier": "USPS",
            "direction": "inbound",
            "estimated_delivery_at": eta,
        }

    first_query = "/v1/delivery-estimates?limit=200"
    backend.responses[("GET", first_query)] = (
        200,
        json.dumps(
            {
                "items": [shipment(SHIPMENT_ID, "Office supplies", "2026-10-20")],
                "next_page": "next+page",
            }
        ).encode(),
        {},
    )
    backend.responses[("GET", f"{first_query}&page_token=next%2Bpage")] = (
        200,
        json.dumps(
            {
                "items": [
                    shipment(
                        "0193c69f-e5c4-7e93-b6cb-8172dacb617f",
                        "Second parcel",
                        "2026-10-21T10:00:00Z",
                    ),
                    shipment(
                        "0193c69f-e5c4-7e93-b6cb-8172dacb6180", "No estimate", None
                    ),
                    shipment(
                        "0193c69f-e5c4-7e93-b6cb-8172dacb6181",
                        "Invalid estimate",
                        "not-a-date",
                    ),
                ],
                "next_page": None,
            }
        ).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    client = await hass_client()
    response = await client.get(
        "/api/calendars/calendar.shipments_inbound_deliveries",
        params={"start": "2026-10-20T00:00:00Z", "end": "2026-10-22T00:00:00Z"},
    )
    assert response.status == 200
    events = await response.json()
    assert [event["summary"] for event in events] == [
        "Office supplies",
        "Second parcel",
    ]
    assert events[0]["start"] == {"date": "2026-10-20"}
    assert events[0]["end"] == {"date": "2026-10-21"}
    assert events[1]["start"] == {"dateTime": "2026-10-21T03:00:00-07:00"}
    assert events[1]["end"] == {"dateTime": "2026-10-21T04:00:00-07:00"}
    assert "Carrier estimates can change" in events[0]["description"]


@pytest.mark.parametrize(
    ("estimate", "expected"),
    [
        ("2026-03-08", {"date": "2026-03-08"}),
        ("2026-11-01", {"date": "2026-11-01"}),
        ("2026-10-21", {"date": "2026-10-21"}),
        ("2026-10-21T00:00:00Z", {"dateTime": "2026-10-20T17:00:00-07:00"}),
        ("2026-11-02T00:00:00Z", {"dateTime": "2026-11-01T16:00:00-08:00"}),
    ],
)
async def test_calendar_preserves_date_precision_and_genuine_timed_estimates(
    hass: HomeAssistant,
    hass_client,
    entry,
    backend: Backend,
    estimate: str,
    expected: dict[str, str],
) -> None:
    backend.responses[("GET", "/v1/delivery-estimates?limit=200")] = (
        200,
        json.dumps(
            {
                "items": [
                    {
                        "id": SHIPMENT_ID,
                        "title": "Fixture parcel",
                        "carrier": "UPS",
                        "direction": "inbound",
                        "status": "in_transit",
                        "estimated_delivery_at": estimate,
                    }
                ],
                "next_page": None,
            }
        ).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    client = await hass_client()
    response = await client.get(
        "/api/calendars/calendar.shipments_inbound_deliveries",
        params={"start": "2026-01-01T00:00:00Z", "end": "2027-01-01T00:00:00Z"},
    )
    assert response.status == 200
    events = await response.json()
    assert len(events) == 1
    assert events[0]["start"] == expected


async def test_version_mismatch_repairs_clear_when_versions_match(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    current = backend.version
    backend.version = "0.0.1"
    await entry.runtime_data.async_refresh()
    issue = ir.async_get(hass).async_get_issue(DOMAIN, "version_mismatch")
    assert issue is not None
    assert issue.translation_placeholders == {
        "app_version": "0.0.1",
        "integration_version": current,
    }
    backend.version = current
    await entry.runtime_data.async_refresh()
    assert not ir.async_get(hass).async_get_issue(DOMAIN, "version_mismatch")


async def test_old_backend_missing_new_endpoint_still_reports_version_mismatch(
    hass: HomeAssistant, backend: Backend
) -> None:
    integration_version = backend.version
    backend.version = "0.4.0"
    backend.responses[("GET", "/v1/delivery-estimates")] = (404, b"{}", {})
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={CONF_BACKEND_URL: backend.url, CONF_TOKEN: "integration-token"},
    )
    entry.add_to_hass(hass)
    assert not await hass.config_entries.async_setup(entry.entry_id)
    issue = ir.async_get(hass).async_get_issue(DOMAIN, "version_mismatch")
    assert issue is not None
    assert issue.translation_placeholders == {
        "app_version": "0.4.0",
        "integration_version": integration_version,
    }


async def test_supervised_panel_uses_stable_path_actual_slug_and_normal_user_access(
    hass: HomeAssistant, backend: Backend, hass_admin_user, hass_user
) -> None:
    MockUser(is_active=False, groups=hass_admin_user.groups).add_to_hass(hass)
    backend.responses[("POST", "/v1/integration/presence")] = (204, b"", {})
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: backend.url,
            CONF_TOKEN: "integration-token",
            CONF_ADDON_SLUG: "actualprefix_shipments",
        },
    )
    entry.add_to_hass(hass)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done(wait_background_tasks=True)
    panel = hass.data[frontend.DATA_PANELS]["shipments"]
    assert panel.component_name == "app"
    assert panel.config == {"addon": "actualprefix_shipments"}
    assert panel.require_admin is False
    presence = next(
        request
        for request in backend.requests
        if request.path == "/v1/integration/presence"
    )
    assert json.loads(presence.body)["administrator_ids"] == [hass_admin_user.id]
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert "shipments" not in hass.data[frontend.DATA_PANELS]


async def test_removing_entry_deletes_only_its_checkpoint_and_repairs(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    other = Store(hass, 1, "shipments.other.events")
    await other.async_save({"after": 99})
    backend.version = "0.0.1"
    await entry.runtime_data.async_refresh()
    assert ir.async_get(hass).async_get_issue(DOMAIN, "version_mismatch")
    assert await hass.config_entries.async_remove(entry.entry_id)
    assert (
        await Store(hass, 1, f"shipments.{entry.entry_id}.events").async_load() is None
    )
    assert await Store(hass, 1, "shipments.other.events").async_load() == {"after": 99}
    assert not ir.async_get(hass).async_get_issue(DOMAIN, "version_mismatch")
