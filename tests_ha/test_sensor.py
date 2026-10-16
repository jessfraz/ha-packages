"""Verify native entities follow coordinated backend state and unload."""

import json

import pytest
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryAuthFailed

from custom_components.shipments.const import DOMAIN, SENSOR_KEYS

from .conftest import SUMMARY, Backend, get_state


async def test_summary_entities_and_unload(hass: HomeAssistant, entry) -> None:
    states = hass.states.async_all("sensor")
    assert len(states) == len(SENSOR_KEYS)
    assert {state.entity_id: state.state for state in states} == {
        **{f"sensor.shipments_{key}": str(value) for key, value in SUMMARY.items()},
        "sensor.shipments_inbound_delivered_today": "1",
        "sensor.shipments_inbound_active": "3",
        "sensor.shipments_inbound_out_for_delivery": "1",
        "sensor.shipments_inbound_exceptions": "0",
        "sensor.shipments_outbound_active": "1",
        "sensor.shipments_outbound_exceptions": "0",
    }
    assert hass.services.has_service(DOMAIN, "archive")
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert all(
        state.state == "unavailable" for state in hass.states.async_all("sensor")
    )
    assert hass.services.has_service(DOMAIN, "archive")
    # Shared HA session survives unloading and the same view can serve a reload.
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    assert len(hass.states.async_all("sensor")) == len(SENSOR_KEYS)


async def test_failed_summary_marks_entities_unavailable(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    backend.responses[("GET", "/v1/summary")] = (503, b"{}", {})
    await entry.runtime_data.async_refresh()
    assert all(
        state.state == "unavailable" for state in hass.states.async_all("sensor")
    )
    del backend.responses[("GET", "/v1/summary")]
    await entry.runtime_data.async_refresh()
    assert get_state(hass, "sensor.shipments_active").state == "4"


async def test_direction_counts_keep_existing_sensors_combined(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    backend.summaries[""].update(active=7, exceptions=3)
    backend.summaries["inbound"].update(active=4, exceptions=1)
    backend.summaries["outbound"].update(active=3, exceptions=2)
    backend.requests.clear()
    await entry.runtime_data.async_refresh()
    assert {
        state.entity_id: state.state for state in hass.states.async_all("sensor")
    } == {
        "sensor.shipments_active": "7",
        "sensor.shipments_expected": "1",
        "sensor.shipments_in_transit": "2",
        "sensor.shipments_out_for_delivery": "1",
        "sensor.shipments_delivered": "3",
        "sensor.shipments_exceptions": "3",
        "sensor.shipments_sync_errors": "1",
        "sensor.shipments_inbound_delivered_today": "1",
        "sensor.shipments_inbound_active": "4",
        "sensor.shipments_inbound_out_for_delivery": "1",
        "sensor.shipments_inbound_exceptions": "1",
        "sensor.shipments_outbound_active": "3",
        "sensor.shipments_outbound_exceptions": "2",
    }
    assert [
        (request.path, request.query)
        for request in backend.requests
        if request.path == "/v1/summary" and "delivered_after" not in request.query
    ] == [
        ("/v1/summary", {}),
        ("/v1/summary", {"direction": "inbound"}),
        ("/v1/summary", {"direction": "outbound"}),
    ]
    assert all(
        request.headers["Authorization"] == "Bearer test-owner-token"
        for request in backend.requests
    )


@pytest.mark.parametrize("direction", ["inbound", "outbound"])
@pytest.mark.parametrize(
    "failure", ["unavailable", "auth", "negative", "bool", "missing"]
)
async def test_direction_summary_failure_keeps_the_last_complete_snapshot(
    hass: HomeAssistant, entry, backend: Backend, direction: str, failure: str
) -> None:
    previous = entry.runtime_data.data.copy()
    backend.summaries[""].update(active=100)
    path = f"/v1/summary?direction={direction}"
    if failure == "unavailable":
        response = (503, b"{}", {})
    elif failure == "auth":
        response = (401, b"{}", {})
    else:
        summary = backend.summaries[direction].copy()
        if failure == "negative":
            summary["active"] = -1
        elif failure == "bool":
            summary["active"] = True
        else:
            del summary["exceptions"]
        response = (200, json.dumps(summary).encode(), {})
    backend.responses[("GET", path)] = response
    await entry.runtime_data.async_refresh()
    assert not entry.runtime_data.last_update_success
    assert entry.runtime_data.data == previous
    assert all(
        state.state == "unavailable" for state in hass.states.async_all("sensor")
    )
    if failure == "auth":
        assert isinstance(entry.runtime_data.last_exception, ConfigEntryAuthFailed)
    del backend.responses[("GET", path)]
    await entry.runtime_data.async_refresh()
    assert entry.runtime_data.last_update_success
    assert get_state(hass, "sensor.shipments_active").state == "100"
    assert get_state(hass, "sensor.shipments_inbound_active").state == "3"
    assert get_state(hass, "sensor.shipments_outbound_active").state == "1"
