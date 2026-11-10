"""Actual native pickup projections keep physical details outside the recorder."""

import json
from datetime import UTC, date, datetime

import pytest
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers import issue_registry as ir

from custom_components.packages.const import DOMAIN
from custom_components.packages.pickups import PickupReport

from .conftest import Backend, get_state


def report(uncertain: int = 1) -> dict[str, object]:
    return {
        "scheduled": 1,
        "uncertain": uncertain,
        "windows_truncated": False,
        "windows": [
            {
                "id": "00000000-0000-4000-8000-000000000001",
                "carrier": "ups",
                "date": "2027-02-02",
                "ready_time": "09:00",
                "close_time": "18:00",
                "timezone": "America/Los_Angeles",
            }
        ],
    }


async def test_native_pickup_counts_calendar_and_repair_follow_authoritative_projection(
    hass: HomeAssistant, entry, backend: Backend
) -> None:
    backend.responses[("GET", "/v1/pickups/status")] = (
        200,
        json.dumps(report()).encode(),
        {"Content-Type": "application/json"},
    )
    await entry.runtime_data.async_refresh()
    assert get_state(hass, "sensor.packages_pickups_scheduled").state == "1"
    assert get_state(hass, "sensor.packages_pickups_uncertain").state == "1"
    assert (
        ir.async_get(hass).async_get_issue(DOMAIN, f"pickup_uncertain_{entry.entry_id}")
        is not None
    )
    calendar = hass.data["calendar"].get_entity("calendar.packages_pickups")
    assert calendar is not None
    events = await calendar.async_get_events(
        hass, datetime(2027, 2, 2, tzinfo=UTC), datetime(2027, 2, 4, tzinfo=UTC)
    )
    assert len(events) == 1
    assert events[0].summary == "UPS pickup"
    assert events[0].start.isoformat() == "2027-02-02T09:00:00-08:00"
    assert calendar.supported_features == 0
    for key in ("sensor.packages_pickups_scheduled", "calendar.packages_pickups"):
        attributes = get_state(hass, key).attributes
        assert not {"address", "tracking", "confirmation", "phone"} & attributes.keys()
    incomplete = {**report(), "windows_truncated": True}
    backend.responses[("GET", "/v1/pickups/status")] = (
        200,
        json.dumps(incomplete).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    assert get_state(hass, "calendar.packages_pickups").state == "unavailable"
    with pytest.raises(HomeAssistantError, match="coverage"):
        await calendar.async_get_events(
            hass, datetime(2027, 2, 2, tzinfo=UTC), datetime(2027, 2, 4, tzinfo=UTC)
        )
    backend.responses[("GET", "/v1/pickups/status")] = (
        200,
        json.dumps(report(0)).encode(),
        {},
    )
    await entry.runtime_data.async_refresh()
    assert (
        ir.async_get(hass).async_get_issue(DOMAIN, f"pickup_uncertain_{entry.entry_id}")
        is None
    )
    backend.responses[("GET", "/v1/pickups/status")] = (503, b"{}", {})
    await entry.runtime_data.async_refresh()
    assert get_state(hass, "sensor.packages_pickups_scheduled").state == "unavailable"
    assert get_state(hass, "calendar.packages_pickups").state == "unavailable"


@pytest.mark.parametrize(
    "invalid",
    [
        None,
        {},
        {**report(), "scheduled": True},
        {**report(), "uncertain": -1},
        {**report(), "windows": [{"carrier": "imaginary"}]},
    ],
)
def test_pickup_projection_does_not_turn_missing_or_invalid_data_into_zero(
    invalid: object,
) -> None:
    with pytest.raises(ValueError):
        PickupReport.parse(invalid)


def test_usps_normal_route_pickup_is_an_all_day_event_not_an_invented_time_window() -> (
    None
):
    data = report()
    data["windows"] = [
        {
            "id": "00000000-0000-4000-8000-000000000001",
            "carrier": "usps",
            "date": "2027-02-02",
            "ready_time": "09:00",
            "close_time": "18:00",
            "timezone": "America/Los_Angeles",
        }
    ]
    event = PickupReport.parse(data).windows[0].calendar_event()
    assert event.start == date(2027, 2, 2)
    assert event.end == date(2027, 2, 3)
    assert event.description is not None
    assert "no pickup time" in event.description
